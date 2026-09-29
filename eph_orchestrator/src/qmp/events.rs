//! Contains the management of QMP events

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::Deserialize;

use crate::Vm;
use crate::qmp::types;

pub struct QmpEvent {
    /// The path to the QMP socket of the VM that sent the event.
    pub vm_path: PathBuf,
    pub json: serde_json::Value,
}

pub fn qmp_event_handler_thread(
    vms: &crate::VmList,
    event_rx: mpsc::Receiver<QmpEvent>,
) -> Result<(), std::io::Error> {
    for event in event_rx {
        let Some(event_name) = event.json.get("event").and_then(|v| v.as_str()) else {
            // Somehow the event doesn't have an "event" field, or the field
            // isn't a string. This shouldn't happen in practice.
            continue;
        };
        let event_data = event.json.get("data").unwrap_or(&serde_json::Value::Null);

        if let Err(e) = handle_event(vms, &event.vm_path, event_name, event_data) {
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
    vms: &crate::VmList,
    vm_path: &Path,
    event_name: &str,
    event_data: &serde_json::Value,
) -> Result<(), std::io::Error> {
    match event_name {
        "CXL_ADD_DYNAMIC_CAPACITY_RESPONSE" => {
            let data: types::CxlAddReleaseCapacityEventData = parse_event_data(event_data)?;
            let Some(consumer) = Vm::get_consumer(vms, vm_path) else {
                eprintln!("{}: Consumer VM not found for path: {:?}", event_name, data);
                return Ok(());
            };

            // Vm::get_consumer() guarantees that `consumer` is Some, so unwrap
            // is safe here.
            consumer
                .consumer
                .as_ref()
                .unwrap()
                .signal_add_dc(&data.extents);
        }
        "CXL_RELEASE_DYNAMIC_CAPACITY" => {
            let data: types::CxlAddReleaseCapacityEventData = parse_event_data(event_data)?;
            let Some(consumer) = Vm::get_consumer(vms, vm_path) else {
                eprintln!("{}: Consumer VM not found for path: {:?}", event_name, data);
                return Ok(());
            };

            consumer
                .consumer
                .as_ref()
                .unwrap()
                .signal_release_dc(&data.extents);
        }
        "EPH_MEM_REVOKE" => {
            let data: types::EphMemRevokeEventData = parse_event_data(event_data)?;
            let Some(donor) = Vm::get_donor(vms, vm_path) else {
                eprintln!("{}: Donor VM not found for path: {:?}", event_name, vm_path);
                return Ok(());
            };

            // Revoking memory requires a lot of outside communication.
            // Spawn a new thread to not block the event handling loop.
            std::thread::spawn(move || {
                // Unwrap is safe here because `Vm::get_donor()` guarantees that
                // `donor` is Some.
                if let Err(e) = donor
                    .donor
                    .as_ref()
                    .unwrap()
                    .revoke_eph_memory(&data.path, data.size)
                {
                    eprintln!("EPH_MEM_REVOKE: Error revoking ephemeral memory: {}", e);
                }
            });
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
