use byteorder::{ByteOrder, NetworkEndian};
use bytes::BufMut;
use bytes::BytesMut;
use std::io;
use tokio::codec::{Decoder, Encoder};

pub type Request = Vec<u8>;
pub type Response = Option<Vec<u8>>;

/// Our multiplexed line-based codec
pub struct Codec;

/// Implementation of the multiplexed line-based protocol.
///
/// Frames begin with a 4 byte header, consisting of the numeric request ID
/// encoded in network order, followed by the frame payload encoded as a UTF-8
/// string and terminated with a '\n' character:
///
/// # An example frame:
///
/// +------------------------+--------------------------+
/// | Type                   | Content                  |
/// +------------------------+--------------------------+
/// | Symbol for Start       | \xDEADBEEF               |
/// | Length of Full Payload | u32                      |
/// +------------------------+--------------------------+
/// | Message                | a serialize data         |
/// +------------------------+--------------------------+
///

// Start of network messages.
const NETMSG_START: u64 = 0xDEAD_BEEF_0000_0000;

fn opt_bytes_extend(buf: &mut BytesMut, data: &[u8]) {
    buf.reserve(data.len());
    unsafe {
        buf.bytes_mut()[..data.len()].copy_from_slice(data);
        buf.advance_mut(data.len());
    }
}

impl Decoder for Codec {
    type Item = Request;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, io::Error> {
        Ok(network_message_to_pubsub_message(buf))
    }
}

impl Encoder for Codec {
    type Item = Response;
    type Error = io::Error;

    fn encode(&mut self, msg: Self::Item, buf: &mut BytesMut) -> io::Result<()> {
        pubsub_message_to_network_message(buf, msg);
        Ok(())
    }
}

pub fn pubsub_message_to_network_message(buf: &mut BytesMut, msg: Option<Vec<u8>>) {
    let mut request_id_bytes = [0; 8];

    if let Some(body) = msg {
        let length_full = body.len();
        if length_full > u32::max_value() as usize {
            //            error!(
            //                "The MQ message with key {} is too long {}.",
            //                key,
            //                body.len()
            //            );
            return;
        }
        let request_id = NETMSG_START + length_full as u64;
        NetworkEndian::write_u64(&mut request_id_bytes, request_id);
        opt_bytes_extend(buf, &request_id_bytes);
        opt_bytes_extend(buf, &body);
    } else {
        let request_id = NETMSG_START;
        NetworkEndian::write_u64(&mut request_id_bytes, request_id);
        opt_bytes_extend(buf, &request_id_bytes);
    }
}

pub fn network_message_to_pubsub_message(buf: &mut BytesMut) -> Option<Vec<u8>> {
    if buf.len() < 8 {
        return None;
    }

    let request_id = NetworkEndian::read_u64(buf.as_ref());
    let netmsg_start = request_id & 0xffff_ffff_0000_0000;
    let length_full = (request_id & 0x0000_0000_ffff_ffff) as usize;
    if netmsg_start != NETMSG_START {
        return None;
    }

    if length_full == 0 {
        return None;
    }

    if length_full + 8 > buf.len() {
        return None;
    }

    let _ = buf.split_to(8);
    let payload_buf = buf.split_to(length_full);

    Some(payload_buf.to_vec())
}

#[cfg(test)]
mod test {
    use super::{network_message_to_pubsub_message, pubsub_message_to_network_message};
    use bytes::BytesMut;

    #[test]
    fn convert_empty_message() {
        let mut buf = BytesMut::with_capacity(4 + 4);
        pubsub_message_to_network_message(&mut buf, None);
        let pub_msg_opt = network_message_to_pubsub_message(&mut buf);
        assert!(pub_msg_opt.is_none());
    }

    #[test]
    fn convert_messages() {
        let msg: Vec<u8> = vec![1, 3, 5, 7, 9];
        let mut buf = BytesMut::with_capacity(4 + 4 + msg.len());
        pubsub_message_to_network_message(&mut buf, Some(msg.clone()));
        let pub_msg_opt = network_message_to_pubsub_message(&mut buf);
        assert!(pub_msg_opt.is_some());
        let msg_new = pub_msg_opt.unwrap();
        assert_eq!(msg, msg_new);
    }
}
