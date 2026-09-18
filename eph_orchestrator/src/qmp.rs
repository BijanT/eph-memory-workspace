//! Helper functions for interacting with QEMU's QMP (QEMU Machine Protocol)
//! interface.
mod types;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use types::Memdev;

pub struct QmpConnection {
    stream: UnixStream,
    response_rx: mpsc::Receiver<serde_json::Value>,
}

impl QmpConnection {
    pub fn new(stream: UnixStream, response_rx: mpsc::Receiver<serde_json::Value>) -> Self {
        Self {
            stream,
            response_rx,
        }
    }

    pub fn send(&mut self, command: &str) -> Result<serde_json::Value, std::io::Error> {
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
}

impl Drop for QmpConnection {
    fn drop(&mut self) {
        // Close the UnixStream when the QmpConnection is dropped
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Takes a new QMP connection and spawns a thread to handle reading from it.
/// Returns two channels: The first is for receiving responses to QMP commands
/// using the connection, and the second is for receiving QMP events that are
/// sent asynchronously by the QEMU instance.
pub fn start_qmp_read_thread(
    qmp_stream: &UnixStream,
    path: &std::path::Path,
) -> Result<
    (
        mpsc::Receiver<serde_json::Value>,
        mpsc::Receiver<serde_json::Value>,
    ),
    std::io::Error,
> {
    let (response_tx, response_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let thread_stream = qmp_stream.try_clone()?;
    let path_string: String = path.to_string_lossy().into_owned();

    let _thread = thread::spawn(move || {
        qmp_read_thread(thread_stream, response_tx, event_tx, path_string);
    });

    Ok((response_rx, event_rx))
}

fn qmp_read_thread(
    qmp_stream: UnixStream,
    response_tx: mpsc::Sender<serde_json::Value>,
    event_tx: mpsc::Sender<serde_json::Value>,
    path_string: String,
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
                        eprintln!("{}: Failed to parse QMP response: {}", path_string, e);
                        break;
                    }
                };

                if value.get("return").is_some() || value.get("error").is_some() {
                    if let Err(e) = response_tx.send(value)
                        && !response_tx_dead
                    {
                        eprintln!("{}: Failed to send QMP response: {}", path_string, e);
                        response_tx_dead = true;
                    }
                } else if value.get("event").is_some() {
                    if let Err(e) = event_tx.send(value)
                        && !event_tx_dead
                    {
                        eprintln!("{}: Failed to send QMP event: {}", path_string, e);
                        event_tx_dead = true;
                    }
                } else if value.get("QMP").is_some() {
                    // Ignore QMP greeting messages
                } else {
                    eprintln!(
                        "{}: Received unexpected QMP message: {:?}",
                        path_string, value
                    );
                }
            }
            Err(e) => {
                eprintln!("{}: Failed to read from QMP stream: {}", path_string, e);
                break;
            }
        }

        // Nobody is listening on either channel, so we can exit the thread.
        if response_tx_dead && event_tx_dead {
            break;
        }
    }
    eprintln!("{}: Exiting QMP read thread", path_string);
}

/// Initiates a QMP connection with the QEMU instance by sending the
/// `qmp_capabilities` command and checking the response.
pub fn initiate_connection(connection: &mut QmpConnection) -> Result<(), std::io::Error> {
    // Send the QMP capabilities command to the QEMU instance
    let capabilities_command = r#"{"execute": "qmp_capabilities"}"#;
    connection.send(capabilities_command)?;

    Ok(())
}

pub fn get_memdevs(connection: &mut QmpConnection) -> Result<Vec<Memdev>, std::io::Error> {
    // Send the query-memdev command to the QEMU instance
    let query_memdev_command = r#"{"execute": "query-memdev"}"#;
    let mut qmp_response = connection.send(query_memdev_command)?;

    let memdevs = qmp_response
        .get_mut("return")
        .map(std::mem::take)
        .expect("QmpConnection.send() guarantees \"return\" is present on Ok");
    let memdevs: Vec<Memdev> = serde_json::from_value(memdevs)?;
    Ok(memdevs)
}
