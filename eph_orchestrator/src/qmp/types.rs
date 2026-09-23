//! Defines structs for QMP commands and responses
use crate::donor::DonatableRegion;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct QmpCommand {
    pub execute: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

impl QmpCommand {
    pub fn new(execute: impl Into<String>) -> Self {
        Self {
            execute: execute.into(),
            arguments: None,
        }
    }

    pub fn with_arguments<T: Serialize>(
        execute: impl Into<String>,
        arguments: T,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            execute: execute.into(),
            arguments: Some(serde_json::to_value(arguments)?),
        })
    }
}

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

impl TryInto<DonatableRegion> for Memdev {
    type Error = &'static str;

    fn try_into(self) -> Result<DonatableRegion, Self::Error> {
        if !self.is_donatable() {
            return Err("Memdev is not donatable");
        }

        // Unwrap is safe here because is_donatable() checks that id is Some.
        let id = self.id.unwrap();
        let path = format!("/objects/{}", id);

        Ok(DonatableRegion::new(self.size, path))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct QomListArgs {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct QomListResponse {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct QomGetArgs {
    pub path: String,
    pub property: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CxlDynamicCapacityExtent {
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CxlAddReleaseCapacityEventData {
    // Path to the DCD device triggering this event in the QOM
    path: String,
    // Extents added/removed from the DCD device
    extents: Vec<CxlDynamicCapacityExtent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CxlExtentSelectionPolicy {
    Free,
    Contiguous,
    // The only one we actually use. Keep the rest for completeness.
    Prescriptive,
    EnableSharedAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CxlAddDynamicCapacityArgs {
    // Path to the DCD device to give capacity to
    pub path: String,
    // The "host-id" of a host in a CXL pool. Not used here
    pub host_id: u16,
    // How to select extents from the donor's memory pool. We always use "Prescriptive"
    pub selection_policy: CxlExtentSelectionPolicy,
    // The region number to use for allocation. We always use 0.
    pub region: u8,
    // Tag to name the capacity. Not used here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    // The extents to add to the DCD device.
    pub extents: Vec<CxlDynamicCapacityExtent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EphMemRevokeEventData {
    // Path to the DCD device triggering this event in the QOM
    path: String,
    // The amount of memory requested to be revoked
    size: u64,
    // The ID to tie this event to returned memory. Not currently used.
    id: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EphMemDonateCapacityData {
    // Path to the DCD device to request data from
    pub path: String,
    // The amount of memory requested in bytes
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EphMemDonateResult {
    // The amount of memory that was actually donated in bytes
    pub granted: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EphMemReturnCapacityData {
    // Path to the DCD device to return memory to
    pub path: String,
    // The amount of memory to return in bytes
    pub size: u64,
    // The ID to tie the return to a revoke event. Not currently used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i32>,
}
