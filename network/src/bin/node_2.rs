extern crate core_p2p;
extern crate env_logger;
extern crate futures;
extern crate network;
extern crate tokio;
#[macro_use]
extern crate log;
#[macro_use]
extern crate crossbeam_channel;

use core_p2p::{
    libp2p::secio,
    service::{build_service},
};
use futures::prelude::*;
use network::{Process};
use std::{env, thread};

fn main() {
    let log_env = env::var("RUST_LOG")
        .and_then(|value| Ok(format!("{},core_p2p=trace,network=trace", value)))
        .unwrap_or_else(|_| "core_p2p=trace,network=trace".to_string());
    env::set_var("RUST_LOG", log_env);
    env_logger::init();

    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let (service_handle, _task_sender, event_receiver) = Process::new();
    let mut service = build_service(key_pair, service_handle, true);
    let _ = service.listen_on("/ip4/127.0.0.1/tcp/1338".parse().unwrap());
    let _ = service.dial("/ip4/127.0.0.1/tcp/1337".parse().unwrap());

    thread::spawn(move || tokio::run(service.map_err(|_| ()).for_each(|_| Ok(()))));

    loop {
        select!(
            recv(event_receiver) -> event => {
                match event {
                    Ok(event) => {
                        info!("{:?}", event);
                    }
                    Err(err) => error!("{}", err)
                }
            }
        )
    }
}
