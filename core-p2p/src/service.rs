use cita_handler::{CITAInEvent, CITANodeHandler, CITAOutEvent, IdentificationRequest};
use custom_proto::encode_decode::{Request, Response};
use futures::prelude::*;
use libp2p::core::{
    either,
    multiaddr::Protocol,
    muxing::StreamMuxerBox,
    nodes::raw_swarm::{ConnectedPoint, RawSwarm, RawSwarmEvent},
    nodes::Substream,
    transport::boxed::Boxed,
    upgrade, Endpoint, Multiaddr, PeerId, PublicKey, Transport,
};
use libp2p::{mplex, secio, yamux, TransportTimeout};
use std::collections::HashMap;
use std::io::Error;
use std::time::Duration;
use std::usize;
use tokio::io::{AsyncRead, AsyncWrite};

const MAX_OUTBOUND: usize = 30;
const MAX_INBOUND: usize = 30;

type P2PRawSwarm = RawSwarm<
    Boxed<(PeerId, StreamMuxerBox)>,
    CITAInEvent,
    CITAOutEvent<Substream<StreamMuxerBox>>,
    CITANodeHandler<Substream<StreamMuxerBox>>,
>;

#[derive(Debug)]
pub enum ServiceEvent {
    /// Closed connection to a node.
    NodeClosed {
        index: usize,
    },
    CustomProtocolOpen {
        index: usize,
        protocol: usize,
        version: u8,
        node_info: NodeInfo
    },
    CustomProtocolClosed {
        index: usize,
        protocol: usize,
    },
    CustomMessage {
        index: usize,
        protocol: usize,
        data: Response,
    },
    NodeInfo {
        index: usize,
        endpoint: Endpoint,
        listen_address: Vec<Multiaddr>,
    },
}

/// Service hook
pub trait ServiceHandle: Sync + Send + Stream<Item = (), Error = ()> {
    /// Send service event to out
    fn out_event(&self, event: Option<ServiceEvent>);
    /// Dialing a new address
    fn new_dialer(&mut self) -> Option<Multiaddr> {
        None
    }
    /// Listening a new address
    fn new_listen(&mut self) -> Option<Multiaddr> {
        None
    }
    /// Disconnect a peer
    fn disconnect(&mut self) -> Option<usize> {
        None
    }
    /// Send message to specified node
    fn send_message(&mut self) -> Vec<(Vec<usize>, usize, Request)> {
        Vec::new()
    }
}

/// Encapsulation of raw_swarm, providing external interfaces
pub struct Service<Handle> {
    swarm: P2PRawSwarm,
    local_public_key: PublicKey,
    local_peer_id: PeerId,
    listening_address: Vec<Multiaddr>,
    /// Connected node information
    connected_nodes: HashMap<usize, NodeInfo>,
    peer_index: HashMap<PeerId, usize>,
    need_connect: Vec<Multiaddr>,
    service_handle: Handle,
    next_index: usize,
    /// Number of inbound nodes
    outbound_num: usize,
    /// Number of outbound connections
    inbound_num: usize,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub id: PeerId,
    pub endpoint: Endpoint,
    pub address: Multiaddr,
}

