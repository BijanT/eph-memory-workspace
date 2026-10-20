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
// Consumer commands are much simpler than QMP.
// They have the following fields:
// `function`: The type of command being send.
// `size`: The size of memory corresponsing to the command, if applicable.
// `offset`: The offset of the memory corresponding to the command, if applicable.
// `id`: A unique identifier for the command, if applicable.
// `more`: Indicates if there are more commands to follow, if applicable.
pub struct ConsumerCommand {
    pub function: ConsumerFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more: Option<bool>,
}

impl ConsumerCommand {
    pub fn new(function: ConsumerFunction, size: Option<u64>, offset: Option<u64>) -> Self {
        Self {
            function,
            size,
            offset,
            id: None,
            more: None,
        }
    }

    pub fn with_id(&self, id: Option<u64>) -> Self {
        let mut cmd = self.clone();
        cmd.id = id;
        cmd
    }

    pub fn with_more(&self, more: bool) -> Self {
        let mut cmd = self.clone();
        cmd.more = Some(more);
        cmd
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
