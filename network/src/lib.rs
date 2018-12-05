extern crate core_p2p;
extern crate crossbeam_channel;
extern crate env_logger;
extern crate futures;
extern crate tokio;

use core_p2p::{
    custom_proto::encode_decode::Request,
    libp2p::Multiaddr,
    service::{ServiceEvent, ServiceHandle},
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures::prelude::*;
use futures::sync::mpsc::{unbounded as future_unbounded, UnboundedReceiver, UnboundedSender};

#[allow(dead_code)]
pub enum Task {
    Dial(Multiaddr),
    Listen(Multiaddr),
    Disconnect(usize),
    Messages(Vec<(Vec<usize>, usize, Request)>),
}

pub struct Process {
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
