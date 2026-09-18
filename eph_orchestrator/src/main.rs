mod qmp;
mod vm_detection;

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;

#[allow(dead_code)]
const EPH_MEM_DONATION_GRANULARITY: u64 = 256 * 1024 * 1024; // 256 MiB
const QMP_DIRECTORY: &str = "/tmp/ephmem/";

#[allow(dead_code)]
struct DonorVM {
    /* The path to the QMP socket */
    qmp_socket_path: PathBuf,
    /* The Unix stream for communicating with the donor VM */
    connection: crate::qmp::QmpConnection,
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

fn main() {
    println!("Starting eph_orchestrator...");

    let donors = Mutex::new(Vec::new());

    thread::scope(|s| {
        s.spawn(|| {
            if let Err(e) = vm_detection::vm_detection_thread(&donors, QMP_DIRECTORY) {
                eprintln!("Error in VM detection thread: {}", e);
            }
        });
    });
}
