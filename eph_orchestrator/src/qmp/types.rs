//! Defines structs for QMP commands and responses
use serde::{Deserialize, Serialize};

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
