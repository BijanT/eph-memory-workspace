//! Contains the vm_detection thread that watches for VMs to enter and leave
//! the system, and determines if new VMs are eligible to be donors.

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::vec::Vec;

use notify::Watcher;

/// The main function for the vm_detection thread.
///
/// Watches the directory specified by `path` for QEMU QMP sockets entering
/// and leaving. A new QMP socket indicates a new VM has been created, and
/// this function will determine if that VM is a potential donor. A QMP
/// socket leaving indicates that a VM has been destroyed, and this function
/// will remove that VM from the list of potential donors.
///
/// # Arguments
///
/// * `donors` - The list of potential donor VMs that will be updated.
/// * `qmp_path` - The path to the directory to watch for QMP sockets.
pub fn vm_detection_thread(
    donors: &Mutex<Vec<crate::DonorVM>>,
    qmp_path: &str,
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
        if let Err(e) = handle_new_file(donors, &path) {
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
                if let Err(e) = handle_new_file(donors, &path) {
                    eprintln!("Error handling {:?}: {}", path, e);
                }
            }
        } else if event.kind.is_remove() {
            let mut donors = donors.lock().unwrap();
            for path in event.paths {
                // Remove the donor VM from the list of donors if it exists.
                donors.retain(|donor| donor.qmp_socket_path != path);
            }
        }
    }

    Ok(())
}

// Connect to the QMP socket, retrying briefly to cover the race where the
// socket file has been created but the peer hasn't called listen() yet.
fn connect_with_retry(path: &Path) -> std::io::Result<UnixStream> {
    const MAX_ATTEMPTS: u32 = 5;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match UnixStream::connect(path) {
            Ok(socket) => return Ok(socket),
            Err(e) => {
                let attempts_remaining = attempt + 1 < MAX_ATTEMPTS;
                if attempts_remaining {
                    eprintln!("Failed to connect to QMP socket {:?}, retrying: {}", path, e);
                    std::thread::sleep(RETRY_DELAY);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}

fn handle_new_file(
    donors: &Mutex<Vec<crate::DonorVM>>, path: &Path
) -> Result<(), std::io::Error> {
    // We only care about unix domain sockets
    let metadata = path.metadata()?;
    let file_type = metadata.file_type();
    if !file_type.is_socket() {
        return Ok(());
    }

    // Already registered, e.g. seen by both the initial scan and a watch event.
    if donors
        .lock()
        .unwrap()
        .iter()
        .any(|d| d.qmp_socket_path == path)
    {
        return Ok(());
    }

    let socket = connect_with_retry(path)?;
    let mut connection = crate::LineStream::new(socket);

    // Initialize the QMP connection
    crate::qmp::initiate_connection(&mut connection)?;

    check_new_vm(donors, path, connection)?;

    Ok(())
}

// Check if a newly identified VM is eligible to be a donor. If it is, add it
// to the list of donors.
// Transfer ownership of the connection to this function, so it can be given to
// the DonorVM struct if the VM is eligible to be a donor.
fn check_new_vm(
    donors: &Mutex<Vec<crate::DonorVM>>,
    path: &Path,
    mut connection: crate::LineStream<UnixStream>,
) -> Result<(), std::io::Error> {
    // Get the list of the VM's memory devices.
    let donatable_regions = crate::qmp::get_memdevs(&mut connection)?
        .into_iter()
        .filter_map(|m| m.try_into().ok())
        .collect::<Vec<crate::DonatableRegion>>();

    // If the VM has at least one donatable region, add it to the list of donors.
    if !donatable_regions.is_empty() {
        let donor_vm = crate::DonorVM {
            qmp_socket_path: path.to_path_buf(),
            stream: connection,
            donatable_regions,
        };
        donors.lock().unwrap().push(donor_vm);
    }

    Ok(())
}
