//! Helper functions for interacting with Consumer VMs.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::ops::Bound::{Excluded, Included};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use crate::{EphAllocation, LogLevel, Vm, VmList, donor, log, qmp};
use eph_proto::{ConsumerCommand, ConsumerFunction};
use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

pub struct ConsumerState {
    /* Back-pointer to the Vm that owns this consumer state */
    vm: Weak<Vm>,
    /* The immutable vsock CID of the consumer VM */
    vsock_cid: u32,
    /* The QOM path to the QEMU memory backend device for the donated memory region */
    qom_path: String,
    /* The maximum amount of ephemeral memory the consumer can receive */
    size: u64,
    /* The Vsock stream for communicating with the consumer guest */
    vsock_conn: Mutex<Option<VsockStream>>,
    /* Mutable state for the consumer VM protected by a mutex */
    mut_state: Mutex<ConsumerMutState>,
    /* Condition variable for signaling a CXL add DC event */
    add_dc_cond: Condvar,
    /* Condition variable for signaling a CXL release DC event */
    release_dc_cond: Condvar,
}

struct ConsumerMutState {
    /*
     * Map of areas in the consumer DCD device that have been allocated, and
     * which donor supplied the memory for each area.
     */
    allocated_areas: BTreeMap<u64, Arc<EphAllocation>>,
}

impl ConsumerState {
    pub fn new(
        vm: Weak<Vm>,
        qmp_path: &Path,
        conn: &mut qmp::QmpConnection,
    ) -> std::io::Result<Option<Self>> {
        let qom_base_path = "/machine/peripheral";
        let cxl_dcd_type = "cxl-type3";
        // By convention, the QMP socket will be <qmp_dir>/<vsock_cid>.qmp
        let Some(cid) = qmp_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return Ok(None);
        };

