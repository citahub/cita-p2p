use futures::prelude::*;
use libp2p::core::nodes::handled_node::{NodeHandler, NodeHandlerEndpoint, NodeHandlerEvent};
use libp2p::core::{upgrade};
use libp2p::{ping};
use std::io::{Error, ErrorKind};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::timer::Interval;

//use crate::custom_proto::{
//    encode_decode::{Request, Response},
//    p2p_proto::{CustomProtocol, CustomProtocolSubstream},
//};

type NodePollResult = Poll<Option<NodeHandlerEvent<(), CITAOutEvent>>, Error>;

#[derive(Debug, Clone)]
pub enum CITAInEvent {
    Accept,
//    SendCustomMessage { protocol: usize, data: Request },
}

pub enum CITAOutEvent {
    Useless,
    PingStart,
    PingSuccess(Duration),
    NeedReDial,
    OverMaxConnection,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum UpgradePurpose {
    Ping,
}

/// Enum of all the possible protocols our service handles.
enum FinalUpgrade<Substream> {
    PingDialer(ping::protocol::PingDialer<Substream, ()>),
    PingListener(ping::protocol::PingListener<Substream>),
}

pub struct CITANodeHandler<Substream: AsyncRead + AsyncWrite> {
    /// Substream open for custom protocol
//    custom_protocols_substream: Option<CustomProtocolSubstream<Substream>>,
    /// Substream open for sending pings, if any.
    ping_out_substream: Option<ping::protocol::PingDialer<Substream, Instant>>,
    /// Substream open for receiving pings.
    ping_in_substreams: Option<ping::protocol::PingListener<Substream>>,
    /// Substream open for identify
//    identify_send_back: IdentifyBack,
    /// Substreams being upgraded on the listening side.
    upgrades_in_progress_listen: Option<libp2p::core::upgrade::InboundUpgradeApply<Substream, libp2p::ping::protocol::Ping<Instant>>>,
    /// Substreams being upgraded on the dialing side. Contrary to `upgrades_in_progress_listen`,
    /// these have a known purpose.
    upgrades_in_progress_dial: Option<libp2p::core::upgrade::OutboundUpgradeApply<Substream, libp2p::ping::protocol::Ping<Instant>>>,
    /// Every 30 second ping remote
    next_ping: Interval,
    /// Every 5 minute identify remote
//    next_identify: Interval,
    /// Upgrade purpose
    queued_dial_upgrades: bool,
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
        CITANodeHandler {
            ping_out_substream: None,
            ping_in_substreams: None,
            upgrades_in_progress_listen: None,
            upgrades_in_progress_dial: None,
            next_ping: Interval::new(
                Instant::now() + Duration::from_secs(3),
                Duration::from_secs(5),
            ),
            queued_dial_upgrades: false,
            num_out_user_must_open: 0,
            notify: None,
        }
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
        if !self.queued_dial_upgrades {
            self.queued_dial_upgrades = true;
            self.num_out_user_must_open += 1;

            if let Some(notify) = self.notify.take() {
                notify.notify();
            }
        }

        false
    }

    fn poll_upgrades_in_progress(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
        if let Some(mut in_progress) = self.upgrades_in_progress_listen.take() {
            match in_progress.poll() {
                Ok(Async::Ready(upgrade)) => {
                    self.ping_in_substreams = Some(upgrade);
                }
                Ok(Async::NotReady) => {
                    self.upgrades_in_progress_listen = Some(in_progress);
                }
                Err(err) => {
                    error!("While upgrading listener, error by: {:?}", err);
                    return Ok(Async::Ready(Some(CITAOutEvent::Useless)));
                }
            }
        }

        if let Some(mut in_progress) = self.upgrades_in_progress_dial.take() {
            match in_progress.poll() {
                Ok(Async::Ready(upgrade)) => {
                    self.queued_dial_upgrades = false;
                    self.ping_out_substream = Some(upgrade);
                    if self.ping_remote() {
                        return Ok(Async::Ready(Some(CITAOutEvent::PingStart)));
                    }
                }
                Ok(Async::NotReady) => self.upgrades_in_progress_dial = Some(in_progress),
                Err(err) => {
                    error!("While upgrading dial to {:?}: {:?}", UpgradePurpose::Ping, err);
                    return Ok(Async::Ready(Some(CITAOutEvent::Useless)));
                }
            }
        }

        Ok(Async::NotReady)
    }

    fn poll_ping(&mut self) -> Poll<Option<CITAOutEvent>, Error> {
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
                    self.queued_dial_upgrades = true;
                    self.num_out_user_must_open += 1;
                }
                Ok(Async::NotReady) => self.ping_out_substream = Some(ping_dialer),
                Err(err) => error!("Remote ping substream errored:  {:?}", err),
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
    type Error = Error;

    fn inject_substream(
        &mut self,
        substream: Substream,
        endpoint: NodeHandlerEndpoint<Self::OutboundOpenInfo>,
    ) {
        match endpoint {
            NodeHandlerEndpoint::Listener => {
                let upgrade = upgrade::apply_inbound(substream, ping::protocol::Ping::default());
                self.upgrades_in_progress_listen = Some(upgrade);
            }
            NodeHandlerEndpoint::Dialer(_) => {
                if !self.queued_dial_upgrades {
                    return;
                } else {
                    let upgrade = upgrade::apply_outbound(substream, ping::protocol::Ping::default());
                    self.upgrades_in_progress_dial = Some(upgrade);
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
//            CITAInEvent::SendCustomMessage { protocol, data } => {
//                self.send_custom_message(protocol, data);
//            }
        }
    }

    fn shutdown(&mut self) {
//        if let Some(ref mut custom) = self.custom_protocols_substream {
//            custom.shutdown();
//        }
        trace!("shutdown");
    }

    fn poll(&mut self) -> NodePollResult {
        match self.poll_upgrades_in_progress()? {
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

impl<Substream> Default for CITANodeHandler<Substream>
where
    Substream: AsyncRead + AsyncWrite + Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}
