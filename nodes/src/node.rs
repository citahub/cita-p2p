extern crate core_p2p;
extern crate futures;
extern crate tokio;
#[macro_use]
extern crate crossbeam_channel;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate serde;
extern crate serde_json;

#[macro_use]
extern crate serde_derive;


use core_p2p::{
    custom_proto::encode_decode::Request,
    secio,
    service::{build_service, ServiceEvent, ServiceHandle},
    Multiaddr,
    Endpoint
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures::prelude::*;
use futures::sync::mpsc::{unbounded as future_unbounded, UnboundedReceiver, UnboundedSender};
use std::{env, str, thread};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
enum MessageType {
    ShareAddr,
    PassShareAddr,
    ReturnNodeAddrs,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    mtype: MessageType,
    //data: Vec<u8>,
    data: String,
    timestamp: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct NodeManager {
    pub node_addrs: HashMap<String, usize>,
    // <PeerId index, protocol version>
    pub node_conns: HashMap<usize, usize>
}


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
        .and_then(|value| Ok(format!("{},node=info", value)))
        .unwrap_or_else(|_| "node=info".to_string());
    env::set_var("RUST_LOG", log_env);
    env_logger::init();

    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let (service_handle, task_sender, event_receiver) = Process::new();
    let mut service = build_service(key_pair, service_handle);
    let addr = service.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap().to_string();

    let mut nas = NodeManager {
        node_addrs: HashMap::new(),
        node_conns: HashMap::new()
    };
    // for test now
    nas.node_addrs.insert(addr.clone(), 1);

    let mut dial_node = false;
    if let Some(to_dial) = std::env::args().nth(1) {
        info!("to dial {:?}", to_dial);
        let _ = service.dial(to_dial.parse().unwrap());
        dial_node = true;
    }

    let localaddr = addr.clone();
    info!("localaddr {:?}", localaddr);
    info!("listening on {:?}", localaddr );
    thread::spawn(move || {
        tokio::run(service.map_err(|_| ()).for_each(|_| Ok(())))
    });
    

    loop {
        select!(
            recv(event_receiver) -> event => {
                match event {
                    Ok(event) => {
                        match event {
                            ServiceEvent::CustomProtocolOpen {index, protocol, version, node_info} => {
                                info!("in CustomProtocolOpen {:?} {:?} {:?} {:?}", index, protocol, version, node_info);
                                // each connection opened, this arm was entered.
                                if node_info.endpoint == Endpoint::Dialer {
                                    // here, send my addr
                                    let my_addr_msg = Message {
                                        mtype: MessageType::ShareAddr,
                                        data: localaddr.to_string(),
                                        timestamp: 0
                                    };
                                    let msg_str = serde_json::to_string(&my_addr_msg).unwrap();
                                    
                                    info!("nas {:?}", msg_str);
                                    let _ = task_sender.unbounded_send(Task::Messages(vec![(vec![index], 0, msg_str.into_bytes() )]));
                                }
                                else if node_info.endpoint == Endpoint::Listener {
                                    // here, connection opened, record it 
                                    nas.node_conns.entry(index).or_insert(protocol);
                                    info!("nas {:?}", nas);
                                }
                            },
                            ServiceEvent::NodeInfo {index, endpoint, listen_address} => {
                                //info!("node info {:?} {:?} {:?}", index, listen_address, endpoint);
                            },
                            ServiceEvent::CustomMessage {index, protocol, data } => {
                                if let Some(value) = data {
                                    let value_str = str::from_utf8(&value).unwrap();
                                    let msg: Message = serde_json::from_str(value_str).unwrap();
                                    info!("Message {:?}", msg);
                                    // here, parse message
                                    // check message type
                                    match msg.mtype {
                                        MessageType::ShareAddr => {
                                            let addr = msg.data;
                                            nas.node_addrs.entry(addr.clone()).or_insert(0);

                                            info!("nas {:?}", nas);

                                            // XXX: here, contains the from addr just now, but for test
                                            // now
                                            let addr_list_str = serde_json::to_string(&nas.node_addrs).unwrap(); 
                                            let return_addrs_msg = Message {
                                                mtype: MessageType::ReturnNodeAddrs,
                                                data: addr_list_str,
                                                timestamp: 0
                                            };
                                            let return_addrs_msg = serde_json::to_string(&return_addrs_msg).unwrap(); 
                                            let _ = task_sender.unbounded_send(Task::Messages(vec![(vec![index], 0, return_addrs_msg.into_bytes() )]));

                                            // XXX: here, forward the ShareAddr message to
                                            // neigborhood, except the one message come from 
                                            // from = index
                                            info!("now index is {:?}", index);
                                            //let other_neigbors: Vec<usize> = nas.node_conns.keys().take_while(|x| x != &&index).cloned().collect();
                                            let mut other_neigbors = Vec::new();
                                            let node_conns = nas.node_conns.clone();
                                            for (key, _) in node_conns {
                                                if key != index {
                                                    other_neigbors.push(key.clone());
                                                }
                                            }

                                            info!("other_neigbors {:?}", other_neigbors);
                                            let pass_share_addr = Message {
                                                mtype: MessageType::PassShareAddr,
                                                data: addr.clone(),
                                                timestamp: 0
                                            };
                                            info!("pass_share_addr {:?}", pass_share_addr);
                                            let pass_share_addr_str = serde_json::to_string(&pass_share_addr).unwrap(); 
                                            if other_neigbors.len() > 0 {
                                                let _ = task_sender.unbounded_send(Task::Messages(vec![(other_neigbors, 0, pass_share_addr_str.into_bytes() )]));
                                            }

                                        },
                                        MessageType::ReturnNodeAddrs => {
                                            let addrs: HashMap<String, usize> = serde_json::from_str(&msg.data).unwrap();
                                            info!("ReturnNodeAddrs {:?}", addrs);

                                            for (addr, _) in addrs {
                                                nas.node_addrs.entry(addr).or_insert(0);
                                            }

                                            nas.node_conns.entry(index).or_insert(protocol);
                                            info!("nas {:?}", nas);
                                        },
                                        MessageType::PassShareAddr => {
                                            info!("in passsharaddr");
                                            let addr = msg.data;
                                            nas.node_addrs.entry(addr.clone()).or_insert(0);
                                            info!("nas {:?}", nas);

                                            let mut other_neigbors = Vec::new();
                                            let node_conns = nas.node_conns.clone();
                                            for (key, _) in node_conns {
                                                if key != index {
                                                    other_neigbors.push(key.clone());
                                                }
                                            }

                                            info!("other_neigbors {:?}", other_neigbors);
                                            let pass_share_addr = Message {
                                                mtype: MessageType::PassShareAddr,
                                                data: addr.clone(),
                                                timestamp: 0
                                            };
                                            info!("pass_share_addr {:?}", pass_share_addr);
                                            let pass_share_addr_str = serde_json::to_string(&pass_share_addr).unwrap(); 
                                            if other_neigbors.len() > 0 {
                                                let _ = task_sender.unbounded_send(Task::Messages(vec![(other_neigbors, 0, pass_share_addr_str.into_bytes() )]));
                                            }

                                        },

                                    }

                                }
                            },
                            _ => {}
                        }
                    },
                    Err(err) => error!("{}", err)
                }
            }
        )
    }
}
