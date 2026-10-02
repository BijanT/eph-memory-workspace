use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::thread;

use vsock::VsockStream;

pub trait TryClone: Sized {
    fn try_clone(&self) -> std::io::Result<Self>;
}
impl TryClone for UnixStream {
    fn try_clone(&self) -> std::io::Result<Self> {
        UnixStream::try_clone(self)
    }
}
impl TryClone for VsockStream {
    fn try_clone(&self) -> std::io::Result<Self> {
        VsockStream::try_clone(self)
    }
}

pub trait Shutdown {
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()>;
}
impl Shutdown for UnixStream {
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}
impl Shutdown for VsockStream {
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        VsockStream::shutdown(self, how)
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ConnectionType {
    // The connection is to the orchestrator
    Orchestrator,
    // The connection is to a client application at the specified PID
    Client(u32),
}

impl std::fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionType::Orchestrator => write!(f, "Orchestrator"),
            ConnectionType::Client(pid) => write!(f, "Client({})", pid),
        }
    }
}

pub trait ConnectionTrait: Read + Write + TryClone + Shutdown + Send + 'static {}
impl<T: Read + Write + TryClone + Shutdown + Send + 'static> ConnectionTrait for T {}
#[allow(dead_code)]
pub struct Connection<T: ConnectionTrait> {
    // The type of the connection (Orchestrator or Client)
    ctype: ConnectionType,
    // The stream for writing to the connection
    write_stream: Mutex<T>,
    // The stream for shutting down the connection. A copy of the write stream
    // but available without having to acquire the lock.
    shutdown_stream: T,
    // Unique ID for the connection
    id: u64,
}

impl<T: ConnectionTrait> Drop for Connection<T> {
    fn drop(&mut self) {
        let _ = self.shutdown_stream.shutdown(std::net::Shutdown::Both);
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl<T: ConnectionTrait> Connection<T> {
    /// Create a new connection.
    ///
    /// * `ctype` - The type of the connection
    /// * `stream` - The underlying stream
    /// * `msg_handler` - A function to handle incoming messages
    /// * `close_handler` - A function to handle connection closure
    pub fn new<F, G>(
        ctype: ConnectionType,
        stream: T,
        msg_handler: F,
        close_handler: G,
    ) -> std::io::Result<Self>
    where
        F: FnMut(ConnectionType, serde_json::Value) -> std::io::Result<()> + Send + 'static,
        G: FnMut(ConnectionType, u64) + Send + 'static,
    {
        let read_stream = stream.try_clone()?;
        let shutdown_stream = stream.try_clone()?;

        let ret = Self {
            ctype,
            write_stream: Mutex::new(stream),
            shutdown_stream,
            id: ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        };

        thread::spawn(move || {
            if let Err(e) =
                Self::read_thread::<F, G>(read_stream, ctype, ret.id, msg_handler, close_handler)
            {
                eprintln!("Error reading from connection: {}", e);
            }
        });

        Ok(ret)
    }

    fn read_thread<F, G>(
        stream: T,
        ctype: ConnectionType,
        id: u64,
        msg_handler: F,
        mut close_handler: G,
    ) -> std::io::Result<()>
    where
        F: FnMut(ConnectionType, serde_json::Value) -> std::io::Result<()> + Send + 'static,
        G: FnMut(ConnectionType, u64) + Send + 'static,
    {
        let result = Self::read_loop::<F>(stream, ctype, msg_handler);
        close_handler(ctype, id);
        result
    }

    fn read_loop<F>(stream: T, ctype: ConnectionType, mut msg_handler: F) -> std::io::Result<()>
    where
        F: FnMut(ConnectionType, serde_json::Value) -> std::io::Result<()> + Send + 'static,
    {
        const MAX_LINE_LENGTH: usize = 4096;
        let mut line = String::new();
        let mut buf = BufReader::new(stream);

        loop {
            line.clear();
            let mut limited = (&mut buf).take(MAX_LINE_LENGTH as u64);
            match limited.read_line(&mut line) {
                Ok(0) => break, // EOF reached
                Ok(_) if line.ends_with('\n') => {
                    // Skip empty lines
                    if line.trim().is_empty() {
                        continue;
                    }
                    // Parse the JSON to pass to the handler. An error indicates
                    // a poorly behaving client, so stop listening to it and
                    // return an error.
                    let json: serde_json::Value = serde_json::from_str(&line).map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Invalid JSON from {}: {}", ctype, e),
                        )
                    })?;
                    if let Err(e) = msg_handler(ctype, json) {
                        eprintln!("Error handling message from {}: {}", ctype, e);
                    }
                }
                Ok(len) => {
                    let e = if len == MAX_LINE_LENGTH {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Message from {} exceeds maximum length of {}",
                                ctype, MAX_LINE_LENGTH
                            ),
                        )
                    } else {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Connection closed before end of message from {} ({} bytes read)",
                                ctype, len
                            ),
                        )
                    };
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}
