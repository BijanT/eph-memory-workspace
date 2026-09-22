mod qmp;
mod vm_detection;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;

#[allow(dead_code)]
const EPH_MEM_DONATION_GRANULARITY: u64 = 256 * 1024 * 1024; // 256 MiB
const QMP_DIRECTORY: &str = "/tmp/ephmem/";

#[allow(dead_code)]
struct Vm {
    /* The immutable path to the QMP socket */
    qmp_socket_path: PathBuf,
    /* The connection to the QMP socket */
    qmp: Mutex<qmp::QmpConnection>,
    /* Internal state for DonorVMs */
    donor: Option<DonorState>,
}

#[allow(dead_code)]
struct DonorState {
    /* The donor's mutable state */
    mut_state: Mutex<DonorMutState>,
}

#[allow(dead_code)]
struct DonorMutState {
    /* The list of donatable regions available in the donor VM */
    donatable_regions: Vec<DonatableRegion>,
}

#[allow(dead_code)]
struct DonatableRegion {
    /* The total memory available in the memory region */
    size: u64,
    /* The amount of memory that has been donated from this region */
    donated: u64,
    /* The path to the QEMU memory backend device for this donatable region */
    path: String,
}

type VmList = RwLock<Vec<Arc<Vm>>>;

fn remove_vms(vms: &VmList, paths: &[PathBuf]) {
    let mut vms = vms.write().unwrap();
    vms.retain(|vm| !paths.contains(&vm.qmp_socket_path));
}

fn main() {
    println!("Starting eph_orchestrator...");

    let vms = RwLock::new(Vec::new());

    thread::scope(|s| {
        let (event_tx, event_rx) = mpsc::channel();
        s.spawn(|| {
            if let Err(e) = vm_detection::vm_detection_thread(&vms, QMP_DIRECTORY, event_tx) {
                eprintln!("Error in VM detection thread: {}", e);
            }
        });
        s.spawn(|| {
            if let Err(e) = qmp::events::qmp_event_handler_thread(&vms, event_rx) {
                eprintln!("Error in QMP event handler thread: {}", e);
            }
        });
    });
}
