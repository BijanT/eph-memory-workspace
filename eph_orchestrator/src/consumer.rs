//! Helper functions for interacting with Consumer VMs.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{Vm, qmp};
use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener, VsockStream};

#[allow(dead_code)]
pub struct ConsumerState {
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
    allocated_areas: BTreeMap<u64, DcdAllocation>,
}

#[allow(dead_code)]
struct DcdAllocation {
    /* size of the allocation */
    size: u64,
    /* the donor that supplied the memory for this allocation */
    donor_qmp_path: PathBuf,
}

impl ConsumerState {
    pub fn new(qmp_path: &Path, conn: &mut qmp::QmpConnection) -> std::io::Result<Option<Self>> {
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
}

/// Thread that waits for consumer VMs to connect for the first time.
/// Spawns a new thread for each consumer VM that connects.
///
/// * `vms` - The list of VMs that have connected.
/// * `listening_port` - The port to listen on for new consumer VM connections.
pub fn consumer_listener_thread(
    vms: Arc<crate::VmList>,
    listening_port: u32,
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

        // Find the VM corresponding to the CID of the connecting consumer VM.
        let Some(consumer) = vms
            .read()
            .unwrap()
            .iter()
            .find(|vm| {
                if let Some(consumer_state) = &vm.consumer {
                    consumer_state.vsock_cid == addr.cid()
                } else {
                    false
                }
            })
            .cloned()
        else {
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
    _vms: Arc<crate::VmList>,
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
                println!(
                    "Received message from consumer VM at '{}': {}",
                    consumer.qmp_socket_path.display(),
                    line.trim()
                );
                // Here you would parse the message and handle it accordingly.
                // For now, we just print it.
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

pub fn get_consumer_state(
    qmp_path: &Path,
    conn: &mut qmp::QmpConnection,
) -> std::io::Result<Option<ConsumerState>> {
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

            let consumer_state = ConsumerState {
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
