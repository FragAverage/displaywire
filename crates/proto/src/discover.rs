//! Finding servers on the local network: a client broadcasts a query, servers
//! answer with their name, port and protocol version. Nothing else is sent to
//! someone who hasn't authenticated. (mDNS would be the usual choice, but
//! Windows' own DNS client holds its port.)

use crate::{MAGIC, Reader, Writer};

const QUERY: u8 = 0x30;
const REPLY: u8 = 0x31;
/// Queries are padded to this, and replies are always shorter: a spoofed query
/// can't be used to aim more traffic at someone than it cost to send.
pub const QUERY_LEN: usize = 64;
/// Longest server name sent.
pub const MAX_NAME: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    pub port: u16,
    pub version: u16,
    pub name: String,
}

pub fn query(nonce: [u8; 8]) -> [u8; QUERY_LEN] {
    let mut buf = [0u8; QUERY_LEN];
    let mut w = Writer::new(&mut buf);
    w.bytes(&MAGIC);
    w.u8(QUERY);
    w.bytes(&nonce);
    buf
}

/// The nonce of a discovery query, if `datagram` is one.
pub fn parse_query(datagram: &[u8]) -> Option<[u8; 8]> {
    if datagram.len() != QUERY_LEN {
        return None;
    }
    let mut r = Reader::new(datagram);
    r.magic()?;
    (r.u8()? == QUERY).then_some(())?;
    r.take()
}

pub fn reply(nonce: [u8; 8], a: &Announcement, out: &mut [u8]) -> usize {
    let name = &a.name.as_bytes()[..a.name.len().min(MAX_NAME)];
    let mut w = Writer::new(out);
    w.bytes(&MAGIC);
    w.u8(REPLY);
    w.bytes(&nonce);
    w.u16(a.port);
    w.u16(a.version);
    w.u8(name.len() as u8);
    w.bytes(name);
    w.len()
}

pub fn parse_reply(datagram: &[u8]) -> Option<([u8; 8], Announcement)> {
    let mut r = Reader::new(datagram);
    r.magic()?;
    (r.u8()? == REPLY).then_some(())?;
    let nonce = r.take()?;
    let (port, version) = (r.u16()?, r.u16()?);
    let len = r.u8()? as usize;
    let name = String::from_utf8_lossy(r.rest().get(..len)?).into_owned();
    Some((nonce, Announcement { port, version, name }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_sizes() {
        let q = query([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(parse_query(&q), Some([1, 2, 3, 4, 5, 6, 7, 8]));
        let a = Announcement { port: 9000, version: 5, name: "GAMING-PC".into() };
        let mut out = [0u8; 128];
        let n = reply([1, 2, 3, 4, 5, 6, 7, 8], &a, &mut out);
        assert_eq!(parse_reply(&out[..n]), Some(([1, 2, 3, 4, 5, 6, 7, 8], a)));
        // The longest possible reply is still smaller than a query.
        let long = Announcement { port: 1, version: 1, name: "x".repeat(200) };
        assert!(reply([0; 8], &long, &mut out) < QUERY_LEN);
    }

    #[test]
    fn rejects_other_datagrams() {
        assert_eq!(parse_query(b"DW\x30"), None);
        assert_eq!(parse_query(&[0u8; QUERY_LEN]), None);
        assert_eq!(parse_reply(b"DW\x31short"), None);
        let q = query([0; 8]);
        assert_eq!(parse_reply(&q), None);
    }
}
