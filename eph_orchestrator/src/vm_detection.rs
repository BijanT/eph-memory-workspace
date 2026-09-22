//! Contains the vm_detection thread that watches for VMs to enter and leave
//! the system, and determines if new VMs are eligible to be donors.

use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use crate::{consumer, donor, qmp};
use notify::Watcher;

/// The main function for the vm_detection thread.
///
/// Watches the directory specified by `path` for QEMU QMP sockets entering
/// and leaving. A new QMP socket indicates a new VM has been created, and
/// this function will determine if that VM is a potential donor. A QMP
/// socket leaving indicates that a VM has been destroyed, and this function
/// will remove that VM from the list of VMs.
///
/// # Arguments
///
/// * `vms` - The list of all VMs that will be updated.
/// * `qmp_path` - The path to the directory to watch for QMP sockets.
/// * `event_tx` - The channel to send QMP events to the event handling thread.
pub fn vm_detection_thread(
    vms: &crate::VmList,
    qmp_path: &str,
    event_tx: mpsc::Sender<qmp::events::QmpEvent>,
) -> Result<(), std::io::Error> {
    // Create the directory if it doesn't exist so we can monitor it
    std::fs::create_dir_all(qmp_path)?;

    // Register the watch before the initial scan below, so there is no
    // window where a socket created between the two could be missed by both.
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default())
        .map_err(|e| std::io::Error::other(format!("Failed to create file watcher: {}", e)))?;
    let watch_path = Path::new(qmp_path);
    watcher
        .watch(watch_path, notify::RecursiveMode::NonRecursive)
        .map_err(|e| std::io::Error::other(format!("Failed to watch directory: {}", e)))?;

    // Now look to see if there are any QMP sockets that already existed
    // before the watch was registered.
    let existing_files = std::fs::read_dir(qmp_path)?;
    for entry in existing_files {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("Error reading directory entry: {}", e);
                continue;
            }
        };
        let path = entry.path();
        if let Err(e) = handle_new_file(vms, &path, &event_tx) {
            eprintln!("Error handling {:?}: {}", path, e);
        }
    }

    // Actual thread loop to handle the file create and remove events
    for res in rx {
        let event = match res {
            Ok(event) => event,
            Err(e) => {
                eprintln!("Error from file watcher: {}", e);
                continue;
            }
        };

        if event.kind.is_create() {
            for path in event.paths {
                if let Err(e) = handle_new_file(vms, &path, &event_tx) {
                    eprintln!("Error handling {:?}: {}", path, e);
                }
            }
        } else if event.kind.is_remove() {
            crate::remove_vms(vms, event.paths.as_slice());
        }
    }

    Ok(())
}

fn handle_new_file(
    vms: &crate::VmList,
    path: &Path,
    event_tx: &mpsc::Sender<qmp::events::QmpEvent>,
) -> Result<(), std::io::Error> {
    // We only care about unix domain sockets
    let metadata = path.metadata()?;
    let file_type = metadata.file_type();
    if !file_type.is_socket() {
        return Ok(());
    }

    // Already registered, e.g. seen by both the initial scan and a watch event.
    if vms
        .read()
        .unwrap()
        .iter()
        .any(|d| d.qmp_socket_path == path)
    {
        return Ok(());
    }

    let connection = qmp::QmpConnection::new(path, event_tx.clone())?;

    check_new_vm(vms, path, connection)?;

    Ok(())
}

// Add the newly detected VM to the VM list, setting its donor state if it has
// any donatable memory regions.
// Transfer ownership of the connection to this function, so it can be given to
// the Vm struct.
fn check_new_vm(
    vms: &crate::VmList,
    path: &Path,
    mut connection: qmp::QmpConnection,
) -> Result<(), std::io::Error> {
    let donatable_state = donor::DonorState::new(&mut connection)?;

    // If the VM has a DCD region, set its consumer state
    let consumer_state = consumer::ConsumerState::new(path, &mut connection)?;

    let new_vm = Arc::new(crate::Vm {
        qmp_socket_path: path.to_path_buf(),
        qmp: Mutex::new(connection),
        donor: donatable_state,
        consumer: consumer_state,
    });

    vms.write().unwrap().push(new_vm);

    Ok(())
}
