//! Helper functions for interacting with QEMU's QMP (QEMU Machine Protocol)
//! interface.
pub mod events;
mod types;

use serde::de::DeserializeOwned;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use types::{Memdev, QmpCommand};

pub struct QmpConnection {
    stream: UnixStream,
    response_rx: mpsc::Receiver<serde_json::Value>,
}

impl QmpConnection {
    /// Create a new QMP connection to the specified Unix socket.
    pub fn new(
        path: &Path,
        event_tx: mpsc::Sender<events::QmpEvent>,
    ) -> Result<Self, std::io::Error> {
        let stream = connect_with_retry(path)?;
        let response_rx = start_qmp_read_thread(&stream, path, event_tx)?;
        let mut conn = Self {
            stream,
            response_rx,
        };
        conn.initiate_connection()?;
        Ok(conn)
    }

    pub fn send(
        &mut self,
        command: &types::QmpCommand,
    ) -> Result<serde_json::Value, std::io::Error> {
        let command_str = serde_json::to_string(&command)?;
        self.send_str(&command_str)
    }

    fn send_str(&mut self, command: &str) -> Result<serde_json::Value, std::io::Error> {
        self.stream.write_all(command.as_bytes())?;
        if !command.ends_with('\n') {
            self.stream.write_all(b"\n")?;
        }

        let response = self
            .response_rx
            .recv()
            .map_err(|e| std::io::Error::other(format!("Failed to receive QMP response: {}", e)))?;

        Self::validate_qmp_response(response)
    }

    fn validate_qmp_response(
        response: serde_json::Value,
    ) -> Result<serde_json::Value, std::io::Error> {
        if let Some(error) = response.get("error") {
            Err(std::io::Error::other(format!(
                "QMP command failed: {}",
                error
            )))
        } else if response.get("return").is_some() {
            Ok(response)
        } else {
            // Not actually possible to get here since the qmp_read_thread will
            // only send responses that have either "return" or "error" fields.
            // Keep this branch here for completeness in case things change.
            Err(std::io::Error::other(format!(
                "Unexpected QMP response: {}",
                response
            )))
        }
    }

    /// Initiates a QMP connection with the QEMU instance by sending the
    /// `qmp_capabilities` command and checking the response.
    fn initiate_connection(&mut self) -> Result<(), std::io::Error> {
        let capabilities_command = QmpCommand::new("qmp_capabilities");
        self.send(&capabilities_command)?;
        Ok(())
    }

    fn qmp_call<T: DeserializeOwned>(&mut self, command: &QmpCommand) -> Result<T, std::io::Error> {
        let mut qmp_response = self.send(command)?;

        let return_value = qmp_response
            .get_mut("return")
            .map(std::mem::take)
            .expect("QmpConnection.send() guarantees \"return\" is present on Ok");
        let return_value: T = serde_json::from_value(return_value)?;
        Ok(return_value)
    }

    pub fn get_memdevs(&mut self) -> Result<Vec<Memdev>, std::io::Error> {
        let query_memdev_command = QmpCommand::new("query-memdev");
        self.qmp_call(&query_memdev_command)
    }

    pub fn qom_list(&mut self, path: &str) -> Result<Vec<types::QomListResponse>, std::io::Error> {
        let args = types::QomListArgs {
            path: path.to_string(),
        };
        let command = QmpCommand::with_arguments("qom-list", args)?;
        self.qmp_call(&command)
    }

    pub fn qom_get<T: serde::de::DeserializeOwned>(
        &mut self,
        path: &str,
        property: &str,
    ) -> Result<T, std::io::Error> {
        let args = types::QomGetArgs {
            path: path.to_string(),
            property: property.to_string(),
        };
        let command = QmpCommand::with_arguments("qom-get", args)?;
        self.qmp_call(&command)
    }

    pub fn eph_mem_donate_capacity(&mut self, qom_path: &str, size: u64) -> std::io::Result<u64> {
        let args = types::EphMemDonateCapacityData {
            path: qom_path.to_string(),
            size,
        };
        let command = QmpCommand::with_arguments("eph-mem-donate-capacity", args)?;
        let result: types::EphMemDonateResult = self.qmp_call(&command)?;
        Ok(result.granted)
    }

