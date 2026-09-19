//! Contains the management of QMP events

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::Deserialize;

use crate::qmp::types;

pub struct QmpEvent {
    /// The path to the QMP socket of the VM that sent the event.
    pub vm_path: PathBuf,
    pub json: serde_json::Value,
}

pub fn qmp_event_handler_thread(
    donors: &crate::DonorVMList,
    event_rx: mpsc::Receiver<QmpEvent>,
) -> Result<(), std::io::Error> {
    for event in event_rx {
        let Some(event_name) = event.json.get("event").and_then(|v| v.as_str()) else {
            // Somehow the event doesn't have an "event" field, or the field
            // isn't a string. This shouldn't happen in practice.
            continue;
        };
        let event_data = event.json.get("data").unwrap_or(&serde_json::Value::Null);

        // Might want to spawn a thread for each event, or even have multiple
        // threads for handling events and add events to a queue for processing.
        // For now, just handle them synchronously in the same thread.
        if let Err(e) = handle_event(donors, &event.vm_path, event_name, event_data) {
            eprintln!(
                "{}: Error handling {} event: {}",
                event.vm_path.display(),
                event_name,
                e
            );
        }
    }
    Ok(())
}

/// Deserializes the "data" field of a QMP event into `T` without copying it.
fn parse_event_data<'de, T: Deserialize<'de>>(
    event_data: &'de serde_json::Value,
) -> Result<T, std::io::Error> {
    Ok(T::deserialize(event_data)?)
}

fn handle_event(
    _donors: &crate::DonorVMList,
    vm_path: &Path,
    event_name: &str,
    event_data: &serde_json::Value,
) -> Result<(), std::io::Error> {
    match event_name {
        "CXL_ADD_DYNAMIC_CAPACITY_RESPONSE" | "CXL_RELEASE_DYNAMIC_CAPACITY" => {
            let data: types::CxlAddReleaseCapacityEventData = parse_event_data(event_data)?;
            println!(
                "Received {} event from VM at '{}': {:?}",
                event_name,
                vm_path.display(),
                data
            );
        }
        "EPH_MEM_REVOKE" => {
            let data: types::EphMemRevokeEventData = parse_event_data(event_data)?;
            println!(
                "Received EPH_MEM_REVOKE event from VM at '{}': {:?}",
                vm_path.display(),
                data
            );
        }
        _ => {
            println!(
                "Received unhandled event '{}' from VM at '{}'",
                event_name,
                vm_path.display()
            );
        }
    }
    Ok(())
}
