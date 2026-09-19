//! Contains the management of QMP events

#[allow(dead_code)]
pub struct QmpEvent {
    pub vm_path: String,
    pub event: serde_json::Value,
}
