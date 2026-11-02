//! Pairing a new device with a PIN, from the server's side. See `docs/PAIRING.md`.
//!
//! The server is silent to pairing requests until the person at the PC starts
//! listening ("Pair new device"). A request then waits for them to allow or deny it;
//! allowing shows a fresh PIN, and the client proves it was typed correctly (see
//! `secure::pair`) before it is handed a key of its own, which is stored in `clients`.

use std::net::SocketAddr;
use std::sync::Mutex;

use secure::Key;
use secure::pair::{self, Packet, Pid, ServerPake, Status};

use crate::clients::Store;

const SEC: u64 = 1_000_000_000;
/// How long "Pair new device" keeps listening.
pub const LISTEN_NS: u64 = 120 * SEC;
/// A request nobody answered is dropped after this.
const ASK_NS: u64 = 60 * SEC;
/// A PIN stays valid this long.
const PIN_NS: u64 = 120 * SEC;
/// How long a finished pairing's answer is kept, for a client that missed it.
const RESULT_NS: u64 = 10 * SEC;
/// How long the UI is told about the last result.
const SHOW_NS: u64 = 8 * SEC;
pub const MAX_TRIES: u8 = 3;

/// How the last pairing ended, for the UI to say so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Paired(String),
    Denied(String),
    /// Three wrong PINs.
    Locked(String),
    /// The PIN or the request timed out.
    TimedOut(String),
    Cancelled(String),
}

/// What the UI shows right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    Off,
    Listening { secs_left: u64 },
    Asking { name: String, addr: String, secs_left: u64 },
    Pin { name: String, addr: String, pin: String, tries_left: u8, secs_left: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub view: View,
    pub last: Option<Outcome>,
}

enum Stage {
    /// Nothing in progress: listening if `listen_until` is in the future, else silent.
    Idle,
    Asking { pid: Pid, addr: SocketAddr, name: String, until: u64 },
    Pin { pid: Pid, addr: SocketAddr, name: String, pin: String, until: u64, tries: u8, pake: Option<ServerPake>, last_reply: Option<Vec<u8>> },
    /// Paired: the answer is kept for retransmissions.
    Done { pid: Pid, addr: SocketAddr, reply: Vec<u8>, until: u64 },
}

struct Inner {
    stage: Stage,
    listen_until: u64,
    /// Pairings that ended badly, so a client that missed the news still gets it.
    closed: Vec<(Pid, SocketAddr, Status, u64)>,
    last: Option<(Outcome, u64)>,
}

pub struct Pairing {
    inner: Mutex<Inner>,
    server_name: String,
}

impl Pairing {
    pub fn new(server_name: &str) -> Pairing {
        Pairing {
            inner: Mutex::new(Inner { stage: Stage::Idle, listen_until: 0, closed: Vec::new(), last: None }),
            server_name: server_name.to_string(),
        }
    }

    /// "Pair new device": start answering requests for `LISTEN_NS`.
    pub fn start(&self, now: u64) {
        let mut i = self.inner.lock().unwrap();
        i.listen_until = now + LISTEN_NS;
        i.last = None;
    }

    /// Stops listening and abandons anything in progress.
    pub fn stop(&self, now: u64) {
        let mut i = self.inner.lock().unwrap();
        i.listen_until = 0;
        if let Some(name) = i.current_name() {
            i.finish(now, Outcome::Cancelled(name), None);
        }
        i.stage = Stage::Idle;
    }

    /// The person at the PC allows the request that is waiting: a PIN appears.
    pub fn allow(&self, now: u64) {
        let mut i = self.inner.lock().unwrap();
        if !matches!(i.stage, Stage::Asking { .. }) {
            return;
        }
        if let Stage::Asking { pid, addr, name, .. } = std::mem::replace(&mut i.stage, Stage::Idle) {
            i.stage = Stage::Pin { pid, addr, name, pin: pair::random_pin(), until: now + PIN_NS, tries: 0, pake: None, last_reply: None };
        }
    }

