use std::sync::Mutex;

use crate::client::{Client, ClientList};
use crate::connection::{Connection, ConnectionType};
use crate::dcd::DaxDevice;
use eph_proto::{ConsumerCommand, ConsumerFunction};
use vsock::VsockStream;

#[allow(dead_code)]
pub struct Orchestrator {
    connection: Connection<VsockStream>,
    // Mutex to synchronize sending ephemeral memory requests to the
    // orchestrator. This is used to guard against a race where:
    // 1) Client A updates its request and calculates a new total requested
    // amount for this VM, but does not send it to the orchestrator yet.
    // 2) Client B updates its request and calculates a newer total
    // requested amount for the client.
    // 3) Client B sends its view of the total requested amount to the
    // orchestrator.
    // 4) Client A sends its view of the requested amount to the
    // orchestrator, which does not include Client B's updates.
    request_lock: Mutex<()>,
}

impl Orchestrator {
    pub fn new(clients: ClientList) -> std::io::Result<Self> {
        let host_cid = vsock::VMADDR_CID_HOST;
        let port = eph_proto::VSOCK_PORT;
        let stream = VsockStream::connect_with_cid_port(host_cid, port)?;

        let msg_handler = move |ctype: ConnectionType, json: serde_json::Value| {
            Self::msg_handler(clients.clone(), ctype, json)
        };

        let connection = Connection::new(
            ConnectionType::Orchestrator,
            stream,
            msg_handler,
            Self::close_handler,
        )?;
        Ok(Self {
            connection,
            request_lock: Mutex::new(()),
        })
    }

    fn msg_handler(
        clients: ClientList,
        _ctype: ConnectionType,
        json: serde_json::Value,
    ) -> std::io::Result<()> {
        let cmd = serde_json::from_value::<ConsumerCommand>(json)?;

        match cmd.function {
            ConsumerFunction::EphMemGrant => {
                let more = cmd.more.unwrap_or(false);
                let result = Self::assign_grant(&clients, &cmd);

                if !more {
                    // The orchestrator is done sending grants for this request even
                    // if the last one was malformed or failed, so deliver whatever
                    // was assigned before it.
                    Client::flush_pending_grants(&clients);
                }
                result
            }
            ConsumerFunction::EphMemRelease => {
                // Requires both the size and offset fields to be present
                let Some(size) = cmd.size else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "EphMemRelease missing size field",
                    ));
                };
                let Some(offset) = cmd.offset else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "EphMemRelease missing offset field",
                    ));
                };
                Client::handle_eph_mem_release(&clients, size, offset)
            }
            other => {
                eprintln!(
                    "Received {} from orchestrator, but ephemerald doesn't handle this command.",
                    serde_json::to_string(&other).unwrap_or_else(|_| "unknown".to_string())
                );
                Ok(())
            }
        }
    }

    /// Validate an EphMemGrant from the orchestrator and assign it to a client.
    ///
    /// `clients` - The list of clients
    /// `cmd` - The EphMemGrant command
    fn assign_grant(clients: &ClientList, cmd: &ConsumerCommand) -> std::io::Result<()> {
        // Requires both the size and offset fields to be present
        let Some(size) = cmd.size else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "EphMemGrant missing size field",
            ));
        };
        let Some(offset) = cmd.offset else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "EphMemGrant missing offset field",
            ));
        };
        // Grants of size 0 should not happen
        if size == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "EphMemGrant with size 0",
            ));
        }

        let dax_device = DaxDevice::new(size)?;
        Client::handle_eph_mem_grant(clients, size, offset, dax_device)
    }

    fn close_handler(_ctype: ConnectionType, _id: u64) {
        // What's the point of the ephemeral daemon if there is no connection
        // to the orchestrator? Just kill the process in this case.
        eprintln!("Connection to orchestrator closed. Exiting.");
        std::process::exit(1);
    }

    pub fn request_eph_mem(&self, amount: u64) -> std::io::Result<()> {
        let cmd = ConsumerCommand::new(ConsumerFunction::EphMemRequest, Some(amount), None);
        self.connection.send(serde_json::to_value(&cmd)?)
    }

    pub fn release_eph_mem(&self, size: u64, offset: u64) -> std::io::Result<()> {
        let cmd = ConsumerCommand::new(ConsumerFunction::EphMemRelease, Some(size), Some(offset));
        self.connection.send(serde_json::to_value(&cmd)?)
    }

    pub fn lock_request(&self) -> std::sync::MutexGuard<'_, ()> {
        // The lock doesn't protect data, so a poisoned lock is fine.
        self.request_lock.lock().unwrap_or_else(|e| e.into_inner())
    }
}
