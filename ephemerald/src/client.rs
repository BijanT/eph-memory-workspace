use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, Permissions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::connection::{Connection, ConnectionType};
use crate::dcd::DaxDevice;
use crate::orchestrator::Orchestrator;
use eph_proto::{ConsumerCommand, ConsumerFunction};

#[allow(dead_code)]
struct EphAllocation {
    offset: u64,
    size: u64,
    dax_device: DaxDevice,
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

pub struct Client {
    // The connection to the client
    connection: Connection<UnixStream>,
    // The mutable state for this client
    mut_state: Mutex<ClientMutState>,
}

#[allow(dead_code)]
pub struct ClientMutState {
    // A set of all allocations for this client
    allocations: BTreeSet<EphAllocation>,
    // The size of the pending request if there is one
    pending_request: Option<u64>,
}

pub type ClientList = Arc<Mutex<BTreeMap<u32, Arc<Client>>>>;
impl Client {
    /// Thread that waits for incoming client connections.
    /// Spawns a new client thread for each connection.
    ///
    /// * `clients` - A list of all connected clients
    pub fn listener_thread(
        clients: ClientList,
        orchestrator: Arc<Orchestrator>,
    ) -> std::io::Result<()> {
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
            if let Err(e) = Client::register_client(&clients, &orchestrator, stream) {
                eprintln!("Error registering client: {}", e);
            }
        }
    }

    /// Forward an EphMemResponse to the client
    ///
    /// `allocation` - The allocation to send to the client. If None, the client
    /// will be sent a response with size 0, indicating that the request was
    /// denied.
    fn send_eph_mem_response(&self, allocation: Option<EphAllocation>) -> std::io::Result<()> {
        let size = allocation.as_ref().map_or(0, |a| a.size);
        let mut mut_state = self.mut_state.lock().unwrap();
        if mut_state.pending_request.is_none() {
            return Err(std::io::Error::other("No pending request to respond to"));
        }
        mut_state.pending_request = None;
        if let Some(allocation) = allocation {
            mut_state.allocations.insert(allocation);
        }
        // Drop the lock for the high latency IO
        drop(mut_state);

        let resp = ConsumerCommand {
            function: ConsumerFunction::EphMemResponse,
            size: Some(size),
            // The client doesn't need to know the DCD offset.
            offset: None,
        };
        // TODO: We should return the allocation to the orchestrator if we
        // cannot send the response to the client.
        self.connection.send(serde_json::to_value(&resp)?)
    }

    /// Handle an EphMemResponse from the orchestrator.
    ///
    /// `clients` - The list of clients
    /// `size` - The size of the ephemeral memory allocated to this VM
    /// `offset` - The offset of the allocation in the VM's DCD.
    /// `dax_device` - The DAX device for the allocation.
    pub fn handle_eph_mem_response(
        clients: &ClientList,
        size: u64,
        offset: u64,
        dax_device: DaxDevice,
    ) -> std::io::Result<()> {
        // Find the client with the closest fitting pending request.
        let mut best_match_client: Option<Arc<Client>> = None;
        let mut best_match_size_diff: u64 = u64::MAX;
        let clients_locked = clients.lock().unwrap();
        for client in clients_locked.values() {
            let mut_state = client.mut_state.lock().unwrap();
            if let Some(pending_size) = mut_state.pending_request {
                // The response to a request will be no greater than the
                // requested size, so only consider clients with pending
                // requests greater than or equal to the size of the response.
                if pending_size >= size {
                    let size_diff = pending_size - size;
                    if size_diff < best_match_size_diff {
                        best_match_client = Some(client.clone());
                        best_match_size_diff = size_diff;
                    }
                    // If we find a perfect match, we can stop searching.
                    if size_diff == 0 {
                        break;
                    }
                }
            }
        }
        drop(clients_locked);

        // It looks like there could be a race condition here where the
        // orchestrator sends two responses in parallel, and the same client
        // gets chosen for each because we drop the client lock before clearing
        // its pending_size in send_eph_mem_response.
        // This cannot happen because handling of messages from the orchestrator
        // is serialized by the orchestrator's msg_handler.
        if let Some(client) = best_match_client {
            return client.send_eph_mem_response(Some(EphAllocation {
                offset,
                size,
                dax_device,
            }));
        } else {
            // TODO: We need to add a path to return memory to the orchestrator
            // if no client is found.
            eprintln!(
                "No client found with a pending request for size {} (offset {})",
                size, offset
            );
        }
        Ok(())
    }

