use crate::connection::{Connection, ConnectionType};
use eph_proto::{ConsumerCommand, ConsumerFunction};
use vsock::VsockStream;

#[allow(dead_code)]
pub struct Orchestrator {
    connection: Connection<VsockStream>,
}

impl Orchestrator {
    pub fn new() -> std::io::Result<Self> {
        let host_cid = vsock::VMADDR_CID_HOST;
        let port = eph_proto::VSOCK_PORT;
        let stream = VsockStream::connect_with_cid_port(host_cid, port)?;
        let connection = Connection::new(
            ConnectionType::Orchestrator,
            stream,
            Self::msg_handler,
            Self::close_handler,
        )?;
        Ok(Self { connection })
    }

    fn msg_handler(_ctype: ConnectionType, _json: serde_json::Value) -> std::io::Result<()> {
        // Implement the message handling logic here
        Ok(())
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
