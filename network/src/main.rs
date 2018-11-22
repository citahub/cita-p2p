extern crate core_p2p;
extern crate futures;
extern crate tokio;
#[macro_use]
extern crate crossbeam_channel;

use core_p2p::{
    custom_proto::cita_proto::CitaRequest,
    secio,
    service::{build_service, ServiceEvent, ServiceHandle},
    Multiaddr, PeerId,
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures::prelude::*;
use std::thread;

enum Task {
    Dial(Multiaddr),
    Listen(Multiaddr),
    Messages(Vec<(Option<PeerId>, usize, CitaRequest)>),
}

struct Process {
    task_receiver: Receiver<Task>,
    event_sender: Sender<ServiceEvent>,
    new_dialer: Vec<Multiaddr>,
    new_listen: Vec<Multiaddr>,
    messages_buffer: Vec<(Option<PeerId>, usize, CitaRequest)>,
}

impl Process {
    pub fn new() -> (Self, Sender<Task>, Receiver<ServiceEvent>) {
        let (task_sender, task_receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();

        (
            Process {
                task_receiver,
                event_sender,
                new_dialer: Vec::new(),
                new_listen: Vec::new(),
                messages_buffer: Vec::new(),
            },
            task_sender,
            event_receiver,
        )
    }
    pub fn receive_task(&mut self) {
        while let Ok(task) = self.task_receiver.try_recv() {
            match task {
                Task::Dial(address) => self.new_dialer.push(address),
                Task::Listen(address) => self.new_listen.push(address),
                Task::Messages(messages) => self.messages_buffer.extend(messages),
            }
        }
    }
}

impl ServiceHandle for Process {
    fn before_poll(&mut self) {
        self.receive_task()
    }

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

    fn send_message(&mut self) -> Vec<(Option<PeerId>, usize, CitaRequest)> {
        self.messages_buffer.drain(..).collect()
    }
}

fn main() {
    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let (service_handle, task_sender, event_receiver) = Process::new();
    let mut service = build_service(key_pair, service_handle);
    let _ = service.listen_on("/ip4/127.0.0.1/tcp/1337".parse().unwrap());

    thread::spawn(move || tokio::run(service.map_err(|_| ()).for_each(|_| Ok(()))));

    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let (service_handle, task_sender_1, event_receiver_1) = Process::new();
    let mut service = build_service(key_pair, service_handle);
    let _ = service.dial("/ip4/127.0.0.1/tcp/1337".parse().unwrap());

    thread::spawn(move || tokio::run(service.map_err(|_| ()).for_each(|_| Ok(()))));

    loop {
        select!(
            recv(event_receiver) -> event => {
                match event {
                    Ok(event) => {
                        match event {
                            ServiceEvent::CustomMessage {id, data, ..} => {
                                println!("{:?}, {:?}", id, data);
                                let _ = task_sender.send(Task::Messages(vec![(None, 0, (String::from("hello too"), Vec::new()))]));
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {}
                }
            }
            recv(event_receiver_1) -> event => {
                match event {
                    Ok(event) => println!("1 {:?}", event),
                    Err(_) => {}
                }
                let _ = task_sender_1.send(Task::Messages(vec![(None, 0, (String::from("hello"), Vec::new()))]));
            }
        )
    }
}
