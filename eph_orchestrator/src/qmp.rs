//! Helper functions for interacting with QEMU's QMP (QEMU Machine Protocol)
//! interface.

use serde::{Deserialize, Serialize};
use std::convert::TryInto;
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Memdev {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    size: u64,
    merge: bool,
    dump: bool,
    prealloc: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    donatable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_userfaultfd: Option<bool>,
    share: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve: Option<bool>,
    host_nodes: Vec<u16>,
    policy: HostMemPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostMemPolicy {
    Default,
    Preferred,
    Bind,
    Interleave,
}

impl Memdev {
    pub fn is_donatable(&self) -> bool {
        let has_id = self.id.is_some();
        let donatable = self.donatable.unwrap_or(false);

        has_id && donatable
    }
}

impl TryInto<crate::DonatableRegion> for Memdev {
    type Error = &'static str;

    fn try_into(self) -> Result<crate::DonatableRegion, Self::Error> {
        if !self.is_donatable() {
            return Err("Memdev is not donatable");
        }

        // Unwrap is safe here because is_donatable() checks that id is Some.
        let id = self.id.unwrap();
        let path = format!("/objects/{}", id);

        Ok(crate::DonatableRegion {
            size: self.size,
            donated: 0,
            path,
        })
    }
}

fn read_qmp_response(
    stream: &mut crate::LineStream<UnixStream>
)
-> Result<serde_json::Value, std::io::Error> {
    loop {
        let response = stream.recv_line()?;
        let value: serde_json::Value = serde_json::from_str(&response)?;
        if let Some(error) = value.get("error") {
            return Err(std::io::Error::other(format!("QMP command failed: {}", error)));
        }
        if value.get("return").is_some() {
            return Ok(value);
        }
        // What we read was either the QMP greeting or an event. Ignore these
        // for now. Eventually, we will want to have a separate thread that
        // is dedicated to reading events and uses message passing to route
        // command responses and events to the correct handler.
    }
}

/// Initiates a QMP connection with the QEMU instance by sending the
/// `qmp_capabilities` command and checking the response.
pub fn initiate_connection(
    stream: &mut crate::LineStream<UnixStream>,
) -> Result<(), std::io::Error> {
    // Send the QMP capabilities command to the QEMU instance
    let capabilities_command = r#"{"execute": "qmp_capabilities"}"#;
    stream.send(capabilities_command)?;

    // Read the response from the QEMU instance.
    read_qmp_response(stream)?;
    Ok(())
}

pub fn get_memdevs(
    stream: &mut crate::LineStream<UnixStream>,
) -> Result<Vec<Memdev>, std::io::Error> {
    // Send the query-memdev command to the QEMU instance
    let query_memdev_command = r#"{"execute": "query-memdev"}"#;
    stream.send(query_memdev_command)?;

    // Extract the memdevs from the response.
    let mut qmp_response = read_qmp_response(stream)?;
    let memdevs = qmp_response
        .get_mut("return")
        .map(std::mem::take)
        .expect("read_qmp_response guarantees \"return\" is present on Ok");
    let memdevs: Vec<Memdev> = serde_json::from_value(memdevs)?;
    Ok(memdevs)
}
