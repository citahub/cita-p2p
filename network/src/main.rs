extern crate core_p2p;
extern crate futures;
extern crate tokio;

use core_p2p::{
    build_swarm, secio, CITAInEvent, CITANodeHandler, CITAOutEvent, PeerId, RawSwarmEvent,
};
use futures::prelude::*;
use std::thread;

fn main() {
    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let mut swarm = build_swarm(key_pair);
    match swarm.listen_on("/ip4/127.0.0.1/tcp/1337".parse().unwrap()) {
        Ok(addr) => println!("success {}", addr),
        Err(addr) => println!("fail {}", addr),
    }
    let _ = swarm.dial(
        "/ip4/127.0.0.1/tcp/1338".parse().unwrap(),
        CITANodeHandler::new(),
    );

    thread::spawn(move || {
        let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
        let mut swarm = build_swarm(key_pair);
        let _ = swarm.listen_on("/ip4/127.0.0.1/tcp/1338".parse().unwrap());
        if let Ok(_) = swarm.dial(
            "/ip4/127.0.0.1/tcp/1337".parse().unwrap(),
            CITANodeHandler::new(),
        ) {
            println!("dial true");
        };
        tokio::run(
            futures::stream::poll_fn(move || -> Poll<Option<()>, ()> {
                loop {
                    match swarm.poll() {
                        Async::Ready(RawSwarmEvent::Connected { peer_id, endpoint }) => {
                            println!("client connected {:?}", peer_id);
                        }
                        Async::Ready(RawSwarmEvent::NodeEvent { peer_id, event }) => {
                            println!("{:?}, {:?}", peer_id, event);
                            match event {
                                CITAOutEvent::CustomProtocolOpen { protocol, version } => {
                                    println!("send data");
                                    swarm.broadcast_event(&CITAInEvent::SendCustomMessage {
                                        protocol,
                                        data: (String::from("hello"), Vec::new()),
                                    })
                                }
                                CITAOutEvent::CustomMessage { protocol, data } => {
                                    println!("get back data {:?}", data);
                                }
                                _ => {}
                            }
                        }
                        _ => continue,
                    }
                }
            }).for_each(|_| Ok(())),
        );
    });

    tokio::run(
        futures::stream::poll_fn(move || -> Poll<Option<PeerId>, std::io::Error> {
            loop {
                let (peer_id, event) = match swarm.poll() {
                    Async::Ready(RawSwarmEvent::Connected { peer_id, endpoint }) => {
                        println!("sever connected {:?}", peer_id);
                        continue;
                    }
                    Async::Ready(RawSwarmEvent::IncomingConnection(incoming)) => {
                        println!("{}, {}", incoming.send_back_addr(), incoming.listen_addr());
                        incoming.accept(CITANodeHandler::new());
                        continue;
                    }
                    Async::Ready(RawSwarmEvent::NodeClosed { peer_id, .. }) => {
                        println!("close {:?}", peer_id);
                        return Ok(Async::NotReady);
                    }
                    Async::Ready(RawSwarmEvent::DialError {
                        multiaddr, error, ..
                    }) => {
                        println!("{:?}", multiaddr);
                        return Ok(Async::NotReady);
                    }
                    Async::Ready(RawSwarmEvent::UnknownPeerDialError {
                        multiaddr, error, ..
                    }) => {
                        println!("{:?}, unknown", multiaddr);
                        return Ok(Async::NotReady);
                    }
                    Async::Ready(RawSwarmEvent::IncomingConnectionError {
                        listen_addr,
                        send_back_addr,
                        error,
                    }) => {
                        println!("incoming error: {}", error);
                        continue;
                    }
                    Async::NotReady => return Ok(Async::NotReady),
                    Async::Ready(RawSwarmEvent::Replaced {
                        peer_id, endpoint, ..
                    }) => {
                        println!("{:?} replaced", peer_id);
                        continue;
                    }
                    Async::Ready(RawSwarmEvent::NodeError { peer_id, error, .. }) => {
                        println!("{:?} error: {}", peer_id, error);
                        continue;
                    }
                    Async::Ready(RawSwarmEvent::ListenerClosed {
                        listen_addr,
                        result,
                        ..
                    }) => {
                        println!("{} listen closed", listen_addr);
                        continue;
                    }
                    Async::Ready(RawSwarmEvent::NodeEvent { peer_id, event }) => {
                        println!("{:?}, {:?}", peer_id, event);
                        (peer_id, event)
                    }
                };
                match event {
                    CITAOutEvent::CustomMessage { protocol, data } => {
                        println!("get data {:?}", data);
                        swarm.broadcast_event(&CITAInEvent::SendCustomMessage {
                            protocol,
                            data: (String::from("hello too"), Vec::new()),
                        })
                    }
                    _ => {}
                }
            }
        }).map_err(|_| ())
        .for_each(|_id| {
            println!("accept one link");
            Ok(())
        }),
    )
}
