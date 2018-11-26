use bytes::Bytes;
use crate::custom_proto::encode_decode::{Codec, Request, Response};
use futures::{self, prelude::*, stream, task};
use libp2p::core::{ConnectionUpgrade, Endpoint};
use std::{collections::VecDeque, vec::IntoIter};
use tokio::codec::{Decoder, Framed};
use tokio::io::{AsyncRead, AsyncWrite};

pub struct CustomProtocol {
    id: usize,
    name: Bytes,
    supported_versions: Vec<u8>,
}

impl CustomProtocol {
    pub fn new(protocol: usize, versions: &[u8]) -> Self {
        let mut name = Bytes::from_static(b"/cita/");
        name.extend_from_slice(&[protocol as u8]);
        name.extend_from_slice(b"/");

        CustomProtocol {
            name,
            id: protocol,
            supported_versions: {
                let mut tmp = versions.to_vec();
                tmp.sort_unstable_by(|a, b| b.cmp(&a));
                tmp
            },
        }
    }

    #[inline]
    pub fn id(&self) -> usize {
        self.id
    }
}

pub struct CustomProtocolSubstream<Substream> {
    is_closing: bool,
    /// Buffer to send
    send_queue: VecDeque<Request>,
    /// if true, we should call `poll_complete`
    requires_poll_complete: bool,
    /// The underlying substream
    inner: stream::Fuse<Framed<Substream, Codec>>,
    protocol_id: usize,
    protocol_version: u8,
    /// Task to notify when something is changed and must to be polled.
    notify: Option<task::Task>,
}

impl<Substream> CustomProtocolSubstream<Substream> {
    #[inline]
    pub fn protocol_id(&self) -> usize {
        self.protocol_id
    }

    #[inline]
    pub fn protocol_version(&self) -> u8 {
        self.protocol_version
    }

    pub fn shutdown(&mut self) {
        self.is_closing = true;
        if let Some(task) = self.notify.take() {
            task.notify();
        }
    }

    pub fn send_message(&mut self, data: Request) {
        self.send_queue.push_back(data);

        if let Some(task) = self.notify.take() {
            task.notify();
        }
    }
}

impl<Substream> Stream for CustomProtocolSubstream<Substream>
where
    Substream: AsyncRead + AsyncWrite,
{
    type Item = Response;
    type Error = ::std::io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.is_closing {
            return Ok(self.inner.close()?.map(|()| None));
        }

        // Flushing the local queue.
        while let Some(packet) = self.send_queue.pop_front() {
            match self.inner.start_send(Some(packet))? {
                AsyncSink::NotReady(Some(packet)) => {
                    self.send_queue.push_front(packet);
                    break;
                }
                AsyncSink::NotReady(None) => {
                    break;
                }
                AsyncSink::Ready => self.requires_poll_complete = true,
            }
        }

        // Flushing if necessary.
        if self.requires_poll_complete {
            if let Async::Ready(()) = self.inner.poll_complete()? {
                self.requires_poll_complete = false;
            }
        }

        // Receiving incoming packets.
        // Note that `inner` is wrapped in a `Fuse`, therefore we can poll it forever.
        #[cfg_attr(feature = "cargo-clippy", allow(never_loop))]
        loop {
            match self.inner.poll()? {
                Async::Ready(Some(data)) => return Ok(Async::Ready(Some(Some(data)))),
                Async::Ready(None) => if !self.requires_poll_complete && self.send_queue.is_empty()
                {
                    return Ok(Async::Ready(None));
                } else {
                    break;
                },
                Async::NotReady => break,
            }
        }

        self.notify = Some(task::current());
        Ok(Async::NotReady)
    }
}

impl<Substream> ConnectionUpgrade<Substream> for CustomProtocol
where
    Substream: AsyncRead + AsyncWrite,
{
    type NamesIter = IntoIter<(Bytes, Self::UpgradeIdentifier)>;
    type UpgradeIdentifier = u8; // Protocol version

    #[inline]
    fn protocol_names(&self) -> Self::NamesIter {
        self.supported_versions
            .iter()
            .map(|&ver| {
                let num = ver.to_string();
                let mut name = self.name.clone();
                name.extend_from_slice(num.as_bytes());
                (name, ver)
            }).collect::<Vec<_>>()
            .into_iter()
    }

    type Output = CustomProtocolSubstream<Substream>;
    type Future = futures::future::FutureResult<Self::Output, ::std::io::Error>;

    #[inline]
    fn upgrade(
        self,
        socket: Substream,
        protocol_version: Self::UpgradeIdentifier,
        _endpoint: Endpoint,
    ) -> Self::Future {
        let framed = Codec.framed(socket);

        futures::future::ok(CustomProtocolSubstream {
            is_closing: false,
            send_queue: VecDeque::new(),
            requires_poll_complete: false,
            inner: framed.fuse(),
            protocol_id: self.id,
            protocol_version,
            notify: None,
        })
    }
}
