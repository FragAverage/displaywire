//! One streaming session: handshake, keep-alive pings, keyframe requests, reassembly, and
//! handing complete frames to a `VideoSink`. The same upkeep as the Mac client's receiver,
//! without audio, input or a window.

use std::collections::VecDeque;
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use proto::stream::{Msg, VERSION};
use secure::{Initiator, Key, ReplayWindow, Session};
use timing::{Samples, ms, now_ns};
use transport::is_transient;
use transport::video::{Frame, Reassembler};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const PING_INTERVAL_NS: u64 = 20 * MS;
const HELLO_INTERVAL_NS: u64 = 200 * MS;
const KEYFRAME_RETRY_NS: u64 = 100 * MS;
/// Incomplete frames older than this are given up on.
const PARTIAL_MAX_AGE_NS: u64 = 300 * MS;
/// Nothing that verifies for this long (pongs come every 20 ms): the session is gone.
const SESSION_DEAD_NS: u64 = 1_500 * MS;

/// Where complete frames go. Returns false if the frame couldn't be used (a keyframe is then requested).
pub trait VideoSink {
    fn frame(&mut self, frame: &Frame) -> bool;
}

/// Milestone 1: count frames and nothing else.
pub struct Discard;

impl VideoSink for Discard {
    fn frame(&mut self, _frame: &Frame) -> bool {
        true
    }
}

/// Writes the stream (Annex B) to a file, for checking it on another machine.
pub struct Dump(std::io::BufWriter<std::fs::File>);

impl Dump {
    pub fn create(path: &Path) -> Result<Dump, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Dump(std::io::BufWriter::new(f)))
    }
}

impl VideoSink for Dump {
    fn frame(&mut self, frame: &Frame) -> bool {
        self.0.write_all(&frame.data).is_ok()
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `caps`: `proto::stream::CAP_*` flags for the server (`CAP_H264` for LGNC).
pub fn run(server: SocketAddr, key: Key, caps: u8, mut sink: Box<dyn VideoSink + Send>, stop: &AtomicBool) -> Result<(), String> {
    let sock = transport::udp_socket(transport::any_local(server)).map_err(|e| e.to_string())?;
    sock.connect(server).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_millis(2))).map_err(|e| e.to_string())?;
    let mut link = Link { sock, session: None };

    let begin = now_ns();
    let (mut next_hello, mut next_ping, mut ping_id) = (0, 0, 0u32);
    let mut got_video = false;
    let mut need_keyframe = true;
    let mut last_keyframe_request = 0;
    let mut initiator: Option<(Initiator, Vec<u8>)> = None;
    let mut next_hs = 0u64;
    let mut window = ReplayWindow::default();
    let (mut last_rx, mut ever_connected) = (0u64, false);
    let mut reassembler = Reassembler::default();
    let mut clock = Clock::default();
    let mut stats = Stats::new();
    let mut buf = [0u8; 2048];
    let mut plain = [0u8; 2048];
    println!("connecting to {server}");

    while !stop.load(Relaxed) {
        let now = now_ns();
        let alive = link.session.is_some() && now - last_rx < SESSION_DEAD_NS;
        if !alive {
            if !ever_connected && now - begin > 5 * SEC {
                return Err(format!(
                    "no reply from {server}: the PC may be off or unreachable (is udp/{} allowed through its firewall?), or it no longer has this TV paired.",
                    server.port()
                ));
            }
            if now >= next_hs {
                if initiator.is_none() {
                    let hello = secure::ClientHello { version: VERSION, caps };
                    initiator = Some(Initiator::start(&key, hello, unix_ms()).map_err(|e| e.to_string())?);
                }
                if let Some((_, hs1)) = &initiator {
                    let _ = link.sock.send(hs1);
                }
                next_hs = now + HELLO_INTERVAL_NS;
            }
        }
        // Hello makes the server send a frame (a keyframe of an unchanged screen too).
        if alive && !got_video && now >= next_hello {
            link.send(Msg::Hello { version: VERSION, caps });
            next_hello = now + HELLO_INTERVAL_NS;
        }
        if now >= next_ping {
            link.send(Msg::Ping { id: ping_id, client_ns: now });
            ping_id = ping_id.wrapping_add(1);
            next_ping = now + PING_INTERVAL_NS;
        }
        if reassembler.expire(now, PARTIAL_MAX_AGE_NS) {
            need_keyframe = true;
        }
        if got_video && need_keyframe && now - last_keyframe_request > KEYFRAME_RETRY_NS {
            link.send(Msg::KeyframeRequest);
            last_keyframe_request = now;
        }
        if let Some(line) = stats.tick(now, &clock) {
            println!("{line}");
        }

        let n = match link.sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if is_transient(&e) => continue,
            Err(e) => return Err(e.to_string()),
        };
        let rx = now_ns();
        stats.datagrams += 1;
        let datagram = &buf[..n];
        match secure::kind(datagram) {
            Some(secure::HS2) => {
                if let Some((init, _)) = initiator.take()
                    && let Ok((session, hello)) = init.finish(datagram)
                {
                    if hello.version != VERSION {
                        return Err(format!("the server speaks protocol {}, this client {VERSION}: update the older one", hello.version));
                    }
                    println!("{} {server}", if ever_connected { "reconnected to" } else { "connected to" });
                    link.session = Some(session);
                    window = ReplayWindow::default();
                    (last_rx, ever_connected, got_video, next_hello) = (rx, true, false, 0);
                }
                continue;
            }
            Some(secure::SEC) => {}
            _ => continue,
        }
        let Some(session) = &link.session else { continue };
        let Some(len) = session.open(datagram, &mut window, &mut plain) else {
            stats.unverified += 1;
            continue;
        };
        last_rx = rx;
        match Msg::decode(&plain[..len]) {
            Some(Msg::Video(v, payload)) => {
                got_video = true;
                stats.bytes += n as u64;
                let Some(frame) = reassembler.push(v, payload, rx) else { continue };
                if frame.skipped > 0 {
                    stats.lost += frame.skipped;
                    need_keyframe = true;
                }
                if need_keyframe && !frame.keyframe {
                    stats.dropped += 1;
                    continue;
                }
                if frame.repaired > 0 {
                    stats.repaired += 1;
                }
                if !sink.frame(&frame) {
                    stats.errors += 1;
                    need_keyframe = true;
                    continue;
                }
                if frame.keyframe {
                    need_keyframe = false;
                }
                stats.frames += 1;
                // Server clock → ours: server = client + offset.
                if let Some(o) = clock.offset() {
                    stats.latency.push(frame.complete_ns as i64 + o - frame.capture_ns as i64);
                }
            }
            Some(Msg::Pong { client_ns, server_ns, .. }) => clock.update(rx, client_ns, server_ns),
            _ => {}
        }
    }
    for _ in 0..3 {
        link.send(Msg::Bye);
    }
    Ok(())
}

