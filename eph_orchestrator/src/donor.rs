//! Functions for interacting with DonorVMs

use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::qmp::QmpConnection;
use crate::qmp::{self, eph_mem_donate_capacity};
use crate::{EphAllocation, Vm};

#[allow(dead_code)]
pub struct DonorState {
    /* Back-pointer to the Vm that owns this donor state */
    vm: Weak<Vm>,
    /* The donor's mutable state */
    mut_state: Mutex<DonorMutState>,
}

#[allow(dead_code)]
struct DonorMutState {
    /* The list of donatable regions available in the donor VM */
    donatable_regions: Vec<DonatableRegion>,
}

impl DonorMutState {
    /// Finds the donatable region matching `qom_path`, and the index within
    /// it of the allocation matching `alloc_id`.
    fn find_alloc(
        &mut self,
        qom_path: &str,
        alloc_id: u64,
    ) -> Option<(&mut DonatableRegion, usize)> {
        self.donatable_regions
            .iter_mut()
            .find(|region| region.path == qom_path)
            .and_then(|region| {
                let pos = region.allocations.iter().position(|a| a.id == alloc_id)?;
                Some((region, pos))
            })
    }
}

#[allow(dead_code)]
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
    allocations: Vec<Arc<EphAllocation>>,
}

#[allow(dead_code)]
impl DonorState {
    pub fn new(vm: Weak<Vm>, conn: &mut QmpConnection) -> std::io::Result<Option<Self>> {
        // Get the list of the VM's memory devices.
        let donatable_regions = qmp::get_memdevs(conn)?
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
    #[allow(dead_code)]
    fn vm(&self) -> Arc<Vm> {
        self.vm
            .upgrade()
            .expect("DonorState outlived its parent Vm")
    }

    // Does the bookkeeping to convert a reserved allocation into a confirmed
    // allocation after getting ephemeral memory from the donor.
    // Returns a tuple of the actual size of the allocation and the amount of
    // ephemeral data that we just got from the donor that we need to return
    // immediately.
    fn handle_reserved_alloc(
        &self,
        qom_path: &str,
        alloc_id: u64,
        granted_size: u64,
    ) -> (u64, u64) {
        let mut donor_mut_state = self.mut_state.lock().unwrap();
        let Some((region, pos)) = donor_mut_state.find_alloc(qom_path, alloc_id) else {
            // We should never actually get here, since this function should
            // only be called if there is a reserved allocation.
            return (0, 0);
        };
        let alloc = region.allocations[pos].clone();

        if granted_size > alloc.initial_size {
            eprintln!(
                "Warning: Donor granted more memory than was requested. Requested {}, granted {}",
                alloc.initial_size, granted_size
            );
        }
        let granted_size = std::cmp::min(granted_size, alloc.initial_size);

        // Has memory been revoked before we had a chance to use it?
        let to_revoke = if region.revoked_owed > 0 {
            let to_revoke = std::cmp::min(region.revoked_owed, granted_size);
            region.revoked_owed -= to_revoke;
            to_revoke
        } else {
            0
        };

        let final_size = granted_size - to_revoke;
        if final_size == 0 {
            // If we don't actually have any memory to use, remove the
            // allocation from the list.
            region.allocations.remove(pos);
        }
        // Even though we incremented region.donated when we first
        // reserved the memory, we don't decrement it now if
        // actual_size is less than the reserved size. This is because
        // if the donor gives us less than we requested, it means that
        // the donor did not have enough free donatable memory to handle
        // our request. Additionally, when a donor uses its donatable
        // memory for itself, it will never again be available for
        // donation.
        (final_size, to_revoke)
    }

    fn allocate_eph_memory_inner(
        path_start: &Path,
        vms: &crate::VmList,
        rsvd_alloc: &Arc<EphAllocation>,
    ) -> Option<(Arc<Vm>, String)> {
        let vms = vms.read().unwrap();
        for (_path, vm) in vms.range::<Path, _>((Excluded(path_start), Unbounded)) {
            if let Some(donor_state) = &vm.donor {
                let mut donor_mut_state = donor_state.mut_state.lock().unwrap();
                for region in donor_mut_state.donatable_regions.iter_mut() {
                    let size = rsvd_alloc.initial_size;
                    if region.size - region.donated >= size {
                        // Allocate memory from this region
                        region.donated += size;
                        region.allocations.push(rsvd_alloc.clone());
                        return Some((vm.clone(), region.path.clone()));
                    }
                }
            }
        }
        None
    }

    // Takes a EphAllocation that has been reserved by a consumer, but not
    // yet committed, and actually allocates the memory from a donor.
    // Updates the passed in EphAllocation with the donor information.
    // Return true is successful and false otherwise.
    pub fn allocate_eph_memory(vms: &crate::VmList, rsvd_alloc: &Arc<EphAllocation>) -> bool {
        let mut last_path = PathBuf::new();
        let rsvd_size = rsvd_alloc.initial_size;
        let alloc_id = rsvd_alloc.id;
        loop {
            let Some((donor_vm, qom_path)) =
                Self::allocate_eph_memory_inner(&last_path, vms, rsvd_alloc)
            else {
                return false;
            };
            let donor_state = donor_vm.donor.as_ref().unwrap();

            // Send a QMP command requesting rsvd_size bytes from the donor.
            let Ok(size) =
                eph_mem_donate_capacity(&mut donor_vm.qmp.lock().unwrap(), &qom_path, rsvd_size)
                    .inspect_err(|e| eprintln!("QMP error while getting eph memory: {}", e))
            else {
                // If the QMP command fails, handle the failed allocation and
                // continue searching for another donor.
                // This will make it so this donor can no longer donate memory.
                // That's fine since the QMP connection is broken, don't waste
                // time trying to use it again.
                donor_state.handle_reserved_alloc(&qom_path, alloc_id, 0);
                last_path = donor_vm.qmp_socket_path.clone();
                continue;
            };

            // If the donor accepts the request, confirm the allocation and
            // return the size allocated. Ideally, we would keep trying other
            // donors until we found one that can satisfy our full demand, but
            // that will be for another time.
            let (granted_size, to_revoke) =
                donor_state.handle_reserved_alloc(&qom_path, alloc_id, size);

            // We may need to immediately return some of the memory we just got
            // if to_revoke is non-zero. This can happen if memory is revoked
            // between we were granted memory from the donor and the call to
            // handle_reserved_alloc().
            if to_revoke > 0 {
                donor_vm.return_eph_memory(&qom_path, to_revoke);
            }
            if granted_size != 0 {
                rsvd_alloc.commit_allocation(Arc::downgrade(&donor_vm), qom_path, granted_size);
                return true;
            }
            // Otherwise try the next donor in the list.
            last_path = donor_vm.qmp_socket_path.clone();
        }
    }

    pub fn bookkeep_returned_eph_memory(&self, donor_qom_path: &str, alloc_id: u64) {
        let mut donor_mut_state = self.mut_state.lock().unwrap();
        let Some((region, i)) = donor_mut_state.find_alloc(donor_qom_path, alloc_id) else {
            eprintln!(
                "Warning: Donor at {} is returning memory for an unknown allocation id {}",
                donor_qom_path, alloc_id
            );
            return;
        };
        let size = region.allocations[i].size();
        if region.donated < size {
            eprintln!(
                "Warning: Donor at {} is returning more memory than it has donated. Donated {}, returning {}",
                region.path, region.donated, size
            );
            region.donated = 0;
        } else {
            region.donated -= size;
        }
        region.allocations.remove(i);
    }
}

impl DonatableRegion {
    pub fn new(size: u64, path: String) -> Self {
        Self {
            size,
            donated: 0,
            revoked_owed: 0,
            path,
            allocations: Vec::new(),
        }
    }
}
