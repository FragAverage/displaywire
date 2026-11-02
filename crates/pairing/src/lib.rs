//! The client's side of PIN pairing: asks a server to pair, waits for the person at the PC
//! to allow it, takes the PIN they were shown, and ends with a key of its own. A thread does
//! the network work (UDP, with retransmission); callers poll `progress`. See `docs/PAIRING.md`
//! and `secure::pair` for the protocol.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secure::Key;
use secure::pair::{self, ClientPake, Packet, Pid, Status};

/// Where pairing stands, for the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// Sent a request, nothing back yet.
    Contacting,
    /// Nothing has come back for a while: wrong address, or the PC isn't waiting for a new device.
    NoReply,
    /// The PC is asking its user to allow this device.
    WaitingForAllow,
    /// Allowed: the PC shows a PIN; call `submit_pin`.
    NeedPin,
    /// The PIN was sent; checking it.
    Checking,
    /// The last PIN was wrong; `NeedPin` again once the UI has shown this.
    WrongPin { tries_left: u8 },
    Denied,
    /// Another device is pairing with this PC right now.
    Busy,
    /// Too many wrong PINs, or the PIN expired.
    Locked,
    Cancelled,
    /// The PC stopped answering part-way.
    Failed(String),
    /// Paired: this key is the device's own; store it for the server.
    Done { key: Key, server_name: String },
}

impl Progress {
    /// Nothing more will happen.
    pub fn finished(&self) -> bool {
        matches!(self, Progress::Denied | Progress::Locked | Progress::Cancelled | Progress::Failed(_) | Progress::Done { .. })
    }
}

enum Cmd {
    Pin(String),
}

pub struct Pairing {
    progress: Arc<Mutex<Progress>>,
    cmd: Arc<Mutex<Option<Cmd>>>,
    stop: Arc<AtomicBool>,
    /// For the Cancel datagram.
    sock: UdpSocket,
    pid: Pid,
}

/// How long without any reply before saying so.
const NO_REPLY: Duration = Duration::from_secs(4);
/// How long, once a PIN is sent, the PC may stay silent before giving up.
const CHECK_TIMEOUT: Duration = Duration::from_secs(12);
const RESEND: Duration = Duration::from_millis(700);

impl Pairing {
    /// Starts asking `server` to pair, calling this device `device_name`.
    pub fn start(server: SocketAddr, device_name: &str) -> std::io::Result<Pairing> {
        let bind: SocketAddr = if server.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
        let sock = UdpSocket::bind(bind)?;
        sock.connect(server)?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;
        let pid = pair::new_pid();
        let progress = Arc::new(Mutex::new(Progress::Contacting));
        let cmd = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker { sock: sock.try_clone()?, pid, name: device_name.to_string(), progress: progress.clone(), cmd: cmd.clone(), stop: stop.clone() };
        std::thread::Builder::new().name("pairing".into()).spawn(move || worker.run())?;
        Ok(Pairing { progress, cmd, stop, sock, pid })
    }

    pub fn progress(&self) -> Progress {
        self.progress.lock().unwrap().clone()
    }

    /// Sends the PIN the PC is showing. Ignored unless `NeedPin`.
    pub fn submit_pin(&self, pin: &str) {
        if pair::valid_pin(pin) && matches!(self.progress(), Progress::NeedPin) {
            *self.cmd.lock().unwrap() = Some(Cmd::Pin(pin.to_string()));
        }
    }