    /// The person at the PC denies it.
    pub fn deny(&self, now: u64) {
        let mut i = self.inner.lock().unwrap();
        if !matches!(i.stage, Stage::Asking { .. }) {
            return;
        }
        if let Stage::Asking { pid, addr, name, .. } = std::mem::replace(&mut i.stage, Stage::Idle) {
            i.close(now, pid, addr, Status::Denied, Outcome::Denied(name));
        }
    }

    /// Expires what has run out of time. Call regularly.
    pub fn tick(&self, now: u64) {
        let mut i = self.inner.lock().unwrap();
        i.closed.retain(|c| c.3 > now);
        if i.last.as_ref().is_some_and(|l| l.1 <= now) {
            i.last = None;
        }
        match &i.stage {
            Stage::Asking { until, .. } if *until <= now => {
                if let Stage::Asking { pid, addr, name, .. } = std::mem::replace(&mut i.stage, Stage::Idle) {
                    i.close(now, pid, addr, Status::Denied, Outcome::TimedOut(name));
                }
            }
            Stage::Pin { until, .. } if *until <= now => {
                if let Stage::Pin { pid, addr, name, .. } = std::mem::replace(&mut i.stage, Stage::Idle) {
                    i.close(now, pid, addr, Status::Locked, Outcome::TimedOut(name));
                }
            }
            Stage::Done { until, .. } if *until <= now => i.stage = Stage::Idle,
            _ => {}
        }
    }

    pub fn snapshot(&self, now: u64) -> Snapshot {
        let i = self.inner.lock().unwrap();
        let secs = |t: u64| t.saturating_sub(now).div_ceil(SEC);
        let view = match &i.stage {
            Stage::Asking { name, addr, until, .. } => View::Asking { name: name.clone(), addr: addr.ip().to_string(), secs_left: secs(*until) },
            Stage::Pin { name, addr, pin, until, tries, .. } => View::Pin {
                name: name.clone(),
                addr: addr.ip().to_string(),
                pin: pin.clone(),
                tries_left: MAX_TRIES - tries,
                secs_left: secs(*until),
            },
            Stage::Idle | Stage::Done { .. } if i.listen_until > now => View::Listening { secs_left: secs(i.listen_until) },
            _ => View::Off,
        };
        Snapshot { view, last: i.last.as_ref().map(|l| l.0.clone()) }
    }

