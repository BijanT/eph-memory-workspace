use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, Permissions};
use std::ops::Bound::{Excluded, Included};
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
    pending_release: bool,
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
    // The set of all allocations announced to the client.
    allocations: BTreeSet<EphAllocation>,
    // The set of allocations assigned to the client, but not sent to the client
    pending_allocations: BTreeSet<EphAllocation>,
    // The total amount of ephemeral memory requested by the client
    requested: u64,
    // The total amount of ephemeral memory that has been granted to the client
    granted: u64,
    // The amount of ephemeral memory given by the orchestrator, but has not yet
    // been sent to the client.
    pending: u64,
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

    /// Assign a pending grant to a client.
    ///
    /// `allocation` - The allocation to send to the client.
    fn assign_pending_grant(&self, allocation: EphAllocation) -> std::io::Result<()> {
        let mut mut_state = self.mut_state.lock().unwrap();

        // Keep track of the allocations received for this request.
        // The subtraction may underflow if a client has lowered its requested
        // amount, so saturate the subtraction at 0.
        let remaining = mut_state
            .requested
            .saturating_sub(mut_state.granted + mut_state.pending);
        if allocation.size > remaining {
            return Err(std::io::Error::other(format!(
                "Grant of {} bytes exceeds the {} bytes still outstanding for request ({} requested)",
                allocation.size, remaining, mut_state.requested
            )));
        }
        // Can't overflow: remaining >= allocation.size
        mut_state.pending += allocation.size;
        mut_state.pending_allocations.insert(allocation);

        Ok(())
    }

    // Send the pending grants of ephemeral memory to the client.
    fn send_eph_mem_grant(&self) -> std::io::Result<()> {
        let mut mut_state = self.mut_state.lock().unwrap();
        // No need to inform the client of allocations that are pending release.
        let (new_allocs, pending_release_allocs): (Vec<_>, Vec<_>) =
            std::mem::take(&mut mut_state.pending_allocations)
                .into_iter()
                .partition(|alloc| !alloc.pending_release);
        let pending = new_allocs.iter().map(|alloc| alloc.size).sum::<u64>();
        mut_state.granted += pending;
        mut_state.allocations.extend(new_allocs);
        // Keep the pending allocations so the DaxDevice drop handler can
        // delete the associated DAX device when the orchestrator releases it.
        // (TODO).
        mut_state.pending_allocations.extend(pending_release_allocs);
        mut_state.pending = mut_state
            .pending_allocations
            .iter()
            .map(|alloc| alloc.size)
            .sum::<u64>();
        // Drop the lock for the high latency IO
        drop(mut_state);

        // No need to send a message if there is no pending grant.
        if pending == 0 {
            return Ok(());
        }

        let resp = ConsumerCommand::new(
            ConsumerFunction::EphMemGrant,
            Some(pending),
            // The client doesn't need to know the DCD offset.
            None,
        );
        // TODO: We should return the allocation to the orchestrator if we
        // cannot send the grant to the client.
        self.connection.send(serde_json::to_value(&resp)?)
    }

    pub fn flush_pending_grants(clients: &ClientList) {
        // Copy the clients, so we can send grants without holding the lock.
        let clients_copy: Vec<(u32, Arc<Client>)> = clients
            .lock()
            .unwrap()
            .iter()
            .map(|(pid, client)| (*pid, client.clone()))
            .collect();
        for (pid, client) in clients_copy {
            // Don't let one client's failure stop the others from getting their grants.
            if let Err(e) = client.send_eph_mem_grant() {
                eprintln!("Failed to send grant to client {pid}: {e}");
            }
        }
    }

    /// Handle an EphMemGrant from the orchestrator.
    ///
    /// `clients` - The list of clients
    /// `size` - The size of the ephemeral memory allocated to this VM
    /// `offset` - The offset of the allocation in the VM's DCD.
    /// `dax_device` - The DAX device for the allocation.
    pub fn handle_eph_mem_grant(
        clients: &ClientList,
        size: u64,
        offset: u64,
        dax_device: DaxDevice,
    ) -> std::io::Result<()> {
        // Assign this grant to the client that has the smallest percent
        // of its request fulfilled.
        let mut found_client: Option<Arc<Client>> = None;
        let mut min_percent_fulfilled: u64 = 100;
        let clients_locked = clients.lock().unwrap();
        for client in clients_locked.values() {
            let mut_state = client.mut_state.lock().unwrap();
            let percent_fulfilled = ((mut_state.granted + mut_state.pending) * 100)
                .checked_div(mut_state.requested)
                .unwrap_or(100);
            let space_left = mut_state
                .requested
                .saturating_sub(mut_state.granted + mut_state.pending);
            if percent_fulfilled < min_percent_fulfilled && space_left >= size {
                min_percent_fulfilled = percent_fulfilled;
                found_client = Some(client.clone());
            }

            // We're not going to do better than 0%
            if percent_fulfilled == 0 {
                break;
            }
        }
        drop(clients_locked);

        // Assign the grant to the chosen client, but don't let the client know
        // until the orchestrator has finished sending all of the grants. The
        // caller flushes them with `flush_pending_grants()`.
        if let Some(client) = found_client {
            client.assign_pending_grant(EphAllocation {
                offset,
                size,
                dax_device,
                pending_release: false,
            })
        } else {
            // TODO: We need to add a path to return memory to the orchestrator
            // if no client is found.
            Err(std::io::Error::other(format!(
                "No suitable client found for grant with size {} and offset {}",
                size, offset
            )))
        }
    }

    pub fn handle_eph_mem_release(
        clients: &ClientList,
        size: u64,
        offset: u64,
    ) -> std::io::Result<()> {
        let clients_locked = clients.lock().unwrap();
        for client in clients_locked.values() {
            let remove_fn = |tree: &mut BTreeSet<EphAllocation>| {
                let mut to_remove = Vec::new();
                let mut total_removed = 0;
                for alloc in tree.range((Included(offset), Excluded(offset + size))) {
                    if alloc.offset >= offset && alloc.offset + alloc.size <= offset + size {
                        to_remove.push(alloc.offset);
                        total_removed += alloc.size;
                    }
                }
                for off in to_remove {
                    tree.remove(&off);
                }
                total_removed
            };
            let mut mut_state = client.mut_state.lock().unwrap();
            mut_state.pending -= remove_fn(&mut mut_state.pending_allocations);
            mut_state.granted -= remove_fn(&mut mut_state.allocations);
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
        let o1 = orchestrator.clone();
        let close_handler = move |ctype: ConnectionType, id: u64| {
            if let ConnectionType::Client(client_pid) = ctype {
                let mut clients = c1.lock().unwrap();
                // Make sure we're removing the correct client
                let is_current = clients
                    .get(&client_pid)
                    .is_some_and(|c| c.connection.id() == id);
                let client = if is_current {
                    clients.remove(&client_pid)
                } else {
                    None
                };
                drop(clients);

                if let Some(client) = client {
                    let _ = client.handle_eph_mem_request(&c1, &o1, 0);
                }
            }
        };
        let c2 = clients.clone();
        let o2 = orchestrator.clone();
        let msg_handler = move |ctype: ConnectionType, val: serde_json::Value| {
            if let ConnectionType::Client(_) = ctype {
                Client::msg_handler(c2.clone(), ctype, o2.clone(), val)
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
                pending_allocations: BTreeSet::new(),
                requested: 0,
                granted: 0,
                pending: 0,
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
                let clients_lock = clients.lock().unwrap();
                let Some(client) = clients_lock.get(&client_pid).cloned() else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Client with PID {client_pid} not found"),
                    ));
                };
                drop(clients_lock);
                let Some(amount) = cmd.size else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Missing size in EphMemRequest",
                    ));
                };
                let Some(amount) =
                    amount.checked_next_multiple_of(eph_proto::EPH_MEM_DONATION_GRANULARITY)
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "Requested size {} is too large to align to donation granularity {}",
                            amount,
                            eph_proto::EPH_MEM_DONATION_GRANULARITY
                        ),
                    ));
                };
                client.handle_eph_mem_request(&clients, &orchestrator, amount)
            }
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Received invalid command ({}) from client {client_pid}",
                    serde_json::to_string(&other).unwrap_or_else(|_| "<invalid>".to_string())
                ),
            )),
        }
    }

    fn handle_eph_mem_request(
        &self,
        clients: &ClientList,
        orchestrator: &Arc<Orchestrator>,
        amount: u64,
    ) -> std::io::Result<()> {
        let request_guard = orchestrator.lock_request();
        let clients = clients.lock().unwrap();
        let mut mut_state = self.mut_state.lock().unwrap();

        // Sum the requests of all of the other clients, plus this one's new
        // request. Another client's request is untrusted input, so don't let
        // the sum overflow.
        let Some(new_total_amount) = clients
            .values()
            .filter(|c| c.connection.id() != self.connection.id())
            .try_fold(amount, |total, c| {
                total.checked_add(c.mut_state.lock().unwrap().requested)
            })
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Total requested ephemeral memory is too large",
            ));
        };
        let old_requested_size = mut_state.requested;
        mut_state.requested = amount;
        //Drop the mut_state because we don't want to hold it while doing IO
        drop(mut_state);
        drop(clients);

        // Forward the ephemeral memory request to the orchestrator
        if let Err(e) = orchestrator.request_eph_mem(new_total_amount) {
            // The request failed for some reason, so revert the client's requested amount
            let mut mut_state = self.mut_state.lock().unwrap();
            mut_state.requested = old_requested_size;
            return Err(e);
        }
        // We normally don't hold locks across IO, but ephemeral memory requests
        // are not on the critical path, so it's fine in this case.
        drop(request_guard);

        // If the orchestrator sees a lower requested amount than before, it
        // won't revoke memory. It will just not grant more memory if the total
        // granted amount is already higher than the new requested amount.
        // Instead, we should tell the orchestrator which allocations we want to,
        // so it can release them.
        // We don't need to remove these allocations from the client state because
        // that will happen when the orchestrator revokes the memory.
        if amount < old_requested_size {
            let mut mut_state = self.mut_state.lock().unwrap();
            let mut to_revoke = Vec::new();
            let mut revoked_size = 0;
            let size_to_revoke = (mut_state.granted + mut_state.pending).saturating_sub(amount);
            let mut find_victims = |tree: &mut BTreeSet<EphAllocation>| {
                let mut removed_set = BTreeSet::new();
                // Assume the most recently allocated memory is the least important,
                // releasing it first.
                while let Some(mut alloc) = tree.pop_last() {
                    if revoked_size >= size_to_revoke {
                        removed_set.insert(alloc);
                        break;
                    }
                    if alloc.pending_release {
                        // Allocations that are already pending release will be
                        // eventually removed, so we will count them towards the
                        // amount to be revoked. Technically, it would be better
                        // to sum up all of the pending releases first, but this
                        // lazy is good enough.
                        revoked_size += alloc.size;
                        removed_set.insert(alloc);
                        continue;
                    }
                    alloc.pending_release = true;
                    to_revoke.push((alloc.size, alloc.offset));
                    revoked_size += alloc.size;
                    removed_set.insert(alloc);
                }

                tree.extend(removed_set);
            };
            // Look for victims in the pending set first, since those haven't
            // been sent to the client yet.
            find_victims(&mut mut_state.pending_allocations);
            find_victims(&mut mut_state.allocations);
            drop(mut_state);

            for (size, offset) in to_revoke {
                orchestrator.release_eph_mem(size, offset)?;
            }
        }

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