    pub fn eph_mem_return_capacity(&mut self, qom_path: &str, size: u64) -> std::io::Result<()> {
        let args = types::EphMemReturnCapacityData {
            path: qom_path.to_string(),
            size,
            id: None,
        };
        let command = QmpCommand::with_arguments("eph-mem-return-capacity", args)?;
        self.send(&command)?;
        Ok(())
    }

    pub fn cxl_add_dynamic_capacity(
        &mut self,
        qom_path: &str,
        offset: u64,
        len: u64,
    ) -> std::io::Result<()> {
        let args = types::CxlAddDynamicCapacityArgs {
            path: qom_path.to_string(),
            host_id: 0,
            selection_policy: types::CxlExtentSelectionPolicy::Prescriptive,
            region: 0,
            tag: None,
            extents: vec![types::CxlDynamicCapacityExtent { offset, len }],
        };
        let command = QmpCommand::with_arguments("cxl-add-dynamic-capacity", args)?;
        self.send(&command)?;
        Ok(())
    }
}

impl Drop for QmpConnection {
    fn drop(&mut self) {
        // Close the UnixStream when the QmpConnection is dropped
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Takes a new QMP connection and spawns a thread to handle reading from it.
/// Also takes a mpsc channel for sending QMP events to the event handling
/// thread.
/// Returns a channel for receiving responses to QMP commands using the
/// connection.
fn start_qmp_read_thread(
    qmp_stream: &UnixStream,
    path: &Path,
    event_tx: mpsc::Sender<events::QmpEvent>,
) -> Result<mpsc::Receiver<serde_json::Value>, std::io::Error> {
    let (response_tx, response_rx) = mpsc::channel();
    let thread_stream = qmp_stream.try_clone()?;
    let vm_path = path.to_path_buf();

    let _thread = thread::spawn(move || {
        qmp_read_thread(thread_stream, response_tx, event_tx, vm_path);
    });

    Ok(response_rx)
}

fn qmp_read_thread(
    qmp_stream: UnixStream,
    response_tx: mpsc::Sender<serde_json::Value>,
    event_tx: mpsc::Sender<events::QmpEvent>,
    vm_path: PathBuf,
) {
    let mut buf = BufReader::new(qmp_stream);
    let mut response_tx_dead = false;
    let mut event_tx_dead = false;
    let mut line = String::new();

    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break, // EOF reached
            Ok(_) => {
                let value: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(e) => {
                        eprintln!("{}: Failed to parse QMP response: {}", vm_path.display(), e);
                        break;
                    }
                };

                if value.get("return").is_some() || value.get("error").is_some() {
                    if response_tx_dead {
                        continue;
                    }
                    if let Err(e) = response_tx.send(value) {
                        eprintln!("{}: Failed to send QMP response: {}", vm_path.display(), e);
                        response_tx_dead = true;
                    }
                } else if value.get("event").is_some() {
                    if event_tx_dead {
                        continue;
                    }
                    let event = events::QmpEvent {
                        vm_path: vm_path.clone(),
                        json: value,
                    };
                    if let Err(e) = event_tx.send(event) {
                        eprintln!("{}: Failed to send QMP event: {}", vm_path.display(), e);
                        event_tx_dead = true;
                    }
                } else if value.get("QMP").is_some() {
                    // Ignore QMP greeting messages
                } else {
                    eprintln!(
                        "{}: Received unexpected QMP message: {:?}",
                        vm_path.display(),
                        value
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "{}: Failed to read from QMP stream: {}",
                    vm_path.display(),
                    e
                );
                break;
            }
        }

        // Nobody is listening on either channel, so we can exit the thread.
        if response_tx_dead && event_tx_dead {
            break;
        }
    }
    eprintln!("{}: Exiting QMP read thread", vm_path.display());
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
                    eprintln!(
                        "Failed to connect to QMP socket {:?}, retrying: {}",
                        path, e
                    );
                    std::thread::sleep(RETRY_DELAY);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}