    /// Handles a pairing datagram. The reply, if any; `None` means stay silent.
    pub fn handle(&self, now: u64, from: SocketAddr, datagram: &[u8], store: &mut Store) -> Option<Vec<u8>> {
        let packet = pair::decode(datagram)?;
        let mut i = self.inner.lock().unwrap();
        let pid = match &packet {
            Packet::Req { pid, .. }
            | Packet::MsgA { pid, .. }
            | Packet::Confirm { pid, .. }
            | Packet::Cancel { pid }
            | Packet::MsgB { pid, .. }
            | Packet::Done { pid, .. }
            | Packet::Status { pid, .. } => *pid,
        };
        // A pairing that already ended badly: say so again.
        if let Some(&(_, _, status, _)) = i.closed.iter().find(|c| c.0 == pid && c.1 == from) {
            return Some(pair::encode_status(&pid, status));
        }
        let listening = i.listen_until > now;
        match packet {
            Packet::Req { name, .. } => match &i.stage {
                Stage::Idle if listening => {
                    i.stage = Stage::Asking { pid, addr: from, name: pair::clean_name(name), until: now + ASK_NS };
                    Some(pair::encode_status(&pid, Status::Waiting))
                }
                Stage::Idle | Stage::Done { .. } => None,
                Stage::Asking { pid: p, addr, .. } if *p == pid && *addr == from => Some(pair::encode_status(&pid, Status::Waiting)),
                Stage::Pin { pid: p, addr, .. } if *p == pid && *addr == from => Some(pair::encode_status(&pid, Status::PinRequired)),
                _ => listening.then(|| pair::encode_status(&pid, Status::Busy)),
            },
            Packet::MsgA { msg, .. } => {
                let Stage::Pin { pid: p, addr, pin, pake, tries, .. } = &mut i.stage else { return None };
                if *p != pid || *addr != from {
                    return None;
                }
                if *tries >= MAX_TRIES {
                    return Some(pair::encode_status(&pid, Status::Locked));
                }
                if let Some(existing) = pake.as_ref().filter(|e| e.answers(msg)) {
                    return Some(existing.msg_b_datagram());
                }
                let (state, _) = ServerPake::respond(pid, pin, msg).ok()?;
                let reply = state.msg_b_datagram();
                *pake = Some(state);
                Some(reply)
            }
            Packet::Confirm { tag, .. } => {
                if let Stage::Done { pid: p, addr, reply, .. } = &i.stage
                    && *p == pid
                    && *addr == from
                {
                    return Some(reply.clone());
                }
                let Stage::Pin { pid: p, addr, name, pake, tries, last_reply, .. } = &mut i.stage else { return None };
                if *p != pid || *addr != from {
                    return None;
                }
                let Some(state) = pake.as_ref() else { return last_reply.clone() };
                if state.check_confirm(&tag) {
                    let key = Key::generate();
                    let name = name.clone();
                    let device = match store.add(&name, key.clone()) {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!("pairing: can't save the new device: {e}");
                            return None;
                        }
                    };
                    let reply = state.done_datagram(&key, &self.server_name);
                    println!("{from}: paired \"{}\"", device.name);
                    i.stage = Stage::Done { pid, addr: from, reply: reply.clone(), until: now + RESULT_NS };
                    i.listen_until = 0;
                    i.last = Some((Outcome::Paired(device.name), now + SHOW_NS));
                    Some(reply)
                } else {
                    *tries += 1;
                    *pake = None;
                    if *tries >= MAX_TRIES {
                        let name = name.clone();
                        println!("{from}: pairing locked after {MAX_TRIES} wrong PINs");
                        i.stage = Stage::Idle;
                        i.close(now, pid, from, Status::Locked, Outcome::Locked(name));
                        Some(pair::encode_status(&pid, Status::Locked))
                    } else {
                        let reply = pair::encode_status(&pid, Status::WrongPin(MAX_TRIES - *tries));
                        *last_reply = Some(reply.clone());
                        Some(reply)
                    }
                }
            }
            Packet::Cancel { .. } => {
                let ours = match &i.stage {
                    Stage::Asking { pid: p, addr, .. } | Stage::Pin { pid: p, addr, .. } => *p == pid && *addr == from,
                    _ => false,
                };
                if ours && let Some(name) = i.current_name() {
                    i.stage = Stage::Idle;
                    i.last = Some((Outcome::Cancelled(name), now + SHOW_NS));
                }
                None
            }
            // Server-to-client packets coming the other way are junk.
            Packet::MsgB { .. } | Packet::Done { .. } | Packet::Status { .. } => None,
        }
    }
}