        // Find the QEMU path and size for the CXL DCD device.
        // An error here could just mean that "/machine/peripheral" doesn't exist,
        // so we shouldn't treat that as a fatal error.
        let obj_list = match conn.qom_list(qom_base_path) {
            Ok(list) => list,
            Err(_) => return Ok(None),
        };
        for obj in obj_list {
            if obj.type_.contains(cxl_dcd_type) {
                let qom_path = format!("{}/{}", qom_base_path, obj.name);

                // Every cxl-type3 device has a "volatile-dc-memdev" link
                // property, but it's only set if the device is actually
                // configured for Dynamic Capacity -- an unconfigured link reads
                // back as an empty string rather than an error. Treat that as
                // "not a DCD" and keep looking, rather than erroring out (which
                // would abort registration of the whole VM, donor state and
                // all, via the `?` in check_new_vm).
                let memdev_path = conn.qom_get::<String>(&qom_path, "volatile-dc-memdev")?;
                if memdev_path.is_empty() {
                    continue;
                }

                // Now that we have the memdev path, we can get the size
                let size = conn.qom_get::<u64>(&memdev_path, "size")?;

                let consumer_state = Self {
                    vm,
                    vsock_cid: cid,
                    qom_path,
                    size,
                    vsock_conn: Mutex::new(None),
                    mut_state: Mutex::new(ConsumerMutState {
                        allocated_areas: BTreeMap::new(),
                    }),
                    add_dc_cond: Condvar::new(),
                    release_dc_cond: Condvar::new(),
                };
                return Ok(Some(consumer_state));
            }
        }
        Ok(None)
    }

    pub fn handle_eph_mem_request(
        &self,
        vms: &VmList,
        size: u64,
        request_id: u64,
    ) -> std::io::Result<()> {
        // The consumer allocation is not latency sensitive, so we're fine
        // printing here.
        log!(
            LogLevel::Info,
            "Consumer VM {} requesting ephemeral memory of size {}",
            self.vm().qmp_socket_path.display(),
            size
        );

        // Make sure the size is EPH_MEM_DONATION_GRANULARITY aligned
        let Some(size) = size.checked_next_multiple_of(eph_proto::EPH_MEM_DONATION_GRANULARITY)
        else {
            return Err(std::io::Error::other(
                "Requested size is too large to align to donation granularity",
            ));
        };

        let mut rsvd_allocs = self.reserve_memory(size);
        if rsvd_allocs.is_empty() {
            // TODO: Replace this with a harmless message to the consumer that
            // its request will not be satisfied.
            return Err(std::io::Error::other(
                "Consumer VM has no space available for the requested allocation",
            ));
        };

        let backed_allocs = donor::DonorState::allocate_eph_memory(vms, &rsvd_allocs);
        log!(
            LogLevel::Debug,
            "Consumer VM {} reserved {} allocations, {} of which were backed by donors",
            self.vm().qmp_socket_path.display(),
            rsvd_allocs.len(),
            backed_allocs
        );

        // Not all of the EphAllocations we reserved may  have been backed by
        // the donor. Release any that weren't backed.
        let offsets_to_drop = rsvd_allocs[backed_allocs..]
            .iter()
            .map(|a| a.consumer_offset)
            .collect::<Vec<_>>();
        let _ = self.release_reserved_memory(&offsets_to_drop);

        rsvd_allocs.retain(|a| a.is_committed() && !a.is_release_requested());
        if rsvd_allocs.is_empty() {
            return Err(std::io::Error::other(
                "No donor VM could satisfy the requested allocation.",
            ));
        }

        // Now we can send a QMP event to the consumer VM with the details of
        // the allocation. The lock is scoped to this block so it is released
        // before return_eph_memory_from_consumer locks a different Vm's QMP
        // mutex below; a MutexGuard created directly in an `if let` condition
        // is held for the entire consequent body, not just the condition.
        let result = {
            let consumer_vm = self.vm();
            let mut qmp = consumer_vm.qmp.lock().unwrap();
            let extents = rsvd_allocs
                .iter()
                .map(|a| a.to_extent())
                .collect::<Vec<_>>();
            qmp.cxl_add_dynamic_capacity(self.get_qom_path(), &extents)
        };
        if let Err(e) = result {
            Vm::return_eph_memory_from_consumer(&rsvd_allocs);
            return Err(std::io::Error::other(format!(
                "Failed to add dynamic capacity: {}",
                e
            )));
        }

        // Wait for the confirmation from the consumer that the dynamic capacity
        // has been added.
        // 1000ms should be plenty of time to for the consumer to process the
        // dynamic capacity addition.
        let timed_out = !self.wait_for_add_dc(&rsvd_allocs, crate::QMP_WAIT_TIMEOUT_MS);
        if timed_out {
            eprintln!(
                "Timed out waiting for consumer VM {} to add dynamic capacity",
                self.vm().qmp_socket_path.display()
            );
        }

        // Filter out whatever allocations didn't make the cut
        let (live, dead): (Vec<_>, Vec<_>) = rsvd_allocs.into_iter().partition(|a| a.is_live());
        if !dead.is_empty() {
            Vm::return_eph_memory_from_consumer(&dead);
        }
        if live.is_empty() {
            return Err(std::io::Error::other(
                "No donor VM could satisfy the requested allocation.",
            ));
        }

        // Send message to the consumer guest over the vsock connection to
        // notify it that the dynamic capacity has been added.
        if let Err(e) = self.send_consumer_response(Some(request_id), &live) {
            // If we can't notify the consumer, there's no point in keeping the
            // allocation, so remove it.
            Vm::return_eph_memory_from_consumer(&live);
            return Err(std::io::Error::other(format!(
                "Failed to send consumer response: {}",
                e
            )));
        }

        log!(
            LogLevel::Info,
            "Consumer VM {} successfully added dynamic capacity of size {}",
            self.vm().qmp_socket_path.display(),
            live.len() as u64 * eph_proto::EPH_MEM_DONATION_GRANULARITY
        );

        Ok(())
    }

    /// Returns the `Vm` that owns this consumer state. Since a `ConsumerState`
    /// is only ever reachable through the `Arc<Vm>` that owns it, the parent
    /// Vm is guaranteed to still be alive here.
    fn vm(&self) -> Arc<Vm> {
        self.vm
            .upgrade()
            .expect("ConsumerState outlived its parent Vm")
    }

    /// Reserve up to `size` bytes of ephemeral memory in the consumer's DCD
    /// device. This allows us to know that the consumer can hold its own
    /// request before we ask donors to cough up memory for it.
    ///
    /// Returns a Vec of the EphAllocations for the reserved allocations.
    pub fn reserve_memory(&self, size: u64) -> Vec<Arc<EphAllocation>> {
        let mut allocations = Vec::new();
        let mut remaining_size = size;
        while remaining_size > 0 {
            let mut rsvd_allocs = self.reserve_memory_contig(remaining_size);
            if rsvd_allocs.is_empty() {
                break;
            }
            let alloc_size = rsvd_allocs.len() as u64 * eph_proto::EPH_MEM_DONATION_GRANULARITY;
            allocations.append(&mut rsvd_allocs);

            remaining_size -= alloc_size;
        }
        allocations
    }

    /// Reserve a contiguous range in the consumer's DCD device for an
    /// ephemeral memory allocation.
    /// Returns a Vec of the EphAllocations for the reserved allocations.
    pub fn reserve_memory_contig(&self, size: u64) -> Vec<Arc<EphAllocation>> {
        let mut largest_gap: u64 = 0;
        let mut largest_gap_offset: u64 = 0;
        let mut last_region_end: u64 = 0;
        let mut first_region = true;
        let mut broke_early = false;
        let size = if size > self.size { self.size } else { size };
        let size = size & !(eph_proto::EPH_MEM_DONATION_GRANULARITY - 1);
        let mut allocations = Vec::new();
        let mut mut_state = self.mut_state.lock().unwrap();

        if size == 0 {
            return Vec::new();
        }

        // If there are no allocated areas, the whole device is free, so just
        // reserve from the start.
        if mut_state.allocated_areas.is_empty() {
            for offset in (0..size).step_by(eph_proto::EPH_MEM_DONATION_GRANULARITY as usize) {
                let new_alloc = Arc::new(EphAllocation::new(offset, Arc::downgrade(&self.vm())));
                mut_state.allocated_areas.insert(offset, new_alloc.clone());
                allocations.push(new_alloc);
            }
            return allocations;
        }

        // Find the first gap big enough for the requested allocation.
        for alloc_region in mut_state.allocated_areas.iter() {
            let region_offset = *alloc_region.0;
            let region_size = alloc_region.1.size();

            if first_region {
                // Check for a gap before the first allocated region
                if region_offset > 0 {
                    largest_gap = region_offset;
                    largest_gap_offset = 0;
                }
                first_region = false;
            } else {
                // Check for a gap between the last allocated region and this one
                let gap_size = region_offset - last_region_end;
                if gap_size > largest_gap {
                    largest_gap = gap_size;
                    largest_gap_offset = last_region_end;
                }
            }

            if largest_gap >= size {
                broke_early = true;
                break;
            }

            last_region_end = region_offset + region_size;
        }

        // The loop only checks gaps between allocated regions, so check for a
        // gap after the last region if we haven't yet found a suitable area.
        let final_gap = self.size - last_region_end;
        if final_gap > largest_gap && !broke_early {
            largest_gap = final_gap;
            largest_gap_offset = last_region_end;
        }

        // largest_gap may be smaller than the requested size here. That OK.
        // Just make sure It's at least as big as the allocation granularity.
        largest_gap &= !(eph_proto::EPH_MEM_DONATION_GRANULARITY - 1);
        if largest_gap == 0 {
            return Vec::new();
        }
        let size = if largest_gap < size {
            largest_gap
        } else {
            size
        };

        for offset in (largest_gap_offset..largest_gap_offset + size)
            .step_by(eph_proto::EPH_MEM_DONATION_GRANULARITY as usize)
        {
            let new_alloc = Arc::new(EphAllocation::new(offset, Arc::downgrade(&self.vm())));
            mut_state.allocated_areas.insert(offset, new_alloc.clone());
            allocations.push(new_alloc);
        }

        allocations
    }

    pub fn release_reserved_memory(&self, offsets: &[u64]) -> std::io::Result<()> {
        let mut ret_val: std::io::Result<()> = Ok(());
        let mut allocs_to_release = Vec::new();
        let mut notify_add_dc = false;

        let mut mut_state = self.mut_state.lock().unwrap();
        for offset in offsets {
            let Some(allocation) = mut_state.allocated_areas.get(offset).cloned() else {
                // Not found means either a caller passed a bad offset, or --
                // now that other callers can race to release the same
                // allocation -- someone else already claimed and is handling
                // it. Either way there's nothing for us to do, and every caller
                // already discards this Result, so don't log it as an error.
                continue;
            };

            // Atomically claim responsibility for releasing this allocation
            // exactly once, the same idiom as dcd_set: a concurrent second
            // caller for the same allocation (e.g. this VM's own
            // request-handling thread racing with a VM-teardown-triggered
            // release_all_allocations) finds it already claimed and returns
            // immediately, instead of both of them sending a redundant QMP
            // release command.
            if allocation.release_requested.set(()).is_err() {
                continue;
            }
            notify_add_dc = true;

            if allocation.dcd_set.get().is_none() {
                // The allocation never made it to the DCD, so remove it manually.
                mut_state.allocated_areas.remove(offset);
                continue;
            }
            allocs_to_release.push(allocation);
        }
        drop(mut_state);

        // Wake up threads waiting extents to be added to a consumer's DCD in
        // case an extent they are waiting for was just released.
        if notify_add_dc {
            self.add_dc_cond.notify_all();
        }

        if allocs_to_release.is_empty() {
            return Ok(());
        }

        let vm = self.vm();
        let extents_to_release = allocs_to_release
            .iter()
            .map(|a| a.to_extent())
            .collect::<Vec<_>>();
        let mut qmp = vm.qmp.lock().unwrap();
        if let Err(e) = qmp.cxl_release_dynamic_capacity(&self.qom_path, &extents_to_release) {
            let err = std::io::Error::other(format!(
                "Failed to release dynamic capacity at CID {}: {}",
                self.vsock_cid, e
            ));
            eprintln!("{}", err);
            ret_val = Err(err);
        }
        drop(qmp);

        let manual_remove = if ret_val.is_err() {
            // There was a previous error, so just remove it manually
            true
        } else if !self.wait_for_release_dc(&allocs_to_release, crate::QMP_WAIT_TIMEOUT_MS) {
            // A timeout means that we can't revoke the consumer's ephemeral
            // memory. This is very unlikely to happen in our use case, so we
            // will just log an error for now and manual remove the allocation.
            // We will revisit this if it becomes an issue later.
            let pending_offsets = {
                let mut_state = self.mut_state.lock().unwrap();
                allocs_to_release
                    .iter()
                    .map(|a| a.consumer_offset)
                    .filter(|offset| mut_state.allocated_areas.contains_key(offset))
                    .collect::<Vec<_>>()
            };
            let err = std::io::Error::other(format!(
                "Timeout waiting for release of dynamic capacity at CID {} for offsets {:?}",
                self.vsock_cid, pending_offsets,
            ));
            eprintln!("{}", err);
            ret_val = Err(err);
            // We timed out, so just manually remove the allocation from
            // list of allocated areas.
            true
        } else {
            // The allocation was removed by via the event handler
            false
        };

        if manual_remove {
            let mut mut_state = self.mut_state.lock().unwrap();
            for offset in allocs_to_release.iter().map(|a| a.consumer_offset) {
                mut_state.allocated_areas.remove(&offset);
            }
        }

        ret_val
    }

    pub fn signal_add_dc(&self, extents: &Vec<qmp::types::CxlDynamicCapacityExtent>) {
        let mut notify = false;
        let mut extents_to_release = Vec::new();
        let vm = self.vm();
        let mut note_gap = |offset: u64, len: u64| {
            extents_to_release.push(qmp::types::CxlDynamicCapacityExtent { offset, len });
            eprintln!(
                "CXL Add DC event at CID {} for {}:{} has no matching allocation",
                self.vsock_cid, offset, len
            );
        };

        // See if the added extents match any of the currently allocated areas.
        let mut_state = self.mut_state.lock().unwrap();
        for extent in extents {
            let overlapping_allocs = mut_state.find_covered_allocations(extent.offset, extent.len);

            let mut gap_cursor = extent.offset;
            for alloc in overlapping_allocs {
                // Mark the allocation as being added
                if alloc.dcd_set.set(()).is_err() {
                    eprintln!(
                        "CXL Add DC event at CID {} for {}:{} that has already been added",
                        self.vsock_cid,
                        alloc.consumer_offset,
                        alloc.size()
                    );
                }
                notify = true;

                // Find gaps between allocations in the extent. This is made
                // easy by the fact that find_overlapping_allocations() returns
                // allocations in order of their consumer_offset.
                if gap_cursor < alloc.consumer_offset {
                    note_gap(gap_cursor, alloc.consumer_offset - gap_cursor);
                }
                gap_cursor = alloc.consumer_offset + alloc.size();
            }

            // Check for a gap after the last allocation in the extent
            if gap_cursor < extent.offset + extent.len {
                note_gap(gap_cursor, extent.offset + extent.len - gap_cursor);
            }
        }
        drop(mut_state);

        if notify {
            self.add_dc_cond.notify_all();
        }

        // Inform the consumer of invalid extents that it added to its DCD.
        if !extents_to_release.is_empty() {
            let mut qmp = vm.qmp.lock().unwrap();
            if let Err(e) = qmp.cxl_release_dynamic_capacity(&self.qom_path, &extents_to_release) {
                eprintln!(
                    "Failed to release dynamic capacity at CID {}: {}",
                    self.vsock_cid, e
                );
            }
        }
    }

    pub fn signal_release_dc(&self, extents: &Vec<qmp::types::CxlDynamicCapacityExtent>) {
        let mut notify = false;
        let mut mut_state = self.mut_state.lock().unwrap();

        for extent in extents {
            log!(
                LogLevel::Debug,
                "Consumer VM {} released ephemeral memory at offset {} of size {}",
                self.vm().qmp_socket_path.display(),
                extent.offset,
                extent.len
            );

            let overlapping_allocs = mut_state.find_covered_allocations(extent.offset, extent.len);
            if overlapping_allocs.is_empty() {
                eprintln!(
                    "CXL Release DC event at CID {} for {}:{} has no matching allocation",
                    self.vsock_cid, extent.offset, extent.len
                );
                continue;
            }
            for alloc in overlapping_allocs {
                mut_state.allocated_areas.remove(&alloc.consumer_offset);
                notify = true;
            }
        }

        if notify {
            self.release_dc_cond.notify_all();
        }
    }

    pub fn release_all_allocations(&self) {
        // It's possible that a new allocation could appear between when we
        // collect the list of allocations and when we actually remove them.
        // To be safe, keep checking for new allocations until there are none.
        loop {
            // Clone the EphAllocs separately, so we can call
            // Vm::return_eph_memory_from_consumer(), which takes the consumer lock.
            // Filter out allocations that already have a thread cleaning them up.
            let allocs = {
                let mut_state = self.mut_state.lock().unwrap();
                mut_state
                    .allocated_areas
                    .values()
                    .filter(|a| !a.is_release_requested())
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if allocs.is_empty() {
                break;
            }

            Vm::return_eph_memory_from_consumer(&allocs);
        }
    }

    fn wait_for_add_dc(&self, allocations: &[Arc<EphAllocation>], timeout_ms: u64) -> bool {
        let mut_state = self.mut_state.lock().unwrap();
        let timeout = Duration::from_millis(timeout_ms);

        let (_unused, wait_result) = self
            .add_dc_cond
            .wait_timeout_while(mut_state, timeout, |_| {
                allocations
                    .iter()
                    .any(|a| a.dcd_set.get().is_none() && !a.is_release_requested())
            })
            .unwrap();

        !wait_result.timed_out()
    }

    fn wait_for_release_dc(&self, allocations: &[Arc<EphAllocation>], timeout_ms: u64) -> bool {
        let mut_state = self.mut_state.lock().unwrap();
        let timeout = Duration::from_millis(timeout_ms);

        let (_unused, wait_result) = self
            .release_dc_cond
            .wait_timeout_while(mut_state, timeout, |state| {
                allocations.iter().any(|allocation| {
                    state
                        .allocated_areas
                        .contains_key(&allocation.consumer_offset)
                })
            })
            .unwrap();

        !wait_result.timed_out()
    }

    fn send_consumer_response(
        &self,
        id: Option<u64>,
        allocations: &[Arc<EphAllocation>],
    ) -> std::io::Result<()> {
        let mut conn = self.vsock_conn.lock().unwrap();

        let Some(conn) = conn.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Vsock connection not available",
            ));
        };

        let mut iter = allocations.iter().peekable();
        while let Some(alloc) = iter.next() {
            let more = iter.peek().is_some();
            let response = ConsumerCommand::new(
                ConsumerFunction::EphMemResponse,
                Some(alloc.size()),
                Some(alloc.consumer_offset),
            )
            .with_id(id)
            .with_more(more);

            response.send(conn)?;
        }

        Ok(())
    }

    pub fn get_qom_path(&self) -> &str {
        &self.qom_path
    }
}

