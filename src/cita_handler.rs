use std::io::{Error, ErrorKind};
use bytes::Bytes;
use std::time::{Instant, Duration};
use tokio::io::{AsyncRead, AsyncWrite};
use libp2p::ping;
use libp2p::core::nodes::handled_node::{NodeHandler, NodeHandlerEndpoint, NodeHandlerEvent};
use libp2p::core::{upgrade, Endpoint};
use futures::prelude::*;

#[derive(Debug, Clone)]
pub enum CITAInEvent {
    Accept,
    SendCustomMessage {
        protocol: usize,
        data: Vec<u8>,
    },
    Ping,
}

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
        data: Bytes,
    },
    PingStart,
    PingSuccess(Duration)
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
    Custom(()),
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
    /// Substream open for sending pings, if any.
    ping_out_substream: Option<ping::protocol::PingDialer<Substream, Instant>>,
    /// Substreams open for receiving pings.
    ping_in_substreams: Vec<ping::protocol::PingListener<Substream>>,
    /// Substreams being upgraded on the listening side.
    upgrades_in_progress_listen: Vec<Box<Future<Item = FinalUpgrade<Substream>, Error = Error> + Send>>,
    /// Substreams being upgraded on the dialing side. Contrary to `upgrades_in_progress_listen`,
    /// these have a known purpose.
    upgrades_in_progress_dial: Vec<(UpgradePurpose, Box<Future<Item = FinalUpgrade<Substream>, Error = Error> + Send>)>,

    queued_dial_upgrades: Vec<UpgradePurpose>,

    notify: Option<futures::task::Task>,
}

impl<Substream> CITANodeHandler<Substream>
    where Substream: AsyncRead + AsyncWrite + Send + 'static, {
    pub fn new() -> Self {
        let queued_dial_upgrades = vec![UpgradePurpose::Ping];
        CITANodeHandler {
            ping_out_substream: None,
            ping_in_substreams: Vec::with_capacity(1),
            upgrades_in_progress_listen: Vec::with_capacity(1),
            upgrades_in_progress_dial: Vec::with_capacity(1),
            queued_dial_upgrades,
            notify: None,
        }
    }

    fn send_custom_message(&mut self, protocol: usize, data: Vec<u8>) {

    }

    fn inject_fully_negotiated(&mut self, upgrade: FinalUpgrade<Substream>) -> Option<CITAOutEvent> {
        match upgrade {
            FinalUpgrade::PingDialer(ping_dialer) => {
                println!("22");
                self.upgrades_in_progress_dial.retain(|&(purp, _)| &purp != &UpgradePurpose::Ping);
                self.queued_dial_upgrades.retain(|u| u != &UpgradePurpose::Ping);
                self.ping_out_substream = Some(ping_dialer);
                if self.ping_remote() {
                    Some(CITAOutEvent::PingStart)
                } else {
                    None
                }
            }
            FinalUpgrade::PingListener(ping_listener) => {
                self.ping_in_substreams.push(ping_listener);
                None
            },
            _ => None
        }
    }

    fn has_upgrade_purpose(&self, purpose: &UpgradePurpose) -> bool {
        self.upgrades_in_progress_dial.iter().any(|&(ref p, _)| p == purpose) ||
            self.queued_dial_upgrades.iter().any(|p| p == purpose)
    }

    fn ping_remote(&mut self) -> bool {
        if let Some(ref mut pinger) = self.ping_out_substream {
            let now = Instant::now();
            pinger.ping(now);
            if let Some(notify) = self.notify.take() {
                notify.notify();
            }
            println!("ping start");
            return true;
        }
        // Otherwise, ensure we have an upgrade for a ping substream in queue.
        if !self.has_upgrade_purpose(&UpgradePurpose::Ping) {
            self.queued_dial_upgrades.push(UpgradePurpose::Ping);

            if let Some(notify) = self.notify.take() {
                notify.notify();
            }
        }
        println!("ping start fail");
        false
    }

    fn poll_upgrades_in_progress(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
        for n in (0 .. self.upgrades_in_progress_listen.len()).rev() {
            println!("44");
            let mut in_progress = self.upgrades_in_progress_listen.swap_remove(n);
            match in_progress.poll() {
                Ok(Async::Ready(upgrade)) => {
                    if let Some(event) = self.inject_fully_negotiated(upgrade) {
                        return Ok(Async::Ready(Some(event)));
                    }
                },
                Ok(Async::NotReady) => {
                    self.upgrades_in_progress_listen.push(in_progress);
                },
                Err(err) => {
                    println!("{}", err);
                    return Ok(Async::Ready(Some(CITAOutEvent::Unresponsive)));
                },
            }
        }

        for n in (0 .. self.upgrades_in_progress_dial.len()).rev() {
            println!("33");
            let (purpose, mut in_progress) = self.upgrades_in_progress_dial.swap_remove(n);
            match in_progress.poll() {
                Ok(Async::Ready(upgrade)) => {
                    if let Some(event) = self.inject_fully_negotiated(upgrade) {
                        return Ok(Async::Ready(Some(event)));
                    }
                },
                Ok(Async::NotReady) =>
                    self.upgrades_in_progress_dial.push((purpose, in_progress)),
                Err(err) => {
                    if let UpgradePurpose::Custom(_) = purpose {
                        return Ok(Async::Ready(Some(CITAOutEvent::Unresponsive)));
                    } else {
                        let msg = format!("While upgrading to {:?}: {:?}", purpose, err);
                        let err = Error::new(ErrorKind::Other, msg);
                        println!("{}", err);
                        return Ok(Async::Ready(Some(CITAOutEvent::Unresponsive)));
                    }
                },
            }
        }

        Ok(Async::NotReady)
    }

    fn poll_ping(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
        // Poll the ping substream.
        if let Some(mut ping_dialer) = self.ping_out_substream.take() {
            match ping_dialer.poll() {
                Ok(Async::Ready(Some(started))) => {
                    println!("1");
                    return Ok(Async::Ready(Some(CITAOutEvent::PingSuccess(started.elapsed()))));
                },
                Ok(Async::Ready(None)) => {
                    {}
                },
                Ok(Async::NotReady) => self.ping_out_substream = Some(ping_dialer),
                Err(_) => {},
            }
        }

        // Poll for answering pings.
        for n in (0 .. self.ping_in_substreams.len()).rev() {
            let mut ping = self.ping_in_substreams.swap_remove(n);
            match ping.poll() {
                Ok(Async::Ready(())) => {
                    println!("accept an ping");
                },
                Ok(Async::NotReady) => {
                    println!("accept an ping 1");
                    self.ping_in_substreams.push(ping)
                },
                Err(err) => println!("Remote ping substream errored:  {:?}", err),
            }
        }

        Ok(Async::NotReady)
    }
}

