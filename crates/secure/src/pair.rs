//! PIN pairing: how a new device gets a key of its own. See `docs/PAIRING.md`.
//!
//! A 6-digit PIN is far too small to protect an ordinary handshake: anyone who recorded
//! it could try all million PINs offline in a moment. So the PIN is the password of a
//! PAKE (SPAKE2), which makes every guess cost a live exchange with the server. The
//! exchange yields a shared secret that confirms both sides know the PIN and then
//! carries the new key to the client, sealed.
//!
//! ```text
//! client -> server   REQ      pid, name                       "I'd like to pair"
//! server -> client   STATUS   Waiting / PinRequired / Denied  (the person at the PC decides)
//! client -> server   MSG_A    pid, SPAKE2 message             (once the PIN is typed)
//! server -> client   MSG_B    pid, SPAKE2 message
//! client -> server   CONFIRM  pid, tag                        (proves the client knows the PIN)
//! server -> client   DONE     pid, sealed(new key, name)      (proves the server does)
//!                    STATUS   WrongPin(tries left) / Locked   (instead of DONE)
//! ```

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::{Error, Key, MAGIC};

pub const REQ: u8 = 0x40;
pub const STATUS: u8 = 0x41;
pub const MSG_A: u8 = 0x42;
pub const MSG_B: u8 = 0x43;
pub const CONFIRM: u8 = 0x44;
pub const DONE: u8 = 0x45;
pub const CANCEL: u8 = 0x46;

/// Whether `kind` is one of the pairing datagram types.
pub fn is_pairing(kind: u8) -> bool {
    (REQ..=CANCEL).contains(&kind)
}

/// Names the pairing in every datagram, chosen by the client.
pub type Pid = [u8; 8];

pub const NAME_MAX: usize = 32;
pub const PIN_DIGITS: usize = 6;
/// A SPAKE2 message: a group element and its prefix byte.
const SPAKE_LEN: usize = 33;
const TAG_LEN: usize = 32;
const ID_CLIENT: &[u8] = b"displaywire-client";
const ID_SERVER: &[u8] = b"displaywire-server";

pub fn new_pid() -> Pid {
    let mut p = [0u8; 8];
    getrandom::fill(&mut p).expect("system random number generator");
    p
}