impl ConsumerMutState {
    /// Returns entries in self.allocated_areas that are fully covered by offset
    /// and offset + size. The returned entries will be sorted in ascending
    /// order by EphAllocation.consumer_offset.
    pub fn find_covered_allocations(&self, offset: u64, size: u64) -> Vec<Arc<EphAllocation>> {
        let mut overlapping_allocs = Vec::new();

        for (alloc_offset, allocation) in self
            .allocated_areas
            .range((Included(offset), Excluded(offset + size)))
        {
            if offset <= *alloc_offset && offset + size >= *alloc_offset + allocation.size() {
                overlapping_allocs.push(allocation.clone());
            }
        }

        overlapping_allocs
    }
}

/// Thread that waits for consumer VMs to connect for the first time.
/// Spawns a new thread for each consumer VM that connects.
///
/// * `vms` - The list of VMs that have connected.
/// * `listening_port` - The port to listen on for new consumer VM connections.
/// * `qmp_path` - The path to the directory where QMP sockets are stored.
pub fn consumer_listener_thread(
    vms: Arc<crate::VmList>,
    listening_port: u32,
    qmp_path: &str,
) -> std::io::Result<()> {
    // Listen for new connections from any VM.
    let listen_address = VsockAddr::new(VMADDR_CID_ANY, listening_port);
    let listener = VsockListener::bind(&listen_address)?;

    loop {
        let (stream, addr) = match listener.accept() {
            Ok((s, a)) => (s, a),
            Err(e) => {
                eprintln!("Error accepting connection: {}", e);
                continue;
            }
        };
        log!(
            LogLevel::Info,
            "New consumer VM connected from CID {} on port {}",
            addr.cid(),
            addr.port()
        );

        // By convention, the consumer VM's QMP socket will be at <qmp_dir>/<vsock_cid>.qmp
        let qmp_path = format!("{}/{}.qmp", qmp_path, addr.cid());

        // Find the VM corresponding to the CID of the connecting consumer VM.
        let Some(consumer) = Vm::get_consumer(&vms, &PathBuf::from(&qmp_path)) else {
            eprintln!(
                "No VM found for consumer connection from CID {}. Closing connection.",
                addr.cid()
            );
            continue;
        };
        // Unwrap is safe because Vm::get_consumer ensures that consumer is Some.
        let consumer_state = consumer.consumer.as_ref().unwrap();

        // We don't want to be stalled by a misbehaving consumer VM. If try_lock fails,
        // we know that another connection is active, so we can safely abandon this
        // connection attempt.
        let Ok(mut vsock_conn) = consumer_state.vsock_conn.try_lock() else {
            eprintln!(
                "Consumer VM with CID {} already has a vsock connection. Closing new connection.",
                addr.cid()
            );
            continue;
        };

        // Check if the consumer VM already has a connection.
        if vsock_conn.is_some() {
            eprintln!(
                "Consumer VM with CID {} already has a connection. Closing new connection.",
                addr.cid()
            );
            continue;
        }
        // Set the vsock connection for the consumer VM.
        if let Ok(stream_clone) = stream.try_clone() {
            *vsock_conn = Some(stream_clone);
        } else {
            eprintln!(
                "Failed to clone VsockStream for consumer VM with CID {}. Closing connection.",
                addr.cid()
            );
            continue;
        }
        // We need to drop the state to satisfy the borrow checker before
        // spawning the thread.
        drop(vsock_conn);

        let vms_clone = vms.clone();
        std::thread::spawn(move || {
            if let Err(e) = consumer_thread(consumer, vms_clone, stream) {
                eprintln!("Error in consumer thread for CID {}: {}", addr.cid(), e);
            }
        });
    }
}