    fn register_client(
        clients: &ClientList,
        orchestrator: &Arc<Orchestrator>,
        stream: UnixStream,
    ) -> std::io::Result<()> {
        let pid = get_stream_pid(&stream)?;
        // Take the clients lock before creating the client to defend against
        // a race where the client connection ends the close handler is called
        // before the client is added to the list. Taking the lock here ensures
        // that the close handler is called only after the client is added to
        // the list.
        let mut clients_locked = clients.lock().unwrap();
        let client = Client::new(stream, pid, clients, orchestrator)?;
        // We only want one connection per client, so we replace any existing
        // client
        clients_locked.insert(pid, Arc::new(client));
        Ok(())
    }

    fn new(
        stream: UnixStream,
        pid: u32,
        clients: &ClientList,
        orchestrator: &Arc<Orchestrator>,
    ) -> std::io::Result<Self> {
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
        let o = orchestrator.clone();
        let msg_handler = move |ctype: ConnectionType, val: serde_json::Value| {
            if let ConnectionType::Client(_) = ctype {
                Client::msg_handler(c2.clone(), ctype, o.clone(), val)
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
            mut_state: Mutex::new(ClientMutState {
                allocations: BTreeSet::new(),
                pending_request: None,
            }),
        })
    }

    fn msg_handler(
        clients: ClientList,
        conn: ConnectionType,
        orchestrator: Arc<Orchestrator>,
        val: serde_json::Value,
    ) -> std::io::Result<()> {
        let cmd = serde_json::from_value::<ConsumerCommand>(val)?;
        let client_pid = match conn {
            ConnectionType::Client(pid) => pid,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid connection type",
                ));
            }
        };

        match cmd.function {
            ConsumerFunction::EphMemRequest => {
                let Some(client) = clients.lock().unwrap().get(&client_pid).cloned() else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Client with PID {client_pid} not found"),
                    ));
                };
                let Some(amount) = cmd.size else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Missing size in EphMemRequest",
                    ));
                };
                Self::handle_eph_mem_request(client, &orchestrator, amount)
            }
            ConsumerFunction::EphMemResponse => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Received EphMemResponse command from client {client_pid}"),
            )),
        }
    }

    fn handle_eph_mem_request(
        client: Arc<Client>,
        orchestrator: &Arc<Orchestrator>,
        amount: u64,
    ) -> std::io::Result<()> {
        let mut mut_state = client.mut_state.lock().unwrap();
        // Don't accept another reservation from this client if they already
        // have a pending request
        if mut_state.pending_request.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Client already has a pending ephemeral memory request",
            ));
        }

        mut_state.pending_request = Some(amount);
        //Drop the mut_state because we don't want to hold it while doing IO
        drop(mut_state);

        // Forward the ephemeral memory request to the orchestrator
        let result = orchestrator.request_eph_mem(amount);

        // If the request failed, send the response to the client, which will
        // also unset pending_request.
        // There is potentially a race condition where another request sets
        // pending request before we unset it. We are protected from this
        // for two reasons:
        // 1. pending_request will not be set again when already set
        // 2. Consumer requests are serialized by msg_handler
        if result.is_err()
            && let Err(e) = client.send_eph_mem_response(None)
        {
            eprintln!("Failed to send EphMemResponse to client: {}", e);
        }
        result
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
