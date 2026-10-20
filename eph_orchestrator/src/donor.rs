//! Functions for interacting with DonorVMs

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::qmp::QmpConnection;
use crate::{EphAllocation, LogLevel, Vm, log};

pub struct DonorState {
    /* Back-pointer to the Vm that owns this donor state */
    vm: Weak<Vm>,
    /* The donor's mutable state */
    mut_state: Mutex<DonorMutState>,
}

struct DonorMutState {
    /* The list of donatable regions available in the donor VM */
    donatable_regions: Vec<DonatableRegion>,
}

impl DonorMutState {
    /// Finds the donatable region matching `qom_path`.
    fn find_region(&mut self, qom_path: &str) -> Option<&mut DonatableRegion> {
        self.donatable_regions
            .iter_mut()
            .find(|region| region.path == qom_path)
    }
}

pub struct DonatableRegion {
    /* The total memory available in the memory region */
    size: u64,
    /* The amount of memory that has been donated from this region */
    donated: u64,
    /* The path to the QEMU memory backend device for this donatable region */
    path: String,
    // If there are not enough confirmed allocations in this region to handle a
    // revoke request, that means we have not yet confirmed all of the
    // allocations that this region has made. Keep track of the overshoot here
    // so we can return the excess before confirming the allocation
    revoked_owed: u64,
    /* The allocations donated from this region */
    allocations: BTreeMap<u64, Arc<EphAllocation>>,
}

