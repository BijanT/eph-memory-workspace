mod client;
mod connection;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use client::Client;

fn main() {
    // Have the server crash if any thread panics
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let clients = Arc::new(Mutex::new(BTreeSet::<Client>::new()));

    Client::listener_thread(clients).unwrap();
}
