//! Carrying `xfer` transfers in protocol messages.

use proto::stream::Msg;
use xfer::{Endpoint, Outgoing};

/// Writes what an endpoint wants to send as a protocol message into `buf`
/// (at least `MAX_PLAIN` bytes) and returns its length.
pub fn encode(o: &Outgoing, buf: &mut [u8]) -> usize {
    match o {
        Outgoing::Block { id, index, payload } => Msg::Xfer { id: *id, index: *index, payload }.encode(buf),
        Outgoing::Ack { id, next, sack } => Msg::XferAck { id: *id, next: *next, sack: *sack }.encode(buf),
        Outgoing::Cancel { id, reason } => Msg::XferCancel { id: *id, reason: *reason }.encode(buf),
    }
}

/// Gives a transfer message to the endpoint; returns what to send back
/// (a refusal), or `None` if `msg` isn't a transfer message.
pub fn handle(endpoint: &mut Endpoint, now: u64, msg: &Msg) -> Option<Vec<Outgoing>> {
    Some(match *msg {
        Msg::Xfer { id, index, payload } => endpoint.on_block(now, id, index, payload),
        Msg::XferAck { id, next, sack } => {
            endpoint.on_ack(now, id, next, sack);
            Vec::new()
        }
        Msg::XferCancel { id, reason } => {
            endpoint.on_cancel(id, reason);
            Vec::new()
        }
        _ => return None,
    })
}

// A full block plus the message header fits in a datagram.
const _: () = assert!(xfer::BLOCK + 11 <= proto::MAX_PLAIN);

#[cfg(test)]
mod tests {
    use super::*;
    use xfer::{Bytes, Kind, Meta};

    #[test]
    fn a_block_survives_the_trip_through_a_message() {
        let mut a = Endpoint::new(1 << 20, 10_000_000, |_| Err(std::io::Error::other("unused")));
        a.send(Meta { kind: Kind::Clipboard, size: 5, name: String::new() }, Box::new(Bytes(b"hello".to_vec())));
        let mut out = Vec::new();
        a.poll(1_000_000, &mut out);
        let mut buf = [0u8; proto::MAX_PLAIN];
        let n = encode(&out[0], &mut buf);
        let Some(Msg::Xfer { id, index, payload }) = Msg::decode(&buf[..n]) else { panic!("not a block") };
        assert_eq!((id, index), (1, 0));
        assert_eq!(xfer::Meta::decode(payload).unwrap().size, 5);
    }
}