/// A random PIN, uniform over 000000..=999999.
pub fn random_pin() -> String {
    loop {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("system random number generator");
        let n = u32::from_le_bytes(b);
        // Reject the top sliver so every PIN is equally likely.
        if n < u32::MAX - u32::MAX % 1_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

/// Whether `pin` is exactly six digits.
pub fn valid_pin(pin: &str) -> bool {
    pin.len() == PIN_DIGITS && pin.bytes().all(|b| b.is_ascii_digit())
}

/// A device name as sent and stored: printable, trimmed, at most `NAME_MAX` bytes.
pub fn clean_name(name: &str) -> String {
    let mut s: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    while s.len() > NAME_MAX {
        s.pop();
    }
    if s.is_empty() { "Unnamed device".into() } else { s }
}

/// What the server tells a client who asked to pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Waiting for the person at the PC to allow or deny.
    Waiting,
    /// Allowed: the PC shows a PIN, type it here.
    PinRequired,
    Denied,
    /// Someone else is pairing right now.
    Busy,
    /// Too many wrong PINs, or the PIN timed out.
    Locked,
    /// Wrong PIN, and how many tries are left.
    WrongPin(u8),
}

impl Status {
    fn code(self) -> (u8, u8) {
        match self {
            Status::Waiting => (0, 0),
            Status::PinRequired => (1, 0),
            Status::Denied => (2, 0),
            Status::Busy => (3, 0),
            Status::Locked => (4, 0),
            Status::WrongPin(n) => (5, n),
        }
    }

    fn from_code(code: u8, arg: u8) -> Option<Status> {
        Some(match code {
            0 => Status::Waiting,
            1 => Status::PinRequired,
            2 => Status::Denied,
            3 => Status::Busy,
            4 => Status::Locked,
            5 => Status::WrongPin(arg),
            _ => return None,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Req { pid: Pid, name: &'a str },
    Status { pid: Pid, status: Status },
    MsgA { pid: Pid, msg: &'a [u8] },
    MsgB { pid: Pid, msg: &'a [u8] },
    Confirm { pid: Pid, tag: [u8; TAG_LEN] },
    Done { pid: Pid, sealed: &'a [u8] },
    Cancel { pid: Pid },
}

fn start(kind: u8, pid: &Pid, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&MAGIC);
    out.push(kind);
    out.extend_from_slice(pid);
}

pub fn encode_req(pid: &Pid, name: &str) -> Vec<u8> {
    let name = clean_name(name);
    let mut out = Vec::new();
    start(REQ, pid, &mut out);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out
}

pub fn encode_status(pid: &Pid, status: Status) -> Vec<u8> {
    let (code, arg) = status.code();
    let mut out = Vec::new();
    start(STATUS, pid, &mut out);
    out.extend_from_slice(&[code, arg]);
    out
}

fn encode_body(kind: u8, pid: &Pid, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    start(kind, pid, &mut out);
    out.extend_from_slice(body);
    out
}

pub fn encode_cancel(pid: &Pid) -> Vec<u8> {
    encode_body(CANCEL, pid, &[])
}

pub fn decode(datagram: &[u8]) -> Option<Packet<'_>> {
    if datagram.len() < 3 + 8 || datagram[..2] != MAGIC {
        return None;
    }
    let kind = datagram[2];
    let pid: Pid = datagram[3..11].try_into().ok()?;
    let body = &datagram[11..];
    Some(match kind {
        REQ => {
            let (&len, rest) = body.split_first()?;
            let name = std::str::from_utf8(rest.get(..len as usize)?).ok()?;
            if len as usize != rest.len() || name.len() > NAME_MAX {
                return None;
            }
            Packet::Req { pid, name }
        }
        STATUS => Packet::Status { pid, status: Status::from_code(*body.first()?, *body.get(1)?)? },
        MSG_A if body.len() == SPAKE_LEN => Packet::MsgA { pid, msg: body },
        MSG_B if body.len() == SPAKE_LEN => Packet::MsgB { pid, msg: body },
        CONFIRM => Packet::Confirm { pid, tag: body.try_into().ok()? },
        DONE if body.len() > 16 => Packet::Done { pid, sealed: body },
        CANCEL if body.is_empty() => Packet::Cancel { pid },
        _ => return None,
    })
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2s256::new();
    for p in parts {
        h.update((p.len() as u32).to_le_bytes());
        h.update(p);
    }
    h.finalize().into()
}

/// Secrets both sides derive from the PAKE output and the messages exchanged.
struct Secrets {
    confirm_client: [u8; 32],
    seal: [u8; 32],
}

fn derive(shared: &[u8], pid: &Pid, msg_a: &[u8], msg_b: &[u8]) -> Secrets {
    let transcript = hash(&[pid, msg_a, msg_b]);
    Secrets {
        confirm_client: hash(&[b"displaywire pairing: client confirms", shared, &transcript]),
        seal: hash(&[b"displaywire pairing: sealed key", shared, &transcript]),
    }
}

fn cipher(seal: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(seal.into())
}

/// Compares in constant time.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The client's side, from the moment the PIN has been typed.
pub struct ClientPake {
    pid: Pid,
    state: Option<Spake2<Ed25519Group>>,
    msg_a: Vec<u8>,
    secrets: Option<Secrets>,
}

impl ClientPake {
    /// Returns the state and the MSG_A body to send.
    pub fn start(pid: Pid, pin: &str) -> (ClientPake, Vec<u8>) {
        let (state, msg_a) =
            Spake2::<Ed25519Group>::start_a(&Password::new(pin.as_bytes()), &Identity::new(ID_CLIENT), &Identity::new(ID_SERVER));
        (ClientPake { pid, state: Some(state), msg_a: msg_a.clone(), secrets: None }, msg_a)
    }

    /// The MSG_A datagram.
    pub fn msg_a_datagram(&self) -> Vec<u8> {
        encode_body(MSG_A, &self.pid, &self.msg_a)
    }

    /// Takes the server's SPAKE2 message; returns the CONFIRM datagram.
    pub fn on_msg_b(&mut self, msg_b: &[u8]) -> Result<Vec<u8>, Error> {
        let state = self.state.take().ok_or(Error::Rejected)?;
        let shared = state.finish(msg_b).map_err(|_| Error::Rejected)?;
        let secrets = derive(&shared, &self.pid, &self.msg_a, msg_b);
        let datagram = encode_body(CONFIRM, &self.pid, &secrets.confirm_client);
        self.secrets = Some(secrets);
        Ok(datagram)
    }

    /// Opens the server's DONE: the new key and the server's name. Fails if the server did not know the PIN.
    pub fn open_done(&self, sealed: &[u8]) -> Result<(Key, String), Error> {
        let secrets = self.secrets.as_ref().ok_or(Error::Rejected)?;
        let plain = cipher(&secrets.seal)
            .decrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: sealed, aad: &self.pid })
            .map_err(|_| Error::Rejected)?;
        let (key, rest) = plain.split_first_chunk::<32>().ok_or(Error::Rejected)?;
        let name = std::str::from_utf8(rest).map_err(|_| Error::Rejected)?;
        Ok((Key::from_bytes(*key), clean_name(name)))
    }
}

/// The server's side of one attempt with one PIN.
pub struct ServerPake {
    pid: Pid,
    msg_a: Vec<u8>,
    msg_b: Vec<u8>,
    secrets: Secrets,
}

impl ServerPake {
    /// Answers the client's MSG_A. Returns the state and the MSG_B body.
    pub fn respond(pid: Pid, pin: &str, msg_a: &[u8]) -> Result<(ServerPake, Vec<u8>), Error> {
        let (state, msg_b) =
            Spake2::<Ed25519Group>::start_b(&Password::new(pin.as_bytes()), &Identity::new(ID_CLIENT), &Identity::new(ID_SERVER));
        let shared = state.finish(msg_a).map_err(|_| Error::Rejected)?;
        let secrets = derive(&shared, &pid, msg_a, &msg_b);
        Ok((ServerPake { pid, msg_a: msg_a.to_vec(), msg_b: msg_b.clone(), secrets }, msg_b))
    }

    pub fn msg_b_datagram(&self) -> Vec<u8> {
        encode_body(MSG_B, &self.pid, &self.msg_b)
    }

    /// Whether this MSG_A is the one this state answered (a retransmission).
    pub fn answers(&self, msg_a: &[u8]) -> bool {
        self.msg_a == msg_a
    }

    /// Whether the client's CONFIRM proves it used the same PIN.
    pub fn check_confirm(&self, tag: &[u8; TAG_LEN]) -> bool {
        same(&self.secrets.confirm_client, tag)
    }

    /// The DONE datagram: the new key and this server's name, sealed.
    pub fn done_datagram(&self, new_key: &Key, server_name: &str) -> Vec<u8> {
        let mut plain = new_key.as_bytes().to_vec();
        plain.extend_from_slice(clean_name(server_name).as_bytes());
        let sealed = cipher(&self.secrets.seal)
            .encrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: &plain, aad: &self.pid })
            .expect("encryption with a fixed-size key cannot fail");
        encode_body(DONE, &self.pid, &sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_a(c: &ClientPake) -> Vec<u8> {
        match decode(&c.msg_a_datagram()) {
            Some(Packet::MsgA { msg, .. }) => msg.to_vec(),
            other => panic!("not a MSG_A: {other:?}"),
        }
    }

    fn msg_b(s: &ServerPake) -> Vec<u8> {
        match decode(&s.msg_b_datagram()) {
            Some(Packet::MsgB { msg, .. }) => msg.to_vec(),
            other => panic!("not a MSG_B: {other:?}"),
        }
    }

    fn tag(confirm: &[u8]) -> [u8; 32] {
        match decode(confirm) {
            Some(Packet::Confirm { tag, .. }) => tag,
            other => panic!("not a CONFIRM: {other:?}"),
        }
    }

    fn sealed(done: &[u8]) -> Vec<u8> {
        match decode(done) {
            Some(Packet::Done { sealed, .. }) => sealed.to_vec(),
            other => panic!("not a DONE: {other:?}"),
        }
    }

    /// One attempt: the client types `client_pin`, the server expects `server_pin`.
    fn attempt(client_pin: &str, server_pin: &str) -> (ClientPake, ServerPake) {
        let pid = new_pid();
        let (mut client, _) = ClientPake::start(pid, client_pin);
        let (server, _) = ServerPake::respond(pid, server_pin, &msg_a(&client)).unwrap();
        client.on_msg_b(&msg_b(&server)).unwrap();
        (client, server)
    }

    #[test]
    fn right_pin_hands_over_the_key() {
        let pid = new_pid();
        let (mut client, _) = ClientPake::start(pid, "482913");
        let (server, _) = ServerPake::respond(pid, "482913", &msg_a(&client)).unwrap();
        let confirm = client.on_msg_b(&msg_b(&server)).unwrap();
        assert!(server.check_confirm(&tag(&confirm)));
        let key = Key::generate();
        let (got, name) = client.open_done(&sealed(&server.done_datagram(&key, "Gaming PC"))).unwrap();
        assert_eq!((got, name.as_str()), (key, "Gaming PC"));
    }

    #[test]
    fn wrong_pin_is_caught_by_the_server_and_reveals_nothing() {
        let pid = new_pid();
        let (mut client, _) = ClientPake::start(pid, "000000");
        let (server, _) = ServerPake::respond(pid, "482913", &msg_a(&client)).unwrap();
        let confirm = client.on_msg_b(&msg_b(&server)).unwrap();
        assert!(!server.check_confirm(&tag(&confirm)));
        // Even a server that went ahead anyway produces something the client can't open.
        assert!(client.open_done(&sealed(&server.done_datagram(&Key::generate(), "x"))).is_err());
    }

    #[test]
    fn a_server_that_does_not_know_the_pin_cannot_fake_a_done() {
        let (client, rogue) = attempt("111111", "222222");
        assert!(client.open_done(&sealed(&rogue.done_datagram(&Key::generate(), "evil"))).is_err());
    }

    #[test]
    fn retransmitted_msg_a_is_recognised() {
        let pid = new_pid();
        let (client, _) = ClientPake::start(pid, "123456");
        let a = msg_a(&client);
        let (server, _) = ServerPake::respond(pid, "123456", &a).unwrap();
        assert!(server.answers(&a));
        let (other, _) = ClientPake::start(pid, "123456");
        assert!(!server.answers(&msg_a(&other)), "a fresh attempt has a fresh message");
    }

    #[test]
    fn packets_round_trip_and_junk_is_rejected() {
        let pid = new_pid();
        assert_eq!(decode(&encode_req(&pid, "Jamie's MacBook")), Some(Packet::Req { pid, name: "Jamie's MacBook" }));
        for s in [Status::Waiting, Status::PinRequired, Status::Denied, Status::Busy, Status::Locked, Status::WrongPin(2)] {
            assert_eq!(decode(&encode_status(&pid, s)), Some(Packet::Status { pid, status: s }));
        }
        assert_eq!(decode(&encode_cancel(&pid)), Some(Packet::Cancel { pid }));
        assert_eq!(decode(b"DW"), None);
        assert_eq!(decode(b"XX\x40aaaaaaaa\x00"), None);
        let mut bad = encode_req(&pid, "abc");
        bad.pop();
        assert_eq!(decode(&bad), None, "name length must match");
        assert_eq!(decode(&encode_body(MSG_A, &pid, &[0u8; 5])), None);
        assert_eq!(decode(&[]), None);
    }

    #[test]
    fn pins_are_six_digits_and_names_are_cleaned() {
        for _ in 0..200 {
            assert!(valid_pin(&random_pin()));
        }
        assert!(!valid_pin("12345") && !valid_pin("1234567") && !valid_pin("12345a"));
        assert_eq!(clean_name("  Jamie\n's Mac\t "), "Jamie's Mac");
        assert_eq!(clean_name(""), "Unnamed device");
        assert!(clean_name(&"x".repeat(100)).len() <= NAME_MAX);
    }
}
