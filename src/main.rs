extern crate libp2p;
extern crate futures;
extern crate tokio;
extern crate bytes;

use libp2p::core::{Transport, nodes::raw_swarm::{RawSwarm, RawSwarmEvent},
                   either, upgrade, transport::boxed::Boxed, muxing::StreamMuxerBox};
use libp2p::core::nodes::Substream;
use libp2p::{PeerId, mplex, secio, yamux};
use libp2p::TransportTimeout;
use std::thread;
use std::usize;
use std::time::Duration;
use futures::prelude::*;
use std::sync::{Arc, Mutex};

use cita_handler::{CITAInEvent, CITAOutEvent, CITANodeHandler};

mod cita_handler;

fn main() {
    let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
    let local_peer_id = key_pair.to_public_key().into_peer_id();
    let mut swarm = build_swarm(key_pair);
    match swarm.listen_on("/ip4/127.0.0.1/tcp/1337".parse().unwrap()) {
        Ok(addr) => println!("success {}", addr),
        Err(addr) => println!("fail {}", addr),
    }

    thread::spawn(move || {
        let key_pair = secio::SecioKeyPair::secp256k1_generated().unwrap();
        let local_peer_id_client = key_pair.to_public_key().into_peer_id();
        let mut swarm = build_swarm(key_pair);
        if let Ok(_) = swarm.dial("/ip4/127.0.0.1/tcp/1337".parse().unwrap(), CITANodeHandler::new()) {
            println!("dial true");
        };
        tokio::run(futures::stream::poll_fn(move || -> Poll<Option<PeerId>, ()> {
            match swarm.poll() {
                Async::Ready(RawSwarmEvent::Connected {peer_id, endpoint}) => {
                    println!("client connected {:?}", peer_id);
                    Ok(Async::Ready(Some(peer_id)))
                }
                _ => {
//                        println!("111");
                    Ok(Async::NotReady)
                }
            }
        }).for_each(|_|Ok(())));

    });

    tokio::run(futures::stream::poll_fn(move || -> Poll<Option<PeerId>, std::io::Error> {
        loop {
            match swarm.poll() {
                Async::Ready(RawSwarmEvent::Connected {peer_id, endpoint}) => {
                    println!("sever connected {:?}", peer_id);
                    return Ok(Async::Ready(Some(peer_id)));
                }
                Async::Ready(RawSwarmEvent::IncomingConnection(incoming)) => {
                    println!("{}, {}", incoming.send_back_addr(), incoming.listen_addr());
                    incoming.accept(CITANodeHandler::new());
                    continue;
                }
                Async::Ready(RawSwarmEvent::NodeClosed { peer_id, .. }) => {
                    println!("close {:?}", peer_id);
                    return Ok(Async::NotReady)
                }
                Async::Ready(RawSwarmEvent::DialError { multiaddr, error, .. }) => {
                    println!("{:?}", multiaddr);
                    return Ok(Async::NotReady)
                }
                Async::Ready(RawSwarmEvent::UnknownPeerDialError { multiaddr, error, .. }) => {
                    println!("{:?}, unknown", multiaddr);
                    return Ok(Async::NotReady)
                }
                Async::Ready(RawSwarmEvent::IncomingConnectionError { listen_addr, send_back_addr, error }) => {
                    println!("incoming error: {}", error);
                    continue;
                }
                Async::NotReady => return Ok(Async::NotReady),
                _ => {
                    println!("other state");
                    return Ok(Async::NotReady)
                }
            }

        }
    }).map_err(|_|()).for_each(|_id|{
        println!("accept one link");
        Ok(())
        }
    ))
}

pub fn build_transport(
    local_private_key: secio::SecioKeyPair
) -> Boxed<(PeerId, StreamMuxerBox)> {
    let mut mplex_config = mplex::MplexConfig::new();
    mplex_config.max_buffer_len_behaviour(mplex::MaxBufferBehaviour::Block);
    mplex_config.max_buffer_len(usize::MAX);

    let base = libp2p::CommonTransport::new()
        .with_upgrade(secio::SecioConfig::new(local_private_key))
        .and_then(move |out, endpoint| {
            let upgrade = upgrade::or(
                upgrade::map(yamux::Config::default(), either::EitherOutput::First),
                upgrade::map(mplex_config, either::EitherOutput::Second),
            );
            let peer_id = out.remote_key.into_peer_id();
            let upgrade = upgrade::map(upgrade, move |muxer| (peer_id, muxer));
            upgrade::apply(out.stream, upgrade, endpoint.into())
        })
        .map(|(id, muxer), _| (id, StreamMuxerBox::new(muxer)));

    TransportTimeout::new(base, Duration::from_secs(20))
        .boxed()
}

pub fn build_swarm(local_private_key: secio::SecioKeyPair) -> RawSwarm<Boxed<(PeerId, StreamMuxerBox)>, CITAInEvent, CITAOutEvent, CITANodeHandler<Substream<StreamMuxerBox>>> {
    let local_public_key = local_private_key.to_public_key();
    let local_peer_id = local_public_key.clone().into_peer_id();

    let transport = build_transport(local_private_key);

    RawSwarm::new(transport)
}