impl DonorState {
    pub fn new(vm: Weak<Vm>, conn: &mut QmpConnection) -> std::io::Result<Option<Self>> {
        // Get the list of the VM's memory devices.
        let donatable_regions = conn
            .get_memdevs()?
            .into_iter()
            .filter_map(|m| m.try_into().ok())
            .collect::<Vec<DonatableRegion>>();

        if donatable_regions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Self {
                vm,
                mut_state: Mutex::new(DonorMutState { donatable_regions }),
            }))
        }
    }

    /// Returns the `Vm` that owns this donor state. Since a `DonorState` is
    /// only ever reachable through the `Arc<Vm>` that owns it, the parent Vm
    /// is guaranteed to still be alive here.
    fn vm(&self) -> Arc<Vm> {
        self.vm
            .upgrade()
            .expect("DonorState outlived its parent Vm")
    }

    // Does the bookkeeping to convert a reserved allocation into a confirmed
    // allocation after getting ephemeral memory from the donor.
    // Returns a tuple of the size of usable ephemeral memory and the amount of
    // ephemeral memory that we just got from the donor that we need to return
    // immediately.
    fn handle_reserved_alloc(
        &self,
        qom_path: &str,
        alloc_ids: &[u64],
        requested_size: u64,
        granted_size: u64,
    ) -> (u64, u64) {
        let mut donor_mut_state = self.mut_state.lock().unwrap();
        let Some(region) = donor_mut_state.find_region(qom_path) else {
            // We should never actually get here, since this function should
            // only be called if there is a reserved allocation.
            return (0, 0);
        };

        if granted_size > requested_size {
            eprintln!(
                "Warning: Donor granted more memory than was requested. Requested {}, granted {}",
                requested_size, granted_size
            );
        }
        let granted_size = std::cmp::min(granted_size, requested_size);

        // If granted_size is somehow not a multiple of the donation
        // granularity, round it down and return the excess.
        let excess = granted_size % eph_proto::EPH_MEM_DONATION_GRANULARITY;
        let granted_size = granted_size - excess;

        // Has memory been revoked before we had a chance to use it?
        let to_revoke = if region.revoked_owed > 0 {
            let to_revoke = std::cmp::min(region.revoked_owed, granted_size);
            region.revoked_owed -= to_revoke;
            to_revoke
        } else {
            0
        };

        let final_size = granted_size - to_revoke;
        let first_failed_idx = final_size / eph_proto::EPH_MEM_DONATION_GRANULARITY;
        for id in &alloc_ids[first_failed_idx as usize..] {
            region.allocations.remove(id);
        }
        // Even though we incremented region.donated when we first
        // reserved the memory, we don't decrement it now if
        // actual_size is less than the reserved size. This is because
        // if the donor gives us less than we requested, it means that
        // the donor did not have enough free donatable memory to handle
        // our request. Additionally, when a donor uses its donatable
        // memory for itself, it will never again be available for
        // donation.
        (final_size, to_revoke + excess)
    }

    fn allocate_eph_memory_inner(
        path_start: &Path,
        vms: &crate::VmList,
        size: u64,
        rsvd_allocs: &[Arc<EphAllocation>],
    ) -> Option<(Arc<Vm>, String, u64)> {
        let vms = vms.read().unwrap();
        for (_path, vm) in vms.range::<Path, _>((Excluded(path_start), Unbounded)) {
            if let Some(donor_state) = &vm.donor {
                let mut donor_mut_state = donor_state.mut_state.lock().unwrap();
                for region in donor_mut_state.donatable_regions.iter_mut() {
                    if region.size - region.donated < eph_proto::EPH_MEM_DONATION_GRANULARITY {
                        // This region is fully donated, skip it
                        continue;
                    }
                    // Allocate memory from this region, rounding down to the
                    // donation granularity.
                    let size_to_allocate = std::cmp::min(size, region.size - region.donated);
                    let size_to_allocate =
                        size_to_allocate & !(eph_proto::EPH_MEM_DONATION_GRANULARITY - 1);
                    region.donated += size_to_allocate;
                    for i in 0..(size_to_allocate / eph_proto::EPH_MEM_DONATION_GRANULARITY) {
                        let rsvd_alloc = rsvd_allocs[i as usize].clone();
                        region.allocations.insert(rsvd_alloc.id, rsvd_alloc);
                    }
                    return Some((vm.clone(), region.path.clone(), size_to_allocate));
                }
            }
        }
        None
    }

    // Takes EphAllocations reserved by a consumer, but not yet committed, and
    // actually allocates the memory from a donor. Updates the passed in
    // EphAllocation with the donor information if that allocation is backed.
    // Allocations are backed in the order they are provided in rsvd_allocs.
    // Returns the number of allocations that have been processed. They will
    // either be committed or abandoned.
    pub fn allocate_eph_memory(vms: &crate::VmList, rsvd_allocs: &[Arc<EphAllocation>]) -> usize {
        let mut last_path = PathBuf::new();
        let mut backed_allocs = 0;
        while backed_allocs < rsvd_allocs.len() {
            let Some((donor_vm, qom_path, request_size)) = Self::allocate_eph_memory_inner(
                &last_path,
                vms,
                eph_proto::EPH_MEM_DONATION_GRANULARITY
                    * (rsvd_allocs.len() - backed_allocs) as u64,
                &rsvd_allocs[backed_allocs..],
            ) else {
                return backed_allocs;
            };
            let donor_state = donor_vm.donor.as_ref().unwrap();
            let alloc_ids = rsvd_allocs[backed_allocs
                ..backed_allocs
                    + (request_size / eph_proto::EPH_MEM_DONATION_GRANULARITY) as usize]
                .iter()
                .map(|a| a.id)
                .collect::<Vec<u64>>();

            // Send a QMP command requesting request_size bytes from the donor.
            let Ok(size) = donor_vm
                .qmp
                .lock()
                .unwrap()
                .eph_mem_donate_capacity(&qom_path, request_size)
                .inspect_err(|e| eprintln!("QMP error while getting eph memory: {}", e))
            else {
                // If the QMP command fails, handle the failed allocation and
                // continue searching for another donor.
                // This will make it so this donor can no longer donate memory.
                // That's fine since the QMP connection is broken, don't waste
                // time trying to use it again.
                donor_state.handle_reserved_alloc(&qom_path, &alloc_ids, request_size, 0);
                last_path = donor_vm.qmp_socket_path.clone();
                continue;
            };

            // If the donor accepts the request, confirm as many EphAllocations
            // as we can with the granted size.
            let (granted_size, to_revoke) =
                donor_state.handle_reserved_alloc(&qom_path, &alloc_ids, request_size, size);

            // We may need to immediately return some of the memory we just got
            // if to_revoke is non-zero. This can happen if memory is revoked
            // between we were granted memory from the donor and the call to
            // handle_reserved_alloc().
            if to_revoke > 0 {
                let _ = donor_state.return_eph_memory(&qom_path, to_revoke);
            }
            let num_granted = (granted_size / eph_proto::EPH_MEM_DONATION_GRANULARITY) as usize;
            let mut abandoned_allocs = Vec::new();
            for rsvd_alloc in &rsvd_allocs[backed_allocs..backed_allocs + num_granted] {
                if !rsvd_alloc.commit_allocation(Arc::downgrade(&donor_vm), qom_path.clone()) {
                    // The consumer abandoned this allocation while we were
                    // negotiating with the donor. Hand the memory straight back
                    // rather than orphaning it in the donor's bookkeeping.
                    abandoned_allocs.push(rsvd_alloc.id);
                }
            }
            backed_allocs += num_granted;
            if !abandoned_allocs.is_empty() {
                donor_state.return_and_bookkeep_eph_memory(&qom_path, &abandoned_allocs);
                // Abandoned allocations here indicate an issue with the
                // consumer, so just return what we have instead of trying to
                // allocate more.
                break;
            }

            // Try the next donor if we have more allocations to back.
            last_path = donor_vm.qmp_socket_path.clone();
        }

        backed_allocs
    }

    pub fn return_eph_memory(&self, qom_path: &str, size: u64) -> std::io::Result<()> {
        let donor_vm = self.vm();
        donor_vm
            .qmp
            .lock()
            .unwrap()
            .eph_mem_return_capacity(qom_path, size)
            .inspect_err(|e| eprintln!("QMP error while returning eph memory: {}", e))
    }

    pub fn return_and_bookkeep_eph_memory(&self, qom_path: &str, alloc_ids: &[u64]) {
        let return_size = alloc_ids.len() as u64 * eph_proto::EPH_MEM_DONATION_GRANULARITY;
        if self.return_eph_memory(qom_path, return_size).is_ok() {
            for alloc_id in alloc_ids {
                self.bookkeep_returned_eph_memory(qom_path, *alloc_id);
            }
        }
    }

    pub fn bookkeep_returned_eph_memory(&self, donor_qom_path: &str, alloc_id: u64) {
        let mut donor_mut_state = self.mut_state.lock().unwrap();
        let Some(region) = donor_mut_state.find_region(donor_qom_path) else {
            eprintln!(
                "Warning: Donor at {} is returning memory for an unknown ephemeral region {}",
                self.vm().qmp_socket_path.display(),
                donor_qom_path
            );
            return;
        };
        let Some(allocation) = region.allocations.get(&alloc_id) else {
            eprintln!(
                "Warning: Donor at {} is returning memory for an unknown ephemeral allocation id {}",
                self.vm().qmp_socket_path.display(),
                alloc_id
            );
            return;
        };
        let size = allocation.size();
        if region.donated < size {
            eprintln!(
                "Warning: Donor at {} is returning more memory than it has donated. Donated {}, returning {}",
                region.path, region.donated, size
            );
            region.donated = 0;
        } else {
            region.donated -= size;
        }
        region.allocations.remove(&alloc_id);
    }

    /// Revoke `revoke_size` bytes of ephemeral memory owned by the donor VM
    /// device at `qom_path`.
    pub fn revoke_eph_memory(&self, qom_path: &str, revoke_size: u64) -> std::io::Result<()> {
        // Keep revoke_size donation granularity aligned
        let Some(revoke_size) =
            revoke_size.checked_next_multiple_of(eph_proto::EPH_MEM_DONATION_GRANULARITY)
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Revoke size {} is too large to align to donation granularity {}",
                    revoke_size,
                    eph_proto::EPH_MEM_DONATION_GRANULARITY
                ),
            ));
        };
        // First find the allocations to revoke that will satisfy the revocation
        // request. If there are not enough committed allocations to satisfy the
        // request, that may mean there are some allocations that have received
        // memory from the donor, but have not yet committed in. In that case,
        // set the revoked_owed amount accordingly.
        let allocs_to_revoke = {
            let mut donor_mut_state = self.mut_state.lock().unwrap();
            let mut allocs_to_revoke = Vec::new();
            let mut remaining_revoke_size = revoke_size;
            let Some(region) = donor_mut_state.find_region(qom_path) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "Donatable region {} not found in donor {:?}",
                        qom_path,
                        self.vm().qmp_socket_path
                    ),
                ));
            };

            // There are many possibly fancy policies for selecting which
            // allocations to revoke. For now, we will just revoke the most
            // recent committed allocations first.
            for alloc in region.allocations.values().rev() {
                if remaining_revoke_size == 0 {
                    break;
                }
                if alloc.try_claim_for_revoke() {
                    remaining_revoke_size = remaining_revoke_size.saturating_sub(alloc.size());
                    allocs_to_revoke.push(alloc.clone());
                }
            }

            region.revoked_owed += remaining_revoke_size;
            allocs_to_revoke
        };

        Vm::return_eph_memory_from_consumer(&allocs_to_revoke);

        // All the VM communication is done, so we can safely print a debug
        // message without slowing things down.
        log!(
            LogLevel::Info,
            "Revoked ephemeral memory for donor {}: {} bytes",
            self.vm().qmp_socket_path.display(),
            revoke_size
        );
        Ok(())
    }

    pub fn release_all_allocations(&self) {
        // Clone the EphAllocs separately, so we can call
        // Vm::return_eph_memory_from_consumer(), which takes the donor lock
        let allocs = {
            let donor_mut_state = self.mut_state.lock().unwrap();
            let mut allocs = Vec::new();
            for region in &donor_mut_state.donatable_regions {
                // Only get committed allocations. Uncommitted allocations are
                // not the responsibility of this VM.
                allocs.extend(
                    region
                        .allocations
                        .values()
                        .filter(|a| a.is_committed())
                        .cloned(),
                );
            }
            allocs
        };

        log!(
            LogLevel::Debug,
            "Releasing {} ephemeral memory allocations for donor {}",
            allocs.len(),
            self.vm().qmp_socket_path.display()
        );
        Vm::return_eph_memory_from_consumer(&allocs);
    }
}

impl DonatableRegion {
    pub fn new(size: u64, path: String) -> Self {
        Self {
            size,
            donated: 0,
            revoked_owed: 0,
            path,
            allocations: BTreeMap::new(),
        }
    }
}
