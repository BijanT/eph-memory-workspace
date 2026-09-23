mod consumer;
mod donor;
mod qmp;
mod vm_detection;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;

use consumer::ConsumerState;
use donor::DonorState;

#[allow(dead_code)]
const EPH_MEM_DONATION_GRANULARITY: u64 = 256 * 1024 * 1024; // 256 MiB
const QMP_DIRECTORY: &str = "/tmp/ephmem/";
const VSOCK_PORT: u32 = 1848;

#[allow(dead_code)]
struct Vm {
    /* The immutable path to the QMP socket */
    qmp_socket_path: PathBuf,
    /* The connection to the QMP socket */
    qmp: Mutex<qmp::QmpConnection>,
    /* Internal state for DonorVMs */
    donor: Option<DonorState>,
    /* Internal state for ConsumerVMs */
    consumer: Option<ConsumerState>,
}

type VmList = RwLock<BTreeMap<PathBuf, Arc<Vm>>>;

fn remove_vms(vms: &VmList, paths: &[PathBuf]) {
    let mut vms = vms.write().unwrap();
    for p in paths {
        vms.remove(p);
    }
}

fn main() {
    println!("Starting eph_orchestrator...");

    let vms = Arc::new(RwLock::new(BTreeMap::new()));

    thread::scope(|s| {
        let (event_tx, event_rx) = mpsc::channel();

        let vm_list = vms.clone();
        s.spawn(move || {
            if let Err(e) = vm_detection::vm_detection_thread(&vm_list, QMP_DIRECTORY, event_tx) {
                eprintln!("Error in VM detection thread: {}", e);
            }
        });

        let vm_list = vms.clone();
        s.spawn(move || {
            if let Err(e) = qmp::events::qmp_event_handler_thread(&vm_list, event_rx) {
                eprintln!("Error in QMP event handler thread: {}", e);
            }
        });

        let vm_list = vms.clone();
        s.spawn(move || {
            if let Err(e) = consumer::consumer_listener_thread(vm_list, VSOCK_PORT, QMP_DIRECTORY) {
                eprintln!("Error in consumer listener thread: {}", e);
            }
        });
    });
}
