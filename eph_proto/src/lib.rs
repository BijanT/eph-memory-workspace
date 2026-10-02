use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub const VSOCK_PORT: u32 = 1848;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsumerFunction {
    EphMemRequest,
    EphMemResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
// Consumer commands are much simpler than QMP. They only have a function name
// and, optionally, a size argument.
pub struct ConsumerCommand {
    pub function: ConsumerFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl ConsumerCommand {
    pub fn new(function: ConsumerFunction, size: Option<u64>) -> Self {
        Self { function, size }
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub fn send<T>(&self, stream: &mut T) -> std::io::Result<()>
    where
        T: std::io::Write,
    {
        let cmd_string = format!("{}\n", self.to_json()?);
        stream.write_all(cmd_string.as_bytes())?;
        Ok(())
    }
}

impl FromStr for ConsumerCommand {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}
