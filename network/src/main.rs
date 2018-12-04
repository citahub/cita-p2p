extern crate core_p2p;
extern crate futures;
extern crate tokio;
#[macro_use]
extern crate crossbeam_channel;
extern crate env_logger;
#[macro_use]
extern crate log;

use core_p2p::{
    custom_proto::encode_decode::Request,
    libp2p::secio,
    libp2p::Multiaddr,
    service::{build_service, ServiceEvent, ServiceHandle},
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures::prelude::*;
use futures::sync::mpsc::{unbounded as future_unbounded, UnboundedReceiver, UnboundedSender};
use std::{env, str, thread};

#[allow(dead_code)]
enum Task {
    Dial(Multiaddr),
    Listen(Multiaddr),
    Disconnect(usize),
    Messages(Vec<(Vec<usize>, usize, Request)>),
}

struct Process {
    task_receiver: UnboundedReceiver<Task>,
    event_sender: Sender<ServiceEvent>,
    new_dialer: Vec<Multiaddr>,
    new_listen: Vec<Multiaddr>,
    disconnect: Vec<usize>,
    messages_buffer: Vec<(Vec<usize>, usize, Request)>,
}

impl Process {
    pub fn new() -> (Self, UnboundedSender<Task>, Receiver<ServiceEvent>) {
        let (task_sender, task_receiver) = future_unbounded();
        let (event_sender, event_receiver) = unbounded();

        (
            Process {
                task_receiver,
                event_sender,
                new_dialer: Vec::new(),
                new_listen: Vec::new(),
                disconnect: Vec::new(),
                messages_buffer: Vec::new(),
            },
            task_sender,
            event_receiver,
        )
    }
}

impl Stream for Process {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<()>, ()> {
        match self.task_receiver.poll()? {
            Async::Ready(Some(task)) => {
                match task {
                    Task::Dial(address) => self.new_dialer.push(address),
                    Task::Listen(address) => self.new_listen.push(address),
                    Task::Messages(messages) => self.messages_buffer.extend(messages),
                    Task::Disconnect(id) => self.disconnect.push(id),
                }
                Ok(Async::Ready(Some(())))
            }
            Async::Ready(None) => Ok(Async::Ready(None)),
            Async::NotReady => Ok(Async::NotReady),
        }
    }
}

impl ServiceHandle for Process {
    fn out_event(&self, event: Option<ServiceEvent>) {
        if let Some(event) = event {
            self.event_sender.send(event).unwrap();
        }
    }

    fn new_dialer(&mut self) -> Option<Multiaddr> {
        self.new_dialer.pop()
    }

    fn new_listen(&mut self) -> Option<Multiaddr> {
        self.new_listen.pop()
    }

    fn disconnect(&mut self) -> Option<usize> {
        self.disconnect.pop()
    }

    fn send_message(&mut self) -> Vec<(Vec<usize>, usize, Request)> {
        self.messages_buffer.drain(..).collect()
    }
}

fn main() {
    let log_env = env::var("RUST_LOG")
        .and_then(|value| Ok(format!("{},network=info", value)))
        .unwrap_or_else(|_| "network=info".to_string());
    env::set_var("RUST_LOG", log_env);
    env_logger::init();

    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let (service_handle, task_sender, event_receiver) = Process::new();
    let mut service = build_service(key_pair, service_handle);
    let _ = service.listen_on("/ip4/127.0.0.1/tcp/1337".parse().unwrap());

    thread::spawn(move || tokio::run(service.map_err(|_| ()).for_each(|_| Ok(()))));

    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let (service_handle, task_sender_1, event_receiver_1) = Process::new();
    let mut service = build_service(key_pair, service_handle);
    let _ = service.listen_on("/ip4/127.0.0.1/tcp/1338".parse().unwrap());
    let _ = service.dial("/ip4/127.0.0.1/tcp/1337".parse().unwrap());

    thread::spawn(move || tokio::run(service.map_err(|_| ()).for_each(|_| Ok(()))));

    loop {
        select!(
            recv(event_receiver) -> event => {
                match event {
                    Ok(event) => {
                        match event {
                            ServiceEvent::CustomMessage {index, protocol, data} => {
                                if let Some(value) = data {
                                    info!("0 {:?}, {:?}, {:?}", index, protocol, str::from_utf8(&value));
                                }

                                let _ = task_sender.unbounded_send(Task::Messages(vec![(Vec::new(), 0, b"hello too".to_vec())]));
                            }
                            ServiceEvent::NodeInfo {index, listen_address} => {
                                info!("{:?} {:?}", index, listen_address);
                            }
                            _ => {}
                        }
                    }
                    Err(err) => error!("{}", err)
                }
            }
            recv(event_receiver_1) -> event => {
                match event {
                    Ok(event) => {
                        match event {
                            ServiceEvent::CustomMessage {index, protocol, data } => {
                                if let Some(value) = data {
                                    info!("1 {:?}, {:?}, {:?}", index, protocol, str::from_utf8(&value));
                                }
                            },
                            ServiceEvent::NodeInfo {index, listen_address} => {
                                info!("1 {:?} {:?}", index, listen_address);
                            }
                            _ => {}
                        }
                    },
                    Err(err) => error!("{}", err)
                }
                let _ = task_sender_1.unbounded_send(Task::Messages(vec![(Vec::new(), 0, b"hello".to_vec())]));
            }
        )
    }
}
