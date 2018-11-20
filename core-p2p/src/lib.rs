extern crate byteorder;
extern crate bytes;
extern crate futures;
extern crate libp2p;
extern crate tokio;

use libp2p::core::nodes::Substream;
use libp2p::core::{
    either, muxing::StreamMuxerBox, nodes::raw_swarm::RawSwarm, transport::boxed::Boxed, upgrade,
    Transport,
};
use libp2p::TransportTimeout;
use libp2p::{mplex, yamux};
use std::time::Duration;
use std::usize;

pub use libp2p::{core::nodes::raw_swarm::RawSwarmEvent, secio, PeerId};

pub use cita_handler::{CITAInEvent, CITANodeHandler, CITAOutEvent};

mod cita_handler;
mod custom_proto;

fn build_transport(local_private_key: secio::SecioKeyPair) -> Boxed<(PeerId, StreamMuxerBox)> {
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
        }).map(|(id, muxer), _| (id, StreamMuxerBox::new(muxer)));

    TransportTimeout::new(base, Duration::from_secs(20)).boxed()
}

pub fn build_swarm(
    local_private_key: secio::SecioKeyPair,
) -> RawSwarm<
    Boxed<(PeerId, StreamMuxerBox)>,
    CITAInEvent,
    CITAOutEvent,
    CITANodeHandler<Substream<StreamMuxerBox>>,
> {
    let local_public_key = local_private_key.to_public_key();

    let transport = build_transport(local_private_key);

    RawSwarm::new(transport)
}