impl<Handle> Service<Handle>
where
    Handle: ServiceHandle,
{
    /// Send message to specified node
    pub fn send_custom_message(&mut self, node: PeerId, protocol: usize, data: Request) {
        if let Some(mut connected) = self.swarm.peer(node).as_connected() {
            connected.send_event(CITAInEvent::SendCustomMessage { protocol, data });
        }
    }

    /// Send message to all node
    pub fn broadcast(&mut self, protocol: usize, data: Request) {
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

    /// Disconnect a peer by id
    #[inline]
    pub fn drop_node(&mut self, id: PeerId) -> Option<usize> {
        if let Some(index) = self.peer_index.remove(&id) {
            if let Some(info) = self.connected_nodes.remove(&index) {
                match info.endpoint {
                    Endpoint::Dialer => self.outbound_num -= 1,
                    Endpoint::Listener => self.inbound_num -= 1,
                }
            };
            if let Some(connected) = self.swarm.peer(id).as_connected() {
                connected.close();
            }
            Some(index)
        } else {
            None
        }
    }

    /// Index to peer id
    #[inline]
    pub fn get_index_by_id(&self, id: &PeerId) -> Option<&usize> {
        self.peer_index.get(id)
    }

    /// Peer id to index
    #[inline]
    pub fn get_info_by_index(&self, id: usize) -> Option<&NodeInfo> {
        self.connected_nodes.get(&id)
    }

    /// Service handle process
    #[inline]
    fn handle_hook(&mut self) {
        while let Some(address) = self.service_handle.new_dialer() {
            if let Err(address) = self.swarm.dial(address, CITANodeHandler::new()) {
                self.need_connect.push(address);
            }
        }
        while let Some(address) = self.service_handle.new_listen() {
            let _ = self.swarm.listen_on(address);
        }
        while let Some(index) = self.service_handle.disconnect() {
            if let Some(info) = self.get_info_by_index(index).cloned() {
                self.drop_node(info.id);
            }
        }
        self.service_handle
            .send_message()
            .into_iter()
            .for_each(|(indexes, protocol, data)| {
                if indexes.is_empty() {
                    self.broadcast(protocol, data)
                } else {
                    indexes.into_iter().for_each(|index| {
                        if let Some(info) = self.get_info_by_index(index).cloned() {
                            self.send_custom_message(info.id, protocol, data.clone())
                        } else {
                            debug!("Try to send message to {:?}, but not connected", index);
                        }
                    })
                }
            });
    }

    /// Identify respond
    fn respond_to_identify_request<Substream>(
        &mut self,
        requester: &PeerId,
        responder: IdentificationRequest<Substream>,
    ) where
        Substream: AsyncRead + AsyncWrite + Send + 'static,
    {
        if let Some(index) = self.get_index_by_id(&requester) {
            responder.respond(
                self.local_public_key.clone(),
                self.listening_address.clone(),
                &self.get_info_by_index(*index).unwrap().address,
            )
        }
    }

    fn add_observed_addr(&mut self, address: &Multiaddr) {
        for mut addr in self.swarm.nat_traversal(&address) {
            // Ignore addresses we already know about.
            if self.listening_address.iter().any(|a| a == &addr) {
                continue;
            }

            self.listening_address.push(addr.clone());
            addr.append(Protocol::P2p(self.local_peer_id.clone().into()));
        }
    }

    /// All Custom Event to throw process
    fn event_handle<Substream>(
        &mut self,
        peer_id: PeerId,
        event: CITAOutEvent<Substream>,
    ) -> Option<ServiceEvent>
    where
        Substream: AsyncRead + AsyncWrite + Send + 'static,
    {
        match event {
            CITAOutEvent::CustomProtocolOpen { protocol, version } => {
                let index = *self.get_index_by_id(&peer_id).unwrap();
                let node_info = self.get_info_by_index(index).unwrap().to_owned();
                Some(ServiceEvent::CustomProtocolOpen {
                    index: index,
                    protocol,
                    version,
                    node_info
                })
            }
            CITAOutEvent::CustomMessage { protocol, data } => Some(ServiceEvent::CustomMessage {
                index: *self.get_index_by_id(&peer_id).unwrap(),
                protocol,
                data,
            }),
            // if custom protocol close, the node must drop
            CITAOutEvent::CustomProtocolClosed { protocol, .. } => {
                let index = self.drop_node(peer_id).unwrap();
                Some(ServiceEvent::CustomProtocolClosed { index, protocol })
            }
            // if node is useless, must drop
            CITAOutEvent::Useless => {
                let index = self.drop_node(peer_id).unwrap();
                Some(ServiceEvent::NodeClosed { index })
            }
            CITAOutEvent::PingStart => {
                debug!("ping start");
                None
            }
            CITAOutEvent::PingSuccess(time) => {
                debug!("ping success on {:?}", time);
                None
            }
            CITAOutEvent::IdentificationRequest(request) => {
                self.respond_to_identify_request(&peer_id, request);
                None
            }
            CITAOutEvent::Identified {
                info,
                observed_addr,
            } => {
                self.add_observed_addr(&observed_addr);
                let index = *self.get_index_by_id(&peer_id).unwrap();
                let endpoint = self.connected_nodes[&index].endpoint;
                Some(ServiceEvent::NodeInfo {
                    index,
                    endpoint,
                    listen_address: info.listen_addrs,
                })
            }
            CITAOutEvent::NeedReDial => {
                while let Some(addr) = self.need_connect.pop() {
                    let _ = self.dial(addr);
                }
                None
            }
            CITAOutEvent::OverMaxConnection => {
                self.drop_node(peer_id);
                self.next_index -= 1;
                None
            }
        }
    }

    /// Poll raw swarm, throw corresponding event
    fn poll_swarm(&mut self) -> Poll<Option<ServiceEvent>, Error> {
        loop {
            let (id, event) = match self.swarm.poll() {
                Async::Ready(event) => match event {
                    RawSwarmEvent::Connected { peer_id, endpoint } => {
                        let (address, endpoint) = match endpoint {
                            ConnectedPoint::Dialer { address } => {
                                self.outbound_num += 1;
                                (address, Endpoint::Dialer)
                            }
                            ConnectedPoint::Listener { send_back_addr, .. } => {
                                self.inbound_num += 1;
                                (send_back_addr, Endpoint::Listener)
                            }
                        };
                        self.connected_nodes.insert(
                            self.next_index,
                            NodeInfo {
                                id: peer_id.clone(),
                                address,
                                endpoint,
                            },
                        );
                        self.peer_index.insert(peer_id.clone(), self.next_index);
                        // may be overflow?
                        self.next_index += 1;

                        if self.outbound_num < MAX_INBOUND || self.outbound_num < MAX_OUTBOUND {
                            continue;
                        } else {
                            // Disconnected more than the maximum number of connections
                            (peer_id, CITAOutEvent::OverMaxConnection)
                        }
                    }
                    RawSwarmEvent::NodeEvent { peer_id, event } => (peer_id, event),
                    RawSwarmEvent::NodeClosed { peer_id, .. } => (peer_id, CITAOutEvent::Useless),
                    RawSwarmEvent::NodeError { peer_id, error, .. } => {
                        error!("node error: {:?}", error);
                        (peer_id, CITAOutEvent::Useless)
                    }
                    RawSwarmEvent::DialError {
                        multiaddr, error, ..
                    }
                    | RawSwarmEvent::UnknownPeerDialError {
                        multiaddr, error, ..
                    } => {
                        error!("Dial err: {}", error);
                        self.need_connect.push(multiaddr);
                        (self.local_peer_id.clone(), CITAOutEvent::NeedReDial)
                    }
                    RawSwarmEvent::IncomingConnection(incoming) => {
                        incoming.accept(CITANodeHandler::new());
                        continue;
                    }
                    RawSwarmEvent::IncomingConnectionError {
                        send_back_addr,
                        error,
                        ..
                    } => {
                        error!("node {} incoming error: {:?}", send_back_addr, error);
                        continue;
                    }
                    RawSwarmEvent::Replaced { peer_id, .. } => {
                        if let Some(index) = self.peer_index.remove(&peer_id) {
                            self.connected_nodes.remove(&index);
                            if let Some(info) = self.connected_nodes.remove(&index) {
                                match info.endpoint {
                                    Endpoint::Listener => {}
                                    Endpoint::Dialer => self.need_connect.push(info.address),
                                }
                            }
                        }
                        (peer_id, CITAOutEvent::NeedReDial)
                    }
                    RawSwarmEvent::ListenerClosed {
                        listen_addr,
                        result,
                        ..
                    } => {
                        error!("listener {} closed, result: {:?}", listen_addr, result);
                        continue;
                    }
                },
                Async::NotReady => return Ok(Async::NotReady),
            };

            if let Some(event) = self.event_handle(id, event) {
                return Ok(Async::Ready(Some(event)));
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

        let _ = self.service_handle.poll();
        self.handle_hook();

        Ok(Async::NotReady)
    }
}

/// Create a new service
pub fn build_service<Handle: ServiceHandle>(
    key_pair: secio::SecioKeyPair,
    service_handle: Handle,
) -> Service<Handle> {
    let local_public_key = key_pair.clone().to_public_key();
    let local_peer_id = local_public_key.clone().into_peer_id();
    let swarm = build_swarm(key_pair);
    Service {
        swarm,
        local_public_key,
        local_peer_id,
        listening_address: Vec::new(),
        connected_nodes: HashMap::new(),
        peer_index: HashMap::new(),
        need_connect: Vec::new(),
        service_handle,
        next_index: 0,
        inbound_num: 0,
        outbound_num: 0,
    }
}

fn build_swarm(key_pair: secio::SecioKeyPair) -> P2PRawSwarm {
    let transport = build_transport(key_pair);

    RawSwarm::new(transport)
}

fn build_transport(key_pair: secio::SecioKeyPair) -> Boxed<(PeerId, StreamMuxerBox)> {
    let mut mplex_config = mplex::MplexConfig::new();
    mplex_config.max_buffer_len_behaviour(mplex::MaxBufferBehaviour::Block);
    mplex_config.max_buffer_len(usize::MAX);

    let base = libp2p::CommonTransport::new()
        .with_upgrade(secio::SecioConfig::new(key_pair))
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
