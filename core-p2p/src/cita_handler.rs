use futures::prelude::*;
use libp2p::core::nodes::handled_node::{NodeHandler, NodeHandlerEndpoint, NodeHandlerEvent};
use libp2p::core::{upgrade, Endpoint, PublicKey};
use libp2p::{identify, ping, Multiaddr};
use parking_lot::Mutex;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::timer::Interval;

use crate::custom_proto::{
    encode_decode::{Request, Response},
    p2p_proto::{CustomProtocol, CustomProtocolSubstream},
};

type IdentifyBack = Arc<Mutex<Option<Box<Future<Item = (), Error = Error> + Send>>>>;
type DialUpgradeQueue<Substream> = Vec<(
    UpgradePurpose,
    Box<dyn Future<Item = FinalUpgrade<Substream>, Error = Error> + Send>,
)>;

type NodePollResult<Substream> = Poll<Option<NodeHandlerEvent<(), CITAOutEvent<Substream>>>, Error>;

#[derive(Debug, Clone)]
pub enum CITAInEvent {
    Accept,
    SendCustomMessage { protocol: usize, data: Request },
    Ping,
}

pub enum CITAOutEvent<Substream> {
    Useless,
    CustomProtocolOpen {
        protocol: usize,
        version: u8,
    },
    CustomProtocolClosed {
        protocol: usize,
        result: Result<(), Error>,
    },
    CustomMessage {
        protocol: usize,
        data: Response,
    },
    PingStart,
    PingSuccess(Duration),
    IdentificationRequest(IdentificationRequest<Substream>),
    Identified {
        /// Information of the remote.
        info: identify::IdentifyInfo,
        /// Address the remote observes us as.
        observed_addr: Multiaddr,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum UpgradePurpose {
    Custom(usize),
    //    Kad,
    Identify,
    Ping,
}

/// Enum of all the possible protocols our service handles.
enum FinalUpgrade<Substream> {
    //    Kad(KadConnecController, Box<Stream<Item = KadIncomingRequest, Error = IoError> + Send>),
    IdentifyListener(identify::IdentifySender<Substream>),
    IdentifyDialer(identify::IdentifyInfo, Multiaddr),
    PingDialer(ping::protocol::PingDialer<Substream, Instant>),
    PingListener(ping::protocol::PingListener<Substream>),
    Custom(CustomProtocolSubstream<Substream>),
}

impl<Substream> From<ping::protocol::PingOutput<Substream, Instant>> for FinalUpgrade<Substream> {
    fn from(out: ping::protocol::PingOutput<Substream, Instant>) -> Self {
        match out {
            ping::protocol::PingOutput::Ponger(ponger) => FinalUpgrade::PingListener(ponger),
            ping::protocol::PingOutput::Pinger(pinger) => FinalUpgrade::PingDialer(pinger),
        }
    }
}

impl<Substream> From<identify::IdentifyOutput<Substream>> for FinalUpgrade<Substream> {
    fn from(out: identify::IdentifyOutput<Substream>) -> Self {
        match out {
            identify::IdentifyOutput::RemoteInfo {
                info,
                observed_addr,
            } => FinalUpgrade::IdentifyDialer(info, observed_addr),
            identify::IdentifyOutput::Sender { sender } => FinalUpgrade::IdentifyListener(sender),
        }
    }
}

pub struct IdentificationRequest<Substream> {
    /// Where to store the future that sends back the information.
    identify_send_back: IdentifyBack,
    /// Object that sends back the information.
    sender: identify::IdentifySender<Substream>,
}

impl<Substream> IdentificationRequest<Substream> {
    pub fn respond(
        self,
        local_key: PublicKey,
        listen_addrs: Vec<Multiaddr>,
        remote_addr: &Multiaddr,
    ) where
        Substream: AsyncRead + AsyncWrite + Send + 'static,
    {
        let sender = self.sender.send(
            identify::IdentifyInfo {
                public_key: local_key,
                protocol_version: "cita/".to_owned(),
                agent_version: "cita/".to_owned(),
                listen_addrs,
                protocols: vec![
                    "custom".to_string(),
                    "ping".to_string(),
                    "identify".to_string(),
                ],
            },
            remote_addr,
        );

        *self.identify_send_back.lock() = Some(sender);
    }
}

pub struct CITANodeHandler<Substream> {
    /// Substream open for custom protocol
    custom_protocols_substream: Option<CustomProtocolSubstream<Substream>>,
    /// Substream open for sending pings, if any.
    ping_out_substream: Option<ping::protocol::PingDialer<Substream, Instant>>,
    /// Substream open for receiving pings.
    ping_in_substreams: Option<ping::protocol::PingListener<Substream>>,
    /// Substream open for identify
    identify_send_back: IdentifyBack,
    /// Substreams being upgraded on the listening side.
    upgrades_in_progress_listen:
        Vec<Box<dyn Future<Item = FinalUpgrade<Substream>, Error = Error> + Send>>,
    /// Substreams being upgraded on the dialing side. Contrary to `upgrades_in_progress_listen`,
    /// these have a known purpose.
    upgrades_in_progress_dial: DialUpgradeQueue<Substream>,
    /// Every 30 second ping remote
    next_ping: Interval,
    /// Every 5 minute identify remote
    next_identify: Interval,
    /// Upgrade purpose
    queued_dial_upgrades: Vec<UpgradePurpose>,
    /// A outbound substream that must be opened to the outside, one protocol must have at least one
    num_out_user_must_open: usize,
    /// Notify to poll task
    notify: Option<futures::task::Task>,
}

impl<Substream> CITANodeHandler<Substream>
where
    Substream: AsyncRead + AsyncWrite + Send + 'static,
{
    pub fn new() -> Self {
        let queued_dial_upgrades = vec![UpgradePurpose::Custom(0)];
        CITANodeHandler {
            custom_protocols_substream: None,
            ping_out_substream: None,
            ping_in_substreams: None,
            identify_send_back: Arc::new(Mutex::new(None)),
            upgrades_in_progress_listen: Vec::with_capacity(1),
            upgrades_in_progress_dial: Vec::with_capacity(1),
            next_ping: Interval::new(
                Instant::now() + Duration::from_secs(10),
                Duration::from_secs(30),
            ),
            next_identify: Interval::new(
                Instant::now() + Duration::from_secs(2),
                Duration::from_secs(60 * 5),
            ),
            queued_dial_upgrades,
            num_out_user_must_open: 1,
            notify: None,
        }
    }

    fn send_custom_message(&mut self, _protocol: usize, data: Request) {
        if let Some(ref mut custom) = self.custom_protocols_substream {
            custom.send_message(data);
        }
    }

    /// Prue upgrade queue
    fn prune_upgrade_queue(&mut self, purpose: UpgradePurpose) {
        self.upgrades_in_progress_dial
            .retain(|&(p, _)| p != purpose);
        self.queued_dial_upgrades.retain(|p| p != &purpose);
    }

    fn inject_fully_negotiated(
        &mut self,
        upgrade: FinalUpgrade<Substream>,
    ) -> Option<CITAOutEvent<Substream>> {
        match upgrade {
            FinalUpgrade::PingDialer(ping_dialer) => {
                self.prune_upgrade_queue(UpgradePurpose::Ping);
                self.ping_out_substream = Some(ping_dialer);
                if self.ping_remote() {
                    Some(CITAOutEvent::PingStart)
                } else {
                    None
                }
            }
            FinalUpgrade::PingListener(ping_listener) => {
                self.ping_in_substreams = Some(ping_listener);
                None
            }
            FinalUpgrade::IdentifyListener(sender) => {
                Some(CITAOutEvent::IdentificationRequest(IdentificationRequest {
                    sender,
                    identify_send_back: self.identify_send_back.clone(),
                }))
            }
            FinalUpgrade::IdentifyDialer(info, observed_addr) => {
                self.prune_upgrade_queue(UpgradePurpose::Identify);
                Some(CITAOutEvent::Identified {
                    info,
                    observed_addr,
                })
            }
            FinalUpgrade::Custom(proto) => {
                self.prune_upgrade_queue(UpgradePurpose::Custom(proto.protocol_id()));
                if self.custom_protocols_substream.is_some() {
                    return None;
                }
                let event = CITAOutEvent::CustomProtocolOpen {
                    protocol: proto.protocol_id(),
                    version: proto.protocol_version(),
                };
                self.custom_protocols_substream = Some(proto);
                Some(event)
            }
        }
    }

    fn has_upgrade_purpose(&self, purpose: &UpgradePurpose) -> bool {
        self.upgrades_in_progress_dial
            .iter()
            .any(|&(ref p, _)| p == purpose)
            || self.queued_dial_upgrades.iter().any(|p| p == purpose)
    }

    fn ping_remote(&mut self) -> bool {
        if let Some(ref mut pinger) = self.ping_out_substream {
            let now = Instant::now();
            pinger.ping(now);
            if let Some(notify) = self.notify.take() {
                notify.notify();
            }
            return true;
        }
        // Otherwise, ensure we have an upgrade for a ping substream in queue.
        if !self.has_upgrade_purpose(&UpgradePurpose::Ping) {
            self.queued_dial_upgrades.push(UpgradePurpose::Ping);
            self.num_out_user_must_open += 1;

            if let Some(notify) = self.notify.take() {
                notify.notify();
            }
        }

        false
    }

    fn poll_upgrades_in_progress(&mut self) -> Poll<Option<CITAOutEvent<Substream>>, Error> {
        for n in (0..self.upgrades_in_progress_listen.len()).rev() {
            let mut in_progress = self.upgrades_in_progress_listen.swap_remove(n);
            match in_progress.poll() {
                Ok(Async::Ready(upgrade)) => {
                    if let Some(event) = self.inject_fully_negotiated(upgrade) {
                        return Ok(Async::Ready(Some(event)));
                    }
                }
                Ok(Async::NotReady) => {
                    self.upgrades_in_progress_listen.push(in_progress);
                }
                Err(err) => {
                    error!("While upgrading listener, error by: {:?}", err);
                    return Ok(Async::Ready(Some(CITAOutEvent::Useless)));
                }
            }
        }

        for n in (0..self.upgrades_in_progress_dial.len()).rev() {
            let (purpose, mut in_progress) = self.upgrades_in_progress_dial.swap_remove(n);
            match in_progress.poll() {
                Ok(Async::Ready(upgrade)) => {
                    if let Some(event) = self.inject_fully_negotiated(upgrade) {
                        return Ok(Async::Ready(Some(event)));
                    }
                }
                Ok(Async::NotReady) => self.upgrades_in_progress_dial.push((purpose, in_progress)),
                Err(err) => {
                    error!("While upgrading dial to {:?}: {:?}", purpose, err);
                    return Ok(Async::Ready(Some(CITAOutEvent::Useless)));
                }
            }
        }

        Ok(Async::NotReady)
    }

    fn poll_ping(&mut self) -> Poll<Option<CITAOutEvent<Substream>>, Error> {
        // Interval ping remote
        match self.next_ping.poll() {
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(_)) => {
                if self.ping_remote() {
                    return Ok(Async::Ready(Some(CITAOutEvent::PingStart)));
                }
            }
            Err(err) => {
                return Err(Error::new(ErrorKind::Other, err));
            }
        }

        // Poll for answering pings.
        if let Some(mut ping) = self.ping_in_substreams.take() {
            match ping.poll() {
                Ok(Async::Ready(())) => {}
                Ok(Async::NotReady) => self.ping_in_substreams = Some(ping),
                Err(err) => error!("Remote ping substream errored:  {:?}", err),
            }
        }

        // Poll the ping substream.
        if let Some(mut ping_dialer) = self.ping_out_substream.take() {
            match ping_dialer.poll() {
                Ok(Async::Ready(Some(started))) => {
                    return Ok(Async::Ready(Some(CITAOutEvent::PingSuccess(
                        started.elapsed(),
                    ))));
                }
                Ok(Async::Ready(None)) => {
                    self.queued_dial_upgrades.push(UpgradePurpose::Ping);
                    self.num_out_user_must_open += 1;
                }
                Ok(Async::NotReady) => self.ping_out_substream = Some(ping_dialer),
                Err(err) => error!("Remote ping substream errored:  {:?}", err),
            }
        }

        Ok(Async::NotReady)
    }

    fn identify_remote(&mut self) {
        if !self.has_upgrade_purpose(&UpgradePurpose::Identify) {
            self.queued_dial_upgrades.push(UpgradePurpose::Identify);
            self.num_out_user_must_open += 1;
            if let Some(notify) = self.notify.take() {
                notify.notify();
            }
        }
    }

    fn poll_identify(&mut self) -> Poll<Option<CITAOutEvent<Substream>>, Error> {
        loop {
            match self.next_identify.poll() {
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(Some(_))) => self.identify_remote(),
                Ok(Async::Ready(None)) => {
                    return Ok(Async::Ready(None));
                }
                Err(err) => {
                    return Err(Error::new(ErrorKind::Other, err));
                }
            }
        }

        let mut identify_send_back = self.identify_send_back.lock();
        if let Some(mut identify) = identify_send_back.take() {
            match identify.poll() {
                Ok(Async::Ready(())) => {}
                Ok(Async::NotReady) => *identify_send_back = Some(identify),
                Err(err) => error!("Identify err: {:?}", err),
            }
        }

        Ok(Async::NotReady)
    }

