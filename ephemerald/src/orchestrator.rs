use crate::client::{Client, ClientList};
use crate::connection::{Connection, ConnectionType};
use crate::dcd::DaxDevice;
use eph_proto::{ConsumerCommand, ConsumerFunction};
use vsock::VsockStream;

#[allow(dead_code)]
pub struct Orchestrator {
    connection: Connection<VsockStream>,
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
        Ok(Self { connection })
    }

    fn msg_handler(
        clients: ClientList,
        _ctype: ConnectionType,
        json: serde_json::Value,
    ) -> std::io::Result<()> {
        let cmd = serde_json::from_value::<ConsumerCommand>(json)?;

        match cmd.function {
            ConsumerFunction::EphMemRequest => {
                eprintln!(
                    "Received EphMemRequest from consumer, but orchestrator doesn't handle this command."
                );
                Ok(())
            }
            ConsumerFunction::EphMemResponse => {
                // Requires both the size and offset fields to be present
                let Some(size) = cmd.size else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "EphMemResponse missing size field",
                    ));
                };
                let Some(offset) = cmd.offset else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "EphMemResponse missing offset field",
                    ));
                };
                // Responses of size 0 should not happen
                if size == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "EphMemResponse with size 0",
                    ));
                }

                let dax_device = DaxDevice::new(size)?;
                Client::handle_eph_mem_response(&clients, size, offset, dax_device)
            }
        }
    }

    fn close_handler(_ctype: ConnectionType, _id: u64) {
        // What's the point of the ephemeral daemon if there is no connection
        // to the orchestrator? Just kill the process in this case.
        eprintln!("Connection to orchestrator closed. Exiting.");
        std::process::exit(1);
    }

    pub fn request_eph_mem(&self, amount: u64) -> std::io::Result<()> {
        let cmd = ConsumerCommand {
            function: ConsumerFunction::EphMemRequest,
            size: Some(amount),
            offset: None,
        };
        self.connection.send(serde_json::to_value(&cmd)?)
    }
}