impl<Substream> NodeHandler for CITANodeHandler<Substream>
    where Substream: AsyncRead + AsyncWrite + Send + 'static,
{
    type InEvent = CITAInEvent;
    type OutEvent = CITAOutEvent;
    type OutboundOpenInfo = ();
    type Substream = Substream;

    fn inject_substream(&mut self, substream: Substream, endpoint: NodeHandlerEndpoint<Self::OutboundOpenInfo>) {
        println!("4");
        let config = upgrade::map(ping::protocol::Ping::default(), move |proto| FinalUpgrade::from(proto));
        match endpoint {
            NodeHandlerEndpoint::Listener => {
                let upgrade = upgrade::apply(substream, config, Endpoint::Listener);
                self.upgrades_in_progress_listen.push(Box::new(upgrade));
            }
            NodeHandlerEndpoint::Dialer(_) => {
                if self.queued_dial_upgrades.is_empty() {
                    return;
                } else {
                    let upgrade = upgrade::apply(substream, config, Endpoint::Dialer);
                    self.upgrades_in_progress_dial.push((self.queued_dial_upgrades.remove(0), Box::new(upgrade)));
                }
            }
        }
        if let Some(task) = self.notify.take() {
            task.notify();
        }
    }

    fn inject_inbound_closed(&mut self) {

    }

    fn inject_outbound_closed(&mut self, user_data: Self::OutboundOpenInfo) {

    }

    fn inject_event(&mut self, event: Self::InEvent) {
        match event {
            CITAInEvent::Accept => {},
            CITAInEvent::SendCustomMessage { protocol, data } => {
                self.send_custom_message(protocol, data);
            },
            CITAInEvent::Ping => {
                self.ping_remote();
            }
        }
    }

    fn shutdown(&mut self) {
        println!("5");
    }

    fn poll(
        &mut self
    ) -> Poll<Option<NodeHandlerEvent<Self::OutboundOpenInfo, Self::OutEvent>>, Error> {
//        println!("2");
        match self.poll_upgrades_in_progress()? {
            Async::Ready(value) => return Ok(Async::Ready(value.map(NodeHandlerEvent::Custom))),
            Async::NotReady => (),
        };

//        println!("3");
        match self.poll_ping()? {
            Async::Ready(value) => return Ok(Async::Ready(value.map(NodeHandlerEvent::Custom))),
            Async::NotReady => (),
        };

        self.notify = Some(futures::task::current());
        Ok(Async::NotReady)
    }
}