    fn poll_custom_protocol(&mut self) -> Poll<Option<CITAOutEvent<Substream>>, Error> {
        if let Some(ref mut custom) = self.custom_protocols_substream {
            match custom.poll() {
                Ok(Async::NotReady) => {}
                Ok(Async::Ready(Some(data))) => {
                    let protocol = custom.protocol_id();
                    return Ok(Async::Ready(Some(CITAOutEvent::CustomMessage {
                        protocol,
                        data,
                    })));
                }
                Ok(Async::Ready(None)) => {
                    // Trying to reopen the protocol.
                    self.num_out_user_must_open += 1;
                    return Ok(Async::Ready(Some(CITAOutEvent::CustomProtocolClosed {
                        protocol: custom.protocol_id(),
                        result: Ok(()),
                    })));
                }
                Err(err) => {
                    // Trying to reopen the protocol.
                    self.queued_dial_upgrades
                        .push(UpgradePurpose::Custom(custom.protocol_id()));
                    self.num_out_user_must_open += 1;
                    return Ok(Async::Ready(Some(CITAOutEvent::CustomProtocolClosed {
                        protocol: custom.protocol_id(),
                        result: Err(err),
                    })));
                }
            }
        }
        Ok(Async::NotReady)
    }
}

impl<Substream> NodeHandler for CITANodeHandler<Substream>
where
    Substream: AsyncRead + AsyncWrite + Send + 'static,
{
    type InEvent = CITAInEvent;
    type OutEvent = CITAOutEvent<Substream>;
    type OutboundOpenInfo = ();
    type Substream = Substream;


    fn inject_substream(
        &mut self,
        substream: Substream,
        endpoint: NodeHandlerEndpoint<Self::OutboundOpenInfo>,
    ) {
        match endpoint {
            NodeHandlerEndpoint::Listener => {
                let custom_proto = CustomProtocol::new(0, &[0]);
                let config = upgrade::or(
                    upgrade::map(custom_proto, FinalUpgrade::Custom),
                    upgrade::or(
                        upgrade::map(identify::IdentifyProtocolConfig, move |proto| {
                            FinalUpgrade::from(proto)
                        }),
                        upgrade::map(ping::protocol::Ping::default(), move |proto| {
                            FinalUpgrade::from(proto)
                        }),
                    ),
                );
                let upgrade = upgrade::apply(substream, config, Endpoint::Listener);
                self.upgrades_in_progress_listen.push(Box::new(upgrade));
            }
            NodeHandlerEndpoint::Dialer(_) => {
                if self.queued_dial_upgrades.is_empty() {
                    return;
                } else {
                    let dial = self.queued_dial_upgrades.remove(0);
                    match dial {
                        UpgradePurpose::Custom(id) => {
                            let config =
                                upgrade::map(CustomProtocol::new(id, &[0]),
                                    FinalUpgrade::Custom);
                            let upgrade = upgrade::apply(substream, config, Endpoint::Dialer);
                            self.upgrades_in_progress_dial
                                .push((dial, Box::new(upgrade)));
                        }
                        UpgradePurpose::Ping => {
                            let config =
                                upgrade::map(ping::protocol::Ping::default(), move |proto| {
                                    FinalUpgrade::from(proto)
                                });
                            let upgrade = upgrade::apply(substream, config, Endpoint::Dialer);
                            self.upgrades_in_progress_dial
                                .push((dial, Box::new(upgrade)));
                        }
                        UpgradePurpose::Identify => {
                            let config = upgrade::map(identify::IdentifyProtocolConfig, move |i| {
                                FinalUpgrade::from(i)
                            });
                            let upgrade = upgrade::apply(substream, config, Endpoint::Dialer);
                            self.upgrades_in_progress_dial
                                .push((dial, Box::new(upgrade)));
                        }
                    }
                }
            }
        }
        if let Some(task) = self.notify.take() {
            task.notify();
        }
    }

    fn inject_inbound_closed(&mut self) {}

    fn inject_outbound_closed(&mut self, _user_data: Self::OutboundOpenInfo) {}

    fn inject_event(&mut self, event: Self::InEvent) {
        match event {
            CITAInEvent::Accept => {}
            CITAInEvent::SendCustomMessage { protocol, data } => {
                self.send_custom_message(protocol, data);
            }
            CITAInEvent::Ping => {
                self.ping_remote();
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(ref mut custom) = self.custom_protocols_substream {
            custom.shutdown();
        }
        trace!("shutdown");
    }

    fn poll(
        &mut self,
    ) -> NodePollResult<Substream> {
        match self.poll_upgrades_in_progress()? {
            Async::Ready(value) => return Ok(Async::Ready(value.map(NodeHandlerEvent::Custom))),
            Async::NotReady => (),
        };

        match self.poll_custom_protocol()? {
            Async::Ready(value) => return Ok(Async::Ready(value.map(NodeHandlerEvent::Custom))),
            Async::NotReady => (),
        };

        match self.poll_ping()? {
            Async::Ready(value) => return Ok(Async::Ready(value.map(NodeHandlerEvent::Custom))),
            Async::NotReady => (),
        };

        match self.poll_identify()? {
            Async::Ready(value) => return Ok(Async::Ready(value.map(NodeHandlerEvent::Custom))),
            Async::NotReady => (),
        };

        if self.num_out_user_must_open >= 1 {
            self.num_out_user_must_open -= 1;
            return Ok(Async::Ready(Some(
                NodeHandlerEvent::OutboundSubstreamRequest(()),
            )));
        }

        self.notify = Some(futures::task::current());
        Ok(Async::NotReady)
    }
}

impl<Substream> Default for CITANodeHandler<Substream>
where
    Substream: AsyncRead + AsyncWrite + Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}
