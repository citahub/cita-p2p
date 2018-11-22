use cita_handler::{CITAInEvent, CITANodeHandler, CITAOutEvent};
use custom_proto::cita_proto::{CitaRequest, CitaResponse};
use futures::prelude::*;
use libp2p::core::{
    either,
    multiaddr::Protocol,
    muxing::StreamMuxerBox,
    nodes::raw_swarm::{ConnectedPoint, RawSwarm, RawSwarmEvent},
    nodes::Substream,
    transport::boxed::Boxed,
    upgrade, Endpoint, Multiaddr, PeerId, Transport,
};
use libp2p::{mplex, secio, yamux, TransportTimeout};
use std::collections::HashMap;
use std::io::Error;
use std::time::Duration;
use std::usize;

#[derive(Debug)]
pub enum ServiceEvent {
    /// Closed connection to a node.
    NodeClosed {
        id: PeerId,
    },
    CustomProtocolOpen {
        id: PeerId,
        protocol: usize,
        version: u8,
    },
    CustomProtocolClosed {
        id: PeerId,
        protocol: usize,
    },
    CustomMessage {
        id: PeerId,
        protocol: usize,
        data: CitaResponse,
    },
}

pub trait ServiceHandle: Sync + Send {
    /// Hook before poll
    fn before_poll(&mut self);
    /// Send service event to out
    fn out_event(&self, event: Option<ServiceEvent>);
    /// Dialing a new address
    fn new_dialer(&mut self) -> Option<Multiaddr>;
    /// Listening a new address
    fn new_listen(&mut self) -> Option<Multiaddr>;
    /// Send message to specified node
    fn send_message(&mut self) -> Vec<(Option<PeerId>, usize, CitaRequest)>;
}

pub struct Service<Handle> {
    swarm: RawSwarm<
        Boxed<(PeerId, StreamMuxerBox)>,
        CITAInEvent,
        CITAOutEvent,
        CITANodeHandler<Substream<StreamMuxerBox>>,
    >,
    local_peer_id: PeerId,
    listening_address: Vec<Multiaddr>,
    /// Connected node information
    connected_nodes: HashMap<PeerId, NodeInfo>,
    need_connect: Vec<Multiaddr>,
    service_handle: Handle,
}

#[derive(Clone)]
struct NodeInfo {
    endpoint: Endpoint,
    address: Multiaddr,
}