    /// Tells the PC and stops.
    pub fn cancel(&self) {
        if !self.progress().finished() {
            let _ = self.sock.send(&pair::encode_cancel(&self.pid));
            *self.progress.lock().unwrap() = Progress::Cancelled;
        }
        self.stop.store(true, Relaxed);
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

struct Worker {
    sock: UdpSocket,
    pid: Pid,
    name: String,
    progress: Arc<Mutex<Progress>>,
    cmd: Arc<Mutex<Option<Cmd>>>,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn set(&self, p: Progress) {
        let mut cur = self.progress.lock().unwrap();
        // Cancelling is final.
        if !matches!(*cur, Progress::Cancelled) {
            *cur = p;
        }
    }

    fn done(&self) -> bool {
        self.stop.load(Relaxed) || self.progress.lock().unwrap().finished()
    }

    fn run(self) {
        let started = Instant::now();
        let mut last_reply = Instant::now();
        let mut last_send = Instant::now() - RESEND;
        let mut buf = [0u8; 512];
        let mut pake: Option<ClientPake> = None;
        let mut confirm_sent = false;
        let mut checking_since = Instant::now();
        let req = pair::encode_req(&self.pid, &self.name);
        while !self.done() {
            // What to send now.
            let state = self.progress.lock().unwrap().clone();
            if let Some(Cmd::Pin(pin)) = self.cmd.lock().unwrap().take() {
                let (p, _) = ClientPake::start(self.pid, &pin);
                pake = Some(p);
                confirm_sent = false;
                checking_since = Instant::now();
                last_send = Instant::now() - RESEND;
                self.set(Progress::Checking);
            }
            if last_send.elapsed() >= RESEND {
                last_send = Instant::now();
                match (&state, &pake) {
                    (Progress::Checking, Some(p)) => {
                        let _ = self.sock.send(&p.msg_a_datagram());
                    }
                    _ => {
                        let _ = self.sock.send(&req);
                    }
                }
                if matches!(state, Progress::Contacting) && last_reply.elapsed() > NO_REPLY && started.elapsed() > NO_REPLY {
                    self.set(Progress::NoReply);
                }
                if matches!(state, Progress::Checking) && checking_since.elapsed() > CHECK_TIMEOUT {
                    self.set(Progress::Failed("The PC stopped answering".into()));
                }
            }
            let Ok(n) = self.sock.recv(&mut buf) else { continue };
            let Some(packet) = pair::decode(&buf[..n]) else { continue };
            let theirs = match &packet {
                Packet::Status { pid, .. } | Packet::MsgB { pid, .. } | Packet::Done { pid, .. } => *pid == self.pid,
                _ => false,
            };
            if !theirs {
                continue;
            }
            last_reply = Instant::now();
            match packet {
                Packet::Status { status, .. } => self.on_status(status, &mut pake),
                Packet::MsgB { msg, .. } => {
                    if let Some(p) = pake.as_mut()
                        && !confirm_sent
                        && let Ok(confirm) = p.on_msg_b(msg)
                    {
                        confirm_sent = true;
                        let _ = self.sock.send(&confirm);
                        last_send = Instant::now();
                    }
                }
                Packet::Done { sealed, .. } => {
                    if let Some(p) = &pake {
                        match p.open_done(sealed) {
                            Ok((key, server_name)) => self.set(Progress::Done { key, server_name }),
                            Err(_) => self.set(Progress::Failed("The PC's answer could not be verified".into())),
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn on_status(&self, status: Status, pake: &mut Option<ClientPake>) {
        let current = self.progress.lock().unwrap().clone();
        match status {
            Status::Waiting if !matches!(current, Progress::Checking | Progress::NeedPin | Progress::WrongPin { .. }) => self.set(Progress::WaitingForAllow),
            Status::Waiting => {}
            Status::PinRequired if matches!(current, Progress::Contacting | Progress::NoReply | Progress::WaitingForAllow) => self.set(Progress::NeedPin),
            Status::PinRequired => {}
            Status::Denied => self.set(Progress::Denied),
            Status::Busy => self.set(Progress::Busy),
            Status::Locked => self.set(Progress::Locked),
            Status::WrongPin(n) => {
                if matches!(current, Progress::Checking) {
                    *pake = None;
                    self.set(Progress::WrongPin { tries_left: n });
                }
            }
        }
    }
}

impl Pairing {
    /// After showing `WrongPin`, lets the user type another PIN.
    pub fn retry(&self) {
        let mut p = self.progress.lock().unwrap();
        if matches!(*p, Progress::WrongPin { .. }) {
            *p = Progress::NeedPin;
        }
    }
}
