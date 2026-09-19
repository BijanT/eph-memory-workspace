//! Contains the management of QMP events

use std::path::PathBuf;

#[allow(dead_code)]
pub struct QmpEvent {
    /// The path to the QMP socket of the VM that sent the event.
    pub vm_path: PathBuf,
    pub event: serde_json::Value,
}
