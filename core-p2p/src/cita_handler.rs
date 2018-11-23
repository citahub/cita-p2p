use futures::prelude::*;
use libp2p::core::nodes::handled_node::{NodeHandler, NodeHandlerEndpoint, NodeHandlerEvent};
use libp2p::core::{upgrade, Endpoint};
use libp2p::ping;
use std::io::{Error, ErrorKind};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::timer::Delay;

use crate::custom_proto::{
    p2p_proto::{Request, Response},
    custom_proto::{CustomProtocol, CustomProtocolSubstream},
};

#[derive(Debug, Clone)]
pub enum CITAInEvent {
    Accept,
    SendCustomMessage { protocol: usize, data: Request },
    Ping,
}
#[derive(Debug)]
pub enum CITAOutEvent {
    Unresponsive,
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
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum UpgradePurpose {
    Custom(usize),
    //    Kad,
    //    Identify,
    Ping,
}

/// Enum of all the possible protocols our service handles.
enum FinalUpgrade<Substream> {
    //    Kad(KadConnecController, Box<Stream<Item = KadIncomingRequest, Error = IoError> + Send>),
    //    IdentifyListener(identify::IdentifySender<Substream>),
    //    IdentifyDialer(identify::IdentifyInfo, libp2p::Multiaddr),
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

pub struct CITANodeHandler<Substream> {
    /// Substream open for custom protocol
    custom_protocols_substream: Option<CustomProtocolSubstream<Substream>>,
    /// Substream open for sending pings, if any.
    ping_out_substream: Option<ping::protocol::PingDialer<Substream, Instant>>,
    /// Substreams open for receiving pings.
    ping_in_substreams: Option<ping::protocol::PingListener<Substream>>,
    /// Substreams being upgraded on the listening side.
    upgrades_in_progress_listen:
        Vec<Box<dyn Future<Item = FinalUpgrade<Substream>, Error = Error> + Send>>,
    /// Substreams being upgraded on the dialing side. Contrary to `upgrades_in_progress_listen`,
    /// these have a known purpose.
    upgrades_in_progress_dial: Vec<(
        UpgradePurpose,
        Box<dyn Future<Item = FinalUpgrade<Substream>, Error = Error> + Send>,
    )>,

    next_ping: Delay,
    queued_dial_upgrades: Vec<UpgradePurpose>,

    num_out_user_must_open: usize,

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
            upgrades_in_progress_listen: Vec::with_capacity(1),
            upgrades_in_progress_dial: Vec::with_capacity(1),
            next_ping: Delay::new(Instant::now() + Duration::from_secs(10)),
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

    fn inject_fully_negotiated(
        &mut self,
        upgrade: FinalUpgrade<Substream>,
    ) -> Option<CITAOutEvent> {
        match upgrade {
            FinalUpgrade::PingDialer(ping_dialer) => {
                self.upgrades_in_progress_dial
                    .retain(|&(purp, _)| &purp != &UpgradePurpose::Ping);
                self.queued_dial_upgrades
                    .retain(|u| u != &UpgradePurpose::Ping);
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
            FinalUpgrade::Custom(proto) => {
                self.upgrades_in_progress_dial
                    .retain(|&(purp, _)| &purp != &UpgradePurpose::Custom(proto.protocol_id()));
                self.queued_dial_upgrades
                    .retain(|u| u != &UpgradePurpose::Custom(proto.protocol_id()));
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

    fn poll_upgrades_in_progress(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
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
                    println!("{}", err);
                    return Ok(Async::Ready(Some(CITAOutEvent::Unresponsive)));
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
                    if let UpgradePurpose::Custom(_) = purpose {
                        return Ok(Async::Ready(Some(CITAOutEvent::Unresponsive)));
                    } else {
                        let msg = format!("While upgrading to {:?}: {:?}", purpose, err);
                        let err = Error::new(ErrorKind::Other, msg);
                        println!("{}", err);
                        return Ok(Async::Ready(Some(CITAOutEvent::Unresponsive)));
                    }
                }
            }
        }

        Ok(Async::NotReady)
    }

    fn poll_ping(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
        // Interval ping remote
        match self.next_ping.poll() {
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(())) => {
                self.next_ping
                    .reset(Instant::now() + Duration::from_secs(10));
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
                Err(err) => println!("2 Remote ping substream errored:  {:?}", err),
            }
        }

        // Poll the ping substream.
        if let Some(mut ping_dialer) = self.ping_out_substream.take() {
            match ping_dialer.poll() {
                Ok(Async::Ready(Some(started))) => {
                    self.next_ping
                        .reset(Instant::now() + Duration::from_secs(10));
                    return Ok(Async::Ready(Some(CITAOutEvent::PingSuccess(
                        started.elapsed(),
                    ))));
                }
                Ok(Async::Ready(None)) => {
                    self.queued_dial_upgrades.push(UpgradePurpose::Ping);
                    self.num_out_user_must_open += 1;
                }
                Ok(Async::NotReady) => self.ping_out_substream = Some(ping_dialer),
                Err(err) => println!("1 Remote ping substream errored:  {:?}", err),
            }
        }

        Ok(Async::NotReady)
    }

    fn poll_custom_protocol(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
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
    type OutEvent = CITAOutEvent;
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
                    upgrade::map(custom_proto, move |proto| FinalUpgrade::Custom(proto)),
                    upgrade::map(ping::protocol::Ping::default(), move |proto| {
                        FinalUpgrade::from(proto)
                    }),
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
                                upgrade::map(CustomProtocol::new(id, &[0]), move |proto| {
                                    FinalUpgrade::Custom(proto)
                                });
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
                                .push((dial, Box::new(upgrade) as Box<_>));
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
        println!("shutdown");
    }

    fn poll(
        &mut self,
    ) -> Poll<Option<NodeHandlerEvent<Self::OutboundOpenInfo, Self::OutEvent>>, Error> {
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
