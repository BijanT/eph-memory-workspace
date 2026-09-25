//! Helper functions for interacting with Consumer VMs.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::{EphAllocation, Vm, VmList, donor, qmp};
use serde::{Deserialize, Serialize};
use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

#[allow(dead_code)]
pub struct ConsumerState {
    /* Back-pointer to the Vm that owns this consumer state */
    vm: Weak<Vm>,
    /* The immutable vsock CID of the consumer VM */
    vsock_cid: u32,
    /* The QOM path to the QEMU memory backend device for the donated memory region */
    qom_path: String,
    /* The maximum amount of ephemeral memory the consumer can receive */
    size: u64,
    /* Mutable state for the consumer VM protected by a mutex */
    mut_state: Mutex<ConsumerMutState>,
}

#[allow(dead_code)]
struct ConsumerMutState {
    /* The Vsock stream for communicating with the consumer guest */
    vsock_conn: Option<VsockStream>,
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
        let obj_list = match qmp::qom_list(conn, qom_base_path) {
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
                let memdev_path = qmp::qom_get::<String>(conn, &qom_path, "volatile-dc-memdev")?;
                if memdev_path.is_empty() {
                    continue;
                }

                // Now that we have the memdev path, we can get the size
                let size = qmp::qom_get::<u64>(conn, &memdev_path, "size")?;

                let consumer_state = Self {
                    vm,
                    vsock_cid: cid,
                    qom_path,
                    size,
                    mut_state: Mutex::new(ConsumerMutState {
                        vsock_conn: None,
                        allocated_areas: BTreeMap::new(),
                    }),
                };
                return Ok(Some(consumer_state));
            }
        }
        Ok(None)
    }

    pub fn handle_eph_mem_request(&self, vms: &VmList, size: u64) -> std::io::Result<()> {
        // Make sure the size is EPH_MEM_DONATION_GRANULARITY aligned
        let Some(size) = size.checked_next_multiple_of(crate::EPH_MEM_DONATION_GRANULARITY) else {
            return Err(std::io::Error::other(
                "Requested size is too large to align to donation granularity",
            ));
        };

        let Some(rsvd_alloc) = self.reserve_memory(size) else {
            // TODO: Replace this with a harmless message to the consumer that
            // its request will not be satisfied.
            return Err(std::io::Error::other(
                "Consumer VM has no space available for the requested allocation",
            ));
        };

        if !donor::DonorState::allocate_eph_memory(vms, &rsvd_alloc) {
            self.release_reserved_memory(rsvd_alloc.consumer_offset);
            // TODO: If we could not allocate memory from any donor, return the
            // reserved allocation to the consumer and return an error.
            return Err(std::io::Error::other(
                "No donor VM could satisfy the requested allocation",
            ));
        };

        let rsvd_offset = rsvd_alloc.consumer_offset;
        let size_from_donor = rsvd_alloc.size();

        // Now we can send a QMP event to the consumer VM with the details of
        // the allocation. The lock is scoped to this block so it is released
        // before return_eph_memory_from_consumer locks a different Vm's QMP
        // mutex below; a MutexGuard created directly in an `if let` condition
        // is held for the entire consequent body, not just the condition.
        let result = {
            let consumer_vm = self.vm();
            let mut qmp = consumer_vm.qmp.lock().unwrap();
            qmp::cxl_add_dynamic_capacity(
                &mut qmp,
                self.get_qom_path(),
                rsvd_offset,
                size_from_donor,
            )
        };
        if let Err(e) = result {
            Vm::return_eph_memory_from_consumer(&rsvd_alloc);
            return Err(std::io::Error::other(format!(
                "Failed to add dynamic capacity: {}",
                e
            )));
        }

        // TODO: Wait for CXL_ADD_DYNAMIC_CAPACITY_RESPONSE to send notification
        // to the consumer guest over the vsock connection.

        Ok(())
    }

    /// Returns the `Vm` that owns this consumer state. Since a `ConsumerState`
    /// is only ever reachable through the `Arc<Vm>` that owns it, the parent
    /// Vm is guaranteed to still be alive here.
    #[allow(dead_code)]
    fn vm(&self) -> Arc<Vm> {
        self.vm
            .upgrade()
            .expect("ConsumerState outlived its parent Vm")
    }

    /// Reserve a contiguous range in the consumer's DCD device for an
    /// ephemeral memory allocation. Returns the offset and size of the
    /// reserved range, or None if no space is available. This allows us to
    /// know that the consumer can hold its own request before we ask donors to
    /// cough up some memory for it.
    /// Returns the EphAllocation structure for the reserved allocation.
    pub fn reserve_memory(&self, size: u64) -> Option<Arc<EphAllocation>> {
        let mut largest_gap: u64 = 0;
        let mut largest_gap_offset: u64 = 0;
        let mut last_region_end: u64 = 0;
        let mut first_region = true;
        let mut broke_early = false;
        let size = if size > self.size { self.size } else { size };
        let size = size & !(crate::EPH_MEM_DONATION_GRANULARITY - 1);
        let mut mut_state = self.mut_state.lock().unwrap();

        if size == 0 {
            return None;
        }

        // If there are no allocated areas, the whole device is free, so just
        // reserve from the start.
        if mut_state.allocated_areas.is_empty() {
            let new_alloc = Arc::new(EphAllocation::new(size, 0, Arc::downgrade(&self.vm())));
            mut_state.allocated_areas.insert(0, new_alloc.clone());
            return Some(new_alloc);
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
        largest_gap &= !(crate::EPH_MEM_DONATION_GRANULARITY - 1);
        if largest_gap == 0 {
            return None;
        }
        let size = if largest_gap < size {
            largest_gap
        } else {
            size
        };

        let reserved_alloc = Arc::new(EphAllocation::new(
            size,
            largest_gap_offset,
            Arc::downgrade(&self.vm()),
        ));
        mut_state
            .allocated_areas
            .insert(largest_gap_offset, reserved_alloc.clone());

        Some(reserved_alloc)
    }

    /// Release reserved memory that was not successfully allocated.
    pub fn release_reserved_memory(&self, offset: u64) {
        let mut mut_state = self.mut_state.lock().unwrap();
        if mut_state.allocated_areas.remove(&offset).is_none() {
            eprintln!(
                "No reserved allocation found at offset {} for consumer VM with CID {}",
                offset, self.vsock_cid
            );
        }
    }

    pub fn get_qom_path(&self) -> &str {
        &self.qom_path
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
// Consumer commands are much simpler than QMP. They only have a function name
// and, optionally, a size argument.
pub struct ConsumerCommand {
    pub function: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl ConsumerCommand {
    #[allow(dead_code)]
    pub fn new(function: impl Into<String>, size: Option<u64>) -> Self {
        Self {
            function: function.into(),
            size,
        }
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
        println!(
            "New consumer VM connected from CID {} on port {}",
            addr.cid(),
            addr.port()
        );

        // By convention, the consumer VM's QMP socket will be at <qmp_dir>/<vsock_cid>.qmp
        let qmp_path = format!("{}/{}.qmp", qmp_path, addr.cid());

        // Find the VM corresponding to the CID of the connecting consumer VM.
        let Some(consumer) = vms.read().unwrap().get(&PathBuf::from(&qmp_path)).cloned() else {
            eprintln!(
                "No VM found for consumer connection from CID {}. Closing connection.",
                addr.cid()
            );
            continue;
        };

        if let Some(consumer_state) = &consumer.consumer {
            let mut state = consumer_state.mut_state.lock().unwrap();
            // Check if the consumer VM already has a connection.
            if state.vsock_conn.is_some() {
                eprintln!(
                    "Consumer VM with CID {} already has a connection. Closing new connection.",
                    addr.cid()
                );
                continue;
            }
            // Set the vsock connection for the consumer VM.
            if let Ok(stream_clone) = stream.try_clone() {
                state.vsock_conn = Some(stream_clone);
            } else {
                eprintln!(
                    "Failed to clone VsockStream for consumer VM with CID {}. Closing connection.",
                    addr.cid()
                );
                continue;
            }
        } else {
            eprintln!(
                "Consumer state not found for VM with CID {}. Closing connection.",
                addr.cid()
            );
            continue;
        }

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
                let consumer_cmd = match serde_json::from_str::<ConsumerCommand>(&line) {
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
        let mut state = consumer_state.mut_state.lock().unwrap();
        state.vsock_conn = None;
    }
    Ok(())
}

fn consumer_cmd_dispatacher(
    consumer: &Arc<Vm>,
    vms: &Arc<crate::VmList>,
    cmd: ConsumerCommand,
) -> std::io::Result<()> {
    match cmd.function.as_str() {
        "eph-mem-request" => {
            if let Some(size) = cmd.size {
                // Unwrap is safe because consumer_cmd_dispatcher() is only
                // called from consumer_thread(), which only operates on
                // consumer VMs.
                consumer
                    .consumer
                    .as_ref()
                    .unwrap()
                    .handle_eph_mem_request(vms, size)?;
            } else {
                eprintln!(
                    "Allocate command from consumer VM at '{}' missing size argument",
                    consumer.qmp_socket_path.display()
                );
            }
        }
        _ => {
            eprintln!(
                "Unknown command '{}' from consumer VM at '{}'",
                cmd.function,
                consumer.qmp_socket_path.display()
            );
        }
    }
    Ok(())
}