impl<Handle> Service<Handle>
where
    Handle: ServiceHandle,
{
    /// Send message to specified node
    pub fn send_custom_message(&mut self, node: PeerId, protocol: usize, data: CitaRequest) {
        if let Some(mut connected) = self.swarm.peer(node.clone()).as_connected() {
            connected.send_event(CITAInEvent::SendCustomMessage { protocol, data });
        } else {
            println!("Try to send message to {:?}, but not connected", node);
        }
    }

    /// Send message to all node
    pub fn broadcast(&mut self, protocol: usize, data: CitaRequest) {
        self.swarm
            .broadcast_event(&CITAInEvent::SendCustomMessage { protocol, data });
    }

    /// Start listening on a multiaddr.
    #[inline]
    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<Multiaddr, Multiaddr> {
        match self.swarm.listen_on(addr) {
            Ok(mut addr) => {
                addr.append(Protocol::P2p(self.local_peer_id.clone().into()));
                Ok(addr)
            }
            Err(addr) => Err(addr),
        }
    }

    /// Start dialing an address.
    #[inline]
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), Multiaddr> {
        self.swarm.dial(addr, CITANodeHandler::new())
    }

    fn poll_swarm(&mut self) -> Poll<Option<ServiceEvent>, Error> {
        loop {
            match self.swarm.poll() {
                Async::Ready(event) => match event {
                    RawSwarmEvent::Connected { peer_id, endpoint } => {
                        let (address, endpoint) = match endpoint {
                            ConnectedPoint::Dialer { address } => (address, Endpoint::Dialer),
                            ConnectedPoint::Listener { send_back_addr, .. } => {
                                (send_back_addr, Endpoint::Listener)
                            }
                        };
                        self.connected_nodes
                            .insert(peer_id, NodeInfo { address, endpoint });
                    }
                    RawSwarmEvent::NodeEvent { peer_id, event } => match event {
                        CITAOutEvent::CustomProtocolOpen { protocol, version } => {
                            return Ok(Async::Ready(Some(ServiceEvent::CustomProtocolOpen {
                                id: peer_id,
                                protocol,
                                version,
                            })))
                        }
                        CITAOutEvent::CustomMessage { protocol, data } => {
                            return Ok(Async::Ready(Some(ServiceEvent::CustomMessage {
                                id: peer_id,
                                protocol,
                                data,
                            })))
                        }
                        CITAOutEvent::CustomProtocolClosed { protocol, .. } => {
                            return Ok(Async::Ready(Some(ServiceEvent::CustomProtocolClosed {
                                id: peer_id,
                                protocol,
                            })))
                        }
                        CITAOutEvent::Unresponsive => {
                            self.connected_nodes.remove(&peer_id);
                            return Ok(Async::Ready(Some(ServiceEvent::NodeClosed { id: peer_id })));
                        }
                        CITAOutEvent::PingStart => {
                            println!("ping start");
                        }
                        CITAOutEvent::PingSuccess(time) => {
                            println!("ping success on {:?}", time);
                        }
                    },
                    RawSwarmEvent::NodeClosed { peer_id, .. } => {
                        self.connected_nodes.remove(&peer_id);
                        return Ok(Async::Ready(Some(ServiceEvent::NodeClosed { id: peer_id })));
                    }
                    RawSwarmEvent::NodeError { peer_id, error, .. } => {
                        println!("node error: {}", error);
                        self.connected_nodes.remove(&peer_id);
                        return Ok(Async::Ready(Some(ServiceEvent::NodeClosed { id: peer_id })));
                    }
                    RawSwarmEvent::DialError {
                        multiaddr, error, ..
                    }
                    | RawSwarmEvent::UnknownPeerDialError {
                        multiaddr, error, ..
                    } => {
                        self.need_connect.push(multiaddr);
                        println!("dial error: {}", error);
                    }
                    RawSwarmEvent::IncomingConnection(incoming) => {
                        incoming.accept(CITANodeHandler::new());
                    }
                    RawSwarmEvent::IncomingConnectionError {
                        send_back_addr,
                        error,
                        ..
                    } => {
                        println!("node {} incoming error: {}", send_back_addr, error);
                    }
                    RawSwarmEvent::Replaced { peer_id, .. } => {
                        if let Some(info) = self.connected_nodes.remove(&peer_id) {
                            match info.endpoint {
                                Endpoint::Listener => {}
                                Endpoint::Dialer => self.need_connect.push(info.address),
                            }
                        }
                    }
                    RawSwarmEvent::ListenerClosed {
                        listen_addr,
                        result,
                        ..
                    } => {
                        println!("listener {} closed, result: {:?}", listen_addr, result);
                    }
                },
                Async::NotReady => return Ok(Async::NotReady),
            }
        }
    }
}

impl<Handle> Stream for Service<Handle>
where
    Handle: ServiceHandle,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<()>, Self::Error> {
        match self.poll_swarm()? {
            Async::Ready(value) => {
                self.service_handle.out_event(value);
                return Ok(Async::Ready(Some(())));
            }
            Async::NotReady => (),
        }
        self.service_handle.before_poll();
        if let Some(address) = self.service_handle.new_dialer() {
            if let Err(address) = self.swarm.dial(address, CITANodeHandler::new()) {
                self.need_connect.push(address);
            }
        }
        if let Some(address) = self.service_handle.new_listen() {
            if let Ok(address) = self.swarm.listen_on(address) {
                self.listening_address.push(address);
            }
        }
        self.service_handle
            .send_message()
            .into_iter()
            .for_each(|(id, protocol, data)| match id {
                Some(id) => self.send_custom_message(id, protocol, data),
                None => self.broadcast(protocol, data),
            });

        Ok(Async::NotReady)
    }
}

pub fn build_service<Handle: ServiceHandle>(
    local_private_key: secio::SecioKeyPair,
    service_handle: Handle,
) -> Service<Handle> {
    let local_public_key = local_private_key.clone().to_public_key();
    let local_peer_id = local_public_key.into_peer_id();
    let swarm = build_swarm(local_private_key);
    Service {
        swarm,
        local_peer_id,
        listening_address: Vec::new(),
        connected_nodes: HashMap::new(),
        need_connect: Vec::new(),
        service_handle,
    }
}

fn build_swarm(
    local_private_key: secio::SecioKeyPair,
) -> RawSwarm<
    Boxed<(PeerId, StreamMuxerBox)>,
    CITAInEvent,
    CITAOutEvent,
    CITANodeHandler<Substream<StreamMuxerBox>>,
> {
    let transport = build_transport(local_private_key);

    RawSwarm::new(transport)
}

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