// Cap on the length of a single message from a consumer VM, to bound memory
// use if a guest sends a line with no '\n'.
const MAX_LINE_LEN: u64 = 4096;

fn consumer_thread(
    consumer: Arc<Vm>,
    vms: Arc<crate::VmList>,
    vsock: VsockStream,
) -> std::io::Result<()> {
    let mut buf = BufReader::new(vsock);
    let mut line = String::new();

    loop {
        line.clear();
        let mut limited = (&mut buf).take(MAX_LINE_LEN);
        match limited.read_line(&mut line) {
            Ok(0) => break, // EOF reached
            Ok(_) if line.ends_with('\n') => {
                let consumer_cmd = match ConsumerCommand::from_str(&line) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        eprintln!(
                            "Failed to parse command from consumer VM at '{}': {}",
                            consumer.qmp_socket_path.display(),
                            e
                        );
                        continue;
                    }
                };
                // Like the QMP event handler thread, it might be better to
                // spawn a thread to do this work, or add the command to a work
                // queue for processing.
                // For now, just handle the command synchronously in this thread.
                if let Err(e) = consumer_cmd_dispatacher(&consumer, &vms, consumer_cmd) {
                    eprintln!(
                        "Error handling command from consumer VM at '{}': {}",
                        consumer.qmp_socket_path.display(),
                        e
                    );
                }
            }
            Ok(_) => {
                // Either MAX_LINE_LEN was hit before a '\n', or the peer
                // closed mid-line. Either way, stop reading from this guest.
                eprintln!(
                    "Consumer VM at '{}' sent a line longer than {} bytes or an unterminated final line; closing connection",
                    consumer.qmp_socket_path.display(),
                    MAX_LINE_LEN
                );
                break;
            }
            Err(e) => {
                eprintln!(
                    "Error reading from consumer VM at '{}': {}",
                    consumer.qmp_socket_path.display(),
                    e
                );
                break;
            }
        }
    }

    // Reset the vsock connection for the consumer VM when the thread exits.
    // Potential TOCTOU issue here between the connection being closed and the consumer
    // starting a new connection before vsock_conn is set to None. This could be fixed
    // in the future by having the listener thread send a "already connected" message
    // to the consumer, indicating that it should close any other connections andtry again.
    if let Some(consumer_state) = &consumer.consumer {
        let mut vsock_conn = consumer_state.vsock_conn.lock().unwrap();
        *vsock_conn = None;
    }
    Ok(())
}

fn consumer_cmd_dispatacher(
    consumer: &Arc<Vm>,
    vms: &Arc<crate::VmList>,
    cmd: ConsumerCommand,
) -> std::io::Result<()> {
    match cmd.function {
        ConsumerFunction::EphMemRequest => {
            if let Some(size) = cmd.size
                && let Some(request_id) = cmd.id
            {
                // Unwrap is safe because consumer_cmd_dispatcher() is only
                // called from consumer_thread(), which only operates on
                // consumer VMs.
                consumer
                    .consumer
                    .as_ref()
                    .unwrap()
                    .handle_eph_mem_request(vms, size, request_id)?;
            } else {
                eprintln!(
                    "{} command from consumer VM at '{}' missing size or request_id argument",
                    cmd.to_json()
                        .expect("ConsumerCommand is always serializable"),
                    consumer.qmp_socket_path.display()
                );
            }
        }
        _ => {
            eprintln!(
                "Unknown command '{}' from consumer VM at '{}'",
                cmd.to_json()
                    .expect("ConsumerCommand is always serializable"),
                consumer.qmp_socket_path.display()
            );
        }
    }
    Ok(())
}
