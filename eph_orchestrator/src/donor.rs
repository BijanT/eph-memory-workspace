//! Functions for interacting with DonorVMs

use std::sync::Mutex;

use crate::qmp;
use crate::qmp::QmpConnection;

#[allow(dead_code)]
pub struct DonorState {
    /* The donor's mutable state */
    mut_state: Mutex<DonorMutState>,
}

#[allow(dead_code)]
struct DonorMutState {
    /* The list of donatable regions available in the donor VM */
    donatable_regions: Vec<DonatableRegion>,
}

#[allow(dead_code)]
pub struct DonatableRegion {
    /* The total memory available in the memory region */
    size: u64,
    /* The amount of memory that has been donated from this region */
    donated: u64,
    /* The path to the QEMU memory backend device for this donatable region */
    path: String,
}

impl DonorState {
    pub fn new(conn: &mut QmpConnection) -> std::io::Result<Option<Self>> {
        // Get the list of the VM's memory devices.
        let donatable_regions = qmp::get_memdevs(conn)?
            .into_iter()
            .filter_map(|m| m.try_into().ok())
            .collect::<Vec<DonatableRegion>>();

        if donatable_regions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Self {
                mut_state: Mutex::new(DonorMutState { donatable_regions }),
            }))
        }
    }
}

impl DonatableRegion {
    pub fn new(size: u64, path: String) -> Self {
        Self {
            size,
            donated: 0,
            path,
        }
    }
}
