mod client;
mod connection;
mod orchestrator;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use client::Client;
use orchestrator::Orchestrator;

fn main() {
    // Have the server crash if any thread panics
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let clients = Arc::new(Mutex::new(BTreeMap::<u32, Arc<Client>>::new()));
    let orchestrator = Arc::new(match Orchestrator::new(clients.clone()) {
        Ok(orchestrator) => orchestrator,
        Err(err) => {
            eprintln!("Failed to connect to orchestrator: {:?}", err);
            std::process::exit(1);
        }
    });

    Client::listener_thread(clients, orchestrator).unwrap();
}