impl Inner {
    fn current_name(&self) -> Option<String> {
        match &self.stage {
            Stage::Asking { name, .. } | Stage::Pin { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// Records how a pairing ended, and remembers the answer for retransmissions.
    fn close(&mut self, now: u64, pid: Pid, addr: SocketAddr, status: Status, outcome: Outcome) {
        self.closed.push((pid, addr, status, now + RESULT_NS));
        self.last = Some((outcome, now + SHOW_NS));
    }

    fn finish(&mut self, now: u64, outcome: Outcome, _status: Option<Status>) {
        self.last = Some((outcome, now + SHOW_NS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secure::pair::{ClientPake, new_pid};

    const T0: u64 = 1_000 * SEC;

    fn addr(n: u16) -> SocketAddr {
        SocketAddr::from(([192, 168, 1, 50], 40000 + n))
    }

    fn store() -> (Store, std::path::PathBuf) {
        let mut r = [0u8; 4];
        getrandom::fill(&mut r).unwrap();
        let dir = std::env::temp_dir().join(format!("dw-pairing-{}", r.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir).unwrap(), dir)
    }

    fn status(reply: Option<Vec<u8>>) -> Option<Status> {
        match pair::decode(&reply?)? {
            Packet::Status { status, .. } => Some(status),
            _ => None,
        }
    }

    /// A client pairing against `p`, from the request to the PIN prompt.
    fn request(p: &Pairing, s: &mut Store, pid: Pid, from: SocketAddr, now: u64) -> Option<Status> {
        status(p.handle(now, from, &pair::encode_req(&pid, "Jamie's MacBook"), s))
    }

    /// Types `pin` on the client. Returns the server's final reply.
    fn enter_pin(p: &Pairing, s: &mut Store, pid: Pid, from: SocketAddr, pin: &str, now: u64) -> (Option<Vec<u8>>, ClientPake) {
        let (mut client, _) = ClientPake::start(pid, pin);
        let msg_b = p.handle(now, from, &client.msg_a_datagram(), s).expect("a MSG_B");
        let Some(Packet::MsgB { msg, .. }) = pair::decode(&msg_b) else { panic!("not a MSG_B") };
        let confirm = client.on_msg_b(msg).unwrap();
        (p.handle(now, from, &confirm, s), client)
    }

    fn pin_of(p: &Pairing, now: u64) -> String {
        match p.snapshot(now).view {
            View::Pin { pin, .. } => pin,
            other => panic!("no PIN showing: {other:?}"),
        }
    }

    #[test]
    fn silent_until_listening() {
        let (mut s, dir) = store();
        let p = Pairing::new("Gaming PC");
        let pid = new_pid();
        assert_eq!(request(&p, &mut s, pid, addr(1), T0), None, "no answer at all while not listening");
        assert_eq!(p.snapshot(T0).view, View::Off);
        p.start(T0);
        assert_eq!(request(&p, &mut s, pid, addr(1), T0), Some(Status::Waiting));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_whole_pairing_gives_the_device_its_own_key() {
        let (mut s, dir) = store();
        let p = Pairing::new("Gaming PC");
        let (pid, from) = (new_pid(), addr(1));
        p.start(T0);
        assert_eq!(request(&p, &mut s, pid, from, T0), Some(Status::Waiting));
        assert!(matches!(p.snapshot(T0).view, View::Asking { .. }));
        p.allow(T0);
        assert_eq!(request(&p, &mut s, pid, from, T0 + SEC), Some(Status::PinRequired));
        let pin = pin_of(&p, T0);
        let (reply, client) = enter_pin(&p, &mut s, pid, from, &pin, T0 + 5 * SEC);
        let done = reply.expect("DONE");
        let Some(Packet::Done { sealed, .. }) = pair::decode(&done) else { panic!("not a DONE") };
        let (key, server_name) = client.open_done(sealed).unwrap();
        assert_eq!(server_name, "Gaming PC");
        // The server stored exactly that key for this device, and is done listening.
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.list()[0].name, "Jamie's MacBook");
        assert!(s.list()[0].key == key);
        assert_eq!(p.snapshot(T0 + 6 * SEC).last, Some(Outcome::Paired("Jamie's MacBook".into())));
        assert_eq!(p.snapshot(T0 + 6 * SEC).view, View::Off);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn three_wrong_pins_lock_it_and_no_device_is_added() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(2));
        p.start(T0);
        request(&p, &mut s, pid, from, T0);
        p.allow(T0);
        let pin = pin_of(&p, T0);
        let wrong = if pin == "000000" { "111111" } else { "000000" };
        let (r1, _) = enter_pin(&p, &mut s, pid, from, wrong, T0);
        assert_eq!(status(r1), Some(Status::WrongPin(2)));
        let (r2, _) = enter_pin(&p, &mut s, pid, from, wrong, T0);
        assert_eq!(status(r2), Some(Status::WrongPin(1)));
        // Now even the right PIN is refused: it needs a new request and a new PIN.
        let (r3, _) = enter_pin(&p, &mut s, pid, from, wrong, T0);
        assert_eq!(status(r3), Some(Status::Locked));
        assert_eq!(request(&p, &mut s, pid, from, T0 + SEC), Some(Status::Locked), "still told so");
        assert!(s.list().is_empty());
        assert!(matches!(p.snapshot(T0).last, Some(Outcome::Locked(_))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn allow_and_deny_with_nobody_waiting_change_nothing() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(12));
        p.start(T0);
        p.allow(T0);
        p.deny(T0);
        assert!(matches!(p.snapshot(T0).view, View::Listening { .. }));
        request(&p, &mut s, pid, from, T0);
        p.allow(T0);
        let pin = pin_of(&p, T0);
        // A second Allow (a double click) must not reset or lose the PIN that is showing.
        p.allow(T0 + SEC);
        p.deny(T0 + SEC);
        assert_eq!(pin_of(&p, T0 + SEC), pin);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn denying_tells_the_client_and_keeps_listening() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(3));
        p.start(T0);
        request(&p, &mut s, pid, from, T0);
        p.deny(T0);
        assert_eq!(request(&p, &mut s, pid, from, T0 + SEC), Some(Status::Denied));
        assert!(matches!(p.snapshot(T0 + SEC).view, View::Listening { .. }), "the next device can still ask");
        // A different request is a new one.
        assert_eq!(request(&p, &mut s, new_pid(), addr(4), T0 + 2 * SEC), Some(Status::Waiting));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_device_is_told_busy_and_a_stranger_cannot_hijack_the_pin() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(5));
        p.start(T0);
        request(&p, &mut s, pid, from, T0);
        assert_eq!(request(&p, &mut s, new_pid(), addr(6), T0), Some(Status::Busy));
        p.allow(T0);
        let pin = pin_of(&p, T0);
        // Someone else, with the right PIN but a different pairing id/address, gets nothing.
        let other_pid = new_pid();
        let (mut intruder, _) = ClientPake::start(other_pid, &pin);
        assert_eq!(p.handle(T0, addr(6), &intruder.msg_a_datagram(), &mut s), None);
        let _ = intruder.on_msg_b(&[0u8; 33]);
        assert!(s.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn requests_and_pins_expire() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(7));
        p.start(T0);
        request(&p, &mut s, pid, from, T0);
        p.tick(T0 + 61 * SEC);
        assert!(matches!(p.snapshot(T0 + 61 * SEC).last, Some(Outcome::TimedOut(_))));
        // Listening itself ends after two minutes.
        assert_eq!(p.snapshot(T0 + 121 * SEC).view, View::Off);
        assert_eq!(request(&p, &mut s, new_pid(), addr(8), T0 + 121 * SEC), None);
        // A PIN that nobody typed times out too.
        p.start(T0 + 200 * SEC);
        let pid = new_pid();
        request(&p, &mut s, pid, from, T0 + 200 * SEC);
        p.allow(T0 + 200 * SEC);
        p.tick(T0 + 200 * SEC + 121 * SEC);
        assert_eq!(request(&p, &mut s, pid, from, T0 + 322 * SEC), Some(Status::Locked));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn retransmissions_get_the_same_answers() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(9));
        p.start(T0);
        request(&p, &mut s, pid, from, T0);
        p.allow(T0);
        let pin = pin_of(&p, T0);
        let (mut client, _) = ClientPake::start(pid, &pin);
        let a = client.msg_a_datagram();
        let b1 = p.handle(T0, from, &a, &mut s).unwrap();
        let b2 = p.handle(T0, from, &a, &mut s).unwrap();
        assert_eq!(b1, b2, "a repeated MSG_A gets the same MSG_B");
        let Some(Packet::MsgB { msg, .. }) = pair::decode(&b1) else { panic!() };
        let confirm = client.on_msg_b(msg).unwrap();
        let d1 = p.handle(T0, from, &confirm, &mut s).unwrap();
        let d2 = p.handle(T0, from, &confirm, &mut s).unwrap();
        assert_eq!(d1, d2, "a repeated CONFIRM gets the same DONE, and adds no second device");
        assert_eq!(s.list().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_ends_listening_and_cancel_from_the_client_is_noted() {
        let (mut s, dir) = store();
        let p = Pairing::new("PC");
        let (pid, from) = (new_pid(), addr(10));
        p.start(T0);
        request(&p, &mut s, pid, from, T0);
        p.handle(T0, from, &pair::encode_cancel(&pid), &mut s);
        assert!(matches!(p.snapshot(T0).last, Some(Outcome::Cancelled(_))));
        p.stop(T0);
        assert_eq!(p.snapshot(T0).view, View::Off);
        assert_eq!(request(&p, &mut s, new_pid(), addr(11), T0), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
