use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs::{self, Permissions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::connection::{Connection, ConnectionType};

#[allow(dead_code)]
struct EphAllocation {
    offset: u64,
    size: u64,
}

impl std::borrow::Borrow<u64> for EphAllocation {
    fn borrow(&self) -> &u64 {
        &self.offset
    }
}

impl Ord for EphAllocation {
    fn cmp(&self, other: &Self) -> Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for EphAllocation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EphAllocation {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}

impl Eq for EphAllocation {}

#[allow(dead_code)]
pub struct Client {
    // The connection to the client
    connection: Connection<UnixStream>,
    // The PID of the client
    pid: u32,
    // A set of all allocations for this client
    allocations: BTreeSet<EphAllocation>,
}

impl std::borrow::Borrow<u32> for Client {
    fn borrow(&self) -> &u32 {
        &self.pid
    }
}

impl Ord for Client {
    fn cmp(&self, other: &Self) -> Ordering {
        self.pid.cmp(&other.pid)
    }
}

impl PartialOrd for Client {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Client {
    fn eq(&self, other: &Self) -> bool {
        self.pid == other.pid
    }
}

impl Eq for Client {}

pub type ClientList = Arc<Mutex<BTreeSet<Client>>>;
impl Client {
    /// Thread that waits for incoming client connections.
    /// Spawns a new client thread for each connection.
    ///
    /// * `clients` - A list of all connected clients
    pub fn listener_thread(clients: ClientList) -> std::io::Result<()> {
        const MAX_CLIENTS: u64 = 64;
        const SOCKET_PATH: &str = "/run/ephemerald.sock";
        // A socket file left behind by a previous run (e.g. after a crash)
        // would make bind() fail with AddrInUse, so remove it first. This
        // assumes only one ephemerald instance runs at a time.
        match fs::remove_file(SOCKET_PATH) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(SOCKET_PATH)?;
        // Connecting requires write permission on the socket, and the file's
        // initial mode depends on the umask. Any process may connect.
        fs::set_permissions(SOCKET_PATH, Permissions::from_mode(0o666))?;
        loop {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("Error accepting connection: {}", e);
                    thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
            };
            if clients.lock().unwrap().len() >= MAX_CLIENTS as usize {
                eprintln!("Maximum number of clients reached");
                continue;
            }
            if let Err(e) = Client::register_client(&clients, stream) {
                eprintln!("Error registering client: {}", e);
            }
        }
    }

    fn register_client(clients: &ClientList, stream: UnixStream) -> std::io::Result<()> {
        let pid = get_stream_pid(&stream)?;
        // Take the clients lock before creating the client to defend against
        // a race where the client connection ends the close handler is called
        // before the client is added to the list. Taking the lock here ensures
        // that the close handler is called only after the client is added to
        // the list.
        let mut clients_locked = clients.lock().unwrap();
        let client = Client::new(stream, pid, clients)?;
        // We only want one connection per client, so we replace any existing
        // client
        clients_locked.replace(client);
        Ok(())
    }

    fn new(stream: UnixStream, pid: u32, clients: &ClientList) -> std::io::Result<Self> {
        let c1 = clients.clone();
        let close_handler = move |ctype: ConnectionType, id: u64| {
            if let ConnectionType::Client(client_pid) = ctype {
                let mut clients = c1.lock().unwrap();
                // Make sure we're removing the correct client
                let is_current = clients
                    .get(&client_pid)
                    .is_some_and(|c| c.connection.id() == id);
                if is_current {
                    clients.remove(&client_pid);
                }
            }
        };
        let c2 = clients.clone();
        let msg_handler = move |ctype: ConnectionType, val: serde_json::Value| {
            if let ConnectionType::Client(_) = ctype {
                Client::msg_handler(c2.clone(), ctype, val)
            } else {
                // This should never happen by construction. Keep this branch
                // to keep the compiler happy.
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid connection type",
                ))
            }
        };
        let connection = Connection::new(
            ConnectionType::Client(pid),
            stream,
            msg_handler,
            close_handler,
        )?;
        Ok(Self {
            connection,
            pid,
            allocations: BTreeSet::new(),
        })
    }

    fn msg_handler(
        _clients: ClientList,
        _conn: ConnectionType,
        _val: serde_json::Value,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

fn get_stream_pid(stream: &UnixStream) -> std::io::Result<u32> {
    let fd = stream.as_raw_fd();
    let level = libc::SOL_SOCKET;
    let opt = libc::SO_PEERCRED;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut optlen = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    let result = unsafe {
        libc::getsockopt(
            fd,
            level,
            opt,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut optlen,
        )
    };

    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Make sure we got the right amount of data
    if optlen != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid socket option data",
        ));
    }

    // Guard against invalid PIDs
    if cred.pid <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid client PID of {}", cred.pid),
        ));
    }

    Ok(cred.pid as u32)
}
