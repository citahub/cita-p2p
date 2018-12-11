extern crate byteorder;
extern crate bytes;
extern crate futures;
pub extern crate libp2p;
extern crate parking_lot;
extern crate tokio;
#[macro_use]
extern crate log;

pub use cita_handler::{CITAInEvent, CITANodeHandler, CITAOutEvent};

mod cita_handler;
//pub mod custom_proto;
pub mod service;