struct Link {
    sock: UdpSocket,
    session: Option<Session>,
}

impl Link {
    /// Before the handshake completes there is no session and the message is dropped.
    fn send(&self, msg: Msg) {
        let Some(session) = &self.session else { return };
        let (mut plain, mut out) = ([0u8; 64], [0u8; proto::MAX_DATAGRAM]);
        let n = msg.encode(&mut plain);
        let n = session.seal(&plain[..n], &mut out);
        let _ = self.sock.send(&out[..n]);
    }
}

/// Server clock offset from ping/pong pairs, taken from the lowest-RTT recent sample.
#[derive(Default)]
struct Clock {
    samples: VecDeque<(u64, i64)>,
}

impl Clock {
    fn update(&mut self, rx_ns: u64, client_ns: u64, server_ns: u64) {
        let rtt = rx_ns.saturating_sub(client_ns);
        let offset = server_ns as i64 - (client_ns + rtt / 2) as i64;
        if self.samples.len() == 250 {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt, offset));
    }

    fn offset(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|s| s.0).map(|s| s.1)
    }

    fn recent_rtt(&self) -> Option<u64> {
        let mut v: Vec<u64> = self.samples.iter().rev().take(50).map(|s| s.0).collect();
        v.sort_unstable();
        v.get(v.len() / 2).copied()
    }
}

struct Stats {
    window_start: u64,
    frames: u32,
    bytes: u64,
    repaired: u32,
    dropped: u32,
    lost: u32,
    errors: u32,
    /// Everything read from the socket, and what of it failed to decrypt.
    datagrams: u32,
    unverified: u32,
    /// Capture on the PC → frame complete here.
    latency: Samples,
}

impl Stats {
    fn new() -> Stats {
        Stats { window_start: now_ns(), frames: 0, bytes: 0, repaired: 0, dropped: 0, lost: 0, errors: 0, datagrams: 0, unverified: 0, latency: Samples::default() }
    }

    /// Once a second, the stats line.
    fn tick(&mut self, now: u64, clock: &Clock) -> Option<String> {
        let elapsed = now - self.window_start;
        if elapsed < SEC {
            return None;
        }
        let lat = self.latency.summary();
        let mbps = self.bytes as f64 * 8.0 / elapsed as f64 * 1e3;
        let rtt = clock.recent_rtt().map_or(0.0, |r| ms(r as i64));
        let mut line = format!(
            "{} fps  {mbps:.0} Mbps  capture→received {:.1} ms (p99 {:.1})  rtt {rtt:.1} ms",
            self.frames,
            ms(lat.p50),
            ms(lat.p99)
        );
        if self.lost + self.dropped + self.errors > 0 {
            line += &format!("  lost {} dropped {} errors {}", self.lost, self.dropped, self.errors);
        }
        if self.repaired > 0 {
            line += &format!("  fec-repaired {}", self.repaired);
        }
        line += &format!("  rx {}", self.datagrams);
        if self.unverified > 0 {
            line += &format!(" (unverified {})", self.unverified);
        }
        *self = Stats { window_start: now, ..Stats::new() };
        Some(line)
    }
}
