//! Network thread: receive → reassemble → decode → present, plus session
//! upkeep (hello, pings for RTT/clock offset, keyframe requests).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::stream::{CAP_AUDIO, Msg, VERSION};
use timing::history::{Binner, History};
use timing::{Samples, ms, now_ns};
use transport::video::{Frame, Reassembler};
use transport::is_transient;
use videotoolbox::{Format, Image, Output, PIXEL_FORMAT_NV12, Session};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::audio;
use crate::display::Layer;
use crate::gamepad;
use crate::input;
use crate::link::Link;
use crate::mic;
use crate::settings::Prefs;
use crate::transfer::Transfers;
use secure::{Initiator, ReplayWindow};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const PING_INTERVAL_NS: u64 = 20 * MS;
const HELLO_INTERVAL_NS: u64 = 200 * MS;
/// Width of one graph sample, and how many to keep (60 s).
const BIN_NS: u64 = 100 * MS;
const HISTORY_BINS: usize = 600;
const KEYFRAME_RETRY_NS: u64 = 100 * MS;
/// Incomplete frames older than this are given up on.
const PARTIAL_MAX_AGE_NS: u64 = 300 * MS;
/// No video for this long: assume the server dropped us and say hello again.
const VIDEO_STALL_NS: u64 = 2 * SEC;
/// Nothing that verifies for this long (pongs come every 20 ms): the session is gone.
const SESSION_DEAD_NS: u64 = 1_500 * MS;

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `link`'s socket must be connected to `server`; `input` re-sends its state from this thread.
pub fn run(
    server: SocketAddr,
    link: Arc<Link>,
    input: Arc<input::Sender>,
    layer: Layer,
    proxy: EventLoopProxy<UserEvent>,
    telemetry: Arc<Telemetry>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = Receiver::new(server, link, input, layer, proxy.clone(), telemetry).and_then(|mut r| r.run(&stop)) {
        let _ = proxy.send_event(UserEvent::Fatal(e));
    }
}

/// Stream health shared with the UI thread for the graph overlay.
pub struct Telemetry {
    pub history: Mutex<History>,
    /// Set while the graph is showing: the UI is told about each new bin.
    pub graph_on: AtomicBool,
    /// The user's settings, and a counter the UI bumps when they change.
    pub prefs: Mutex<Prefs>,
    pub prefs_version: AtomicU32,
    pub transfers: Arc<Transfers>,
    /// The user's microphone switch (off at every start).
    pub mic_on: AtomicBool,
}

impl Telemetry {
    pub fn new(graph_on: bool, prefs: Prefs, transfers: Arc<Transfers>) -> Telemetry {
        Telemetry {
            transfers,
            mic_on: AtomicBool::new(false),
            history: Mutex::new(History::new(HISTORY_BINS)),
            graph_on: AtomicBool::new(graph_on),
            prefs: Mutex::new(prefs),
            prefs_version: AtomicU32::new(1),
        }
    }
}

struct Decoder {
    param_sets: [Vec<u8>; 3],
    session: Session,
}

struct Receiver {
    server: SocketAddr,
    link: Arc<Link>,
    input: Arc<input::Sender>,
    layer: Layer,
    proxy: EventLoopProxy<UserEvent>,
    reassembler: Reassembler,
    decoder: Option<Decoder>,
    /// Filled by the decoder callback, which runs before `decode` returns.
    decoded: Arc<Mutex<Option<Result<Image, i32>>>>,
    /// Kept alive while CoreAnimation shows it.
    shown: Option<Image>,
    need_keyframe: bool,
    last_keyframe_request: u64,
    clock: Clock,
    stats: Stats,
    binner: Binner,
    telemetry: Arc<Telemetry>,
    /// None if no output device could be opened: video only then.
    audio: Option<audio::Player>,
    /// Open only while the microphone is on.
    mic: Option<mic::Capture>,
    pads: gamepad::Poller,
    mic_seq: u32,
    /// Chunks sent and peak level since the last report, and when that was.
    mic_stats: (u32, i16, u64),
    /// `DISPLAYWIRE_SIM_LOSS=<percent>`: drop that share of incoming video packets, to test recovery.
    sim_loss: Option<SimLoss>,
    /// `DISPLAYWIRE_TRACE=file`: one CSV line per decoded frame, for timing analysis.
    trace: Option<std::io::BufWriter<std::fs::File>>,
}

/// Without an output device nothing is played and the server isn't asked for audio, so say so on the stream window
/// (once it is up) instead of leaving a silent stream unexplained.
fn tell_no_sound(proxy: &EventLoopProxy<UserEvent>, why: &str) {
    let (proxy, text) = (proxy.clone(), format!("No sound: {why}"));
    let _ = std::thread::Builder::new().name("audio-notice".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let _ = proxy.send_event(UserEvent::Notice(Some(text)));
        std::thread::sleep(Duration::from_secs(12));
        let _ = proxy.send_event(UserEvent::Notice(None));
    });
}

impl Receiver {
    fn new(
        server: SocketAddr,
        link: Arc<Link>,
        input: Arc<input::Sender>,
        layer: Layer,
        proxy: EventLoopProxy<UserEvent>,
        telemetry: Arc<Telemetry>,
    ) -> Result<Receiver, String> {
        link.set_read_timeout(Duration::from_millis(2)).map_err(|e| e.to_string())?;
        let proxy_for_audio = proxy.clone();
        let pads = gamepad::Poller::new(link.clone(), input.focused.clone());
        Ok(Receiver {
            server,
            link,
            input,
            layer,
            proxy,
            reassembler: Reassembler::default(),
            decoder: None,
            decoded: Arc::default(),
            shown: None,
            need_keyframe: true,
            last_keyframe_request: 0,
            clock: Clock::default(),
            stats: Stats::new(),
            binner: Binner::new(now_ns(), BIN_NS),
            telemetry,
            sim_loss: std::env::var("DISPLAYWIRE_SIM_LOSS").ok().and_then(|v| v.parse::<f64>().ok()).map(SimLoss::new),
            mic: None,
            pads,
            mic_seq: 0,
            mic_stats: (0, 0, 0),
            audio: audio::Player::new()
                .map_err(|e| {
                    eprintln!("audio disabled: {e}");
                    tell_no_sound(&proxy_for_audio, &e);
                })
                .ok(),
            trace: std::env::var_os("DISPLAYWIRE_TRACE").and_then(|p| std::fs::File::create(p).ok()).map(|mut f| {
                use std::io::Write;
                let _ = writeln!(f, "frame,keyframe,bytes,capture_ns,decoded_ns,decode_ns");
                std::io::BufWriter::new(f)
            }),
        })
    }

    fn send(&self, msg: Msg) {
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf);
        self.link.send(&buf[..n]);
    }

    /// Opens or closes the microphone to match the switch, and sends what it has captured.
    fn pump_mic(&mut self, alive: bool) {
        let want = alive && self.telemetry.mic_on.load(Relaxed);
        if want && self.mic.is_none() {
            match mic::Capture::new() {
                Ok(m) => self.mic = Some(m),
                Err(e) => {
                    eprintln!("microphone: {e}");
                    self.telemetry.mic_on.store(false, Relaxed);
                    let _ = self.proxy.send_event(UserEvent::Notice(Some(format!("Microphone unavailable: {e}"))));
                    return;
                }
            }
        } else if !want && self.mic.is_some() {
            self.mic = None;
            self.mic_stats = (0, 0, 0);
        }
        let Some(m) = &mut self.mic else { return };
        let mut bytes = [0u8; mic::CHUNK_SAMPLES * 2];
        let mut plain = [0u8; 2048];
        let now = now_ns();
        if now - self.mic_stats.2 >= 1_000_000_000 {
            if self.mic_stats.2 != 0 {
                println!("mic: sent {} chunks, peak level {}%", self.mic_stats.0, self.mic_stats.1 as u32 * 100 / 32767);
            }
            self.mic_stats = (0, 0, now);
        }
        while m.next_chunk(&mut bytes) {
            let audio = proto::stream::Audio { seq: self.mic_seq, format: proto::stream::AudioFormat::Pcm16, capture_ns: now_ns() };
            let n = Msg::Mic(audio, &bytes).encode(&mut plain);
            self.link.send(&plain[..n]);
            let peak = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b).saturating_abs()).max().unwrap_or(0);
            self.mic_stats.0 += 1;
            self.mic_stats.1 = self.mic_stats.1.max(peak);
            self.mic_seq = self.mic_seq.wrapping_add(1);
        }
    }

    fn run(&mut self, stop: &AtomicBool) -> Result<(), String> {
        let begin = now_ns();
        let (mut next_hello, mut next_ping, mut next_bin, mut ping_id) = (0, 0, 0, 0u32);
        let mut last_video = 0u64;
        // Whether video has arrived on this session. The server sends frames only when
        // the screen changes, so a quiet screen is not a stall once video has started.
        let mut got_video = false;
        let mut buf = [0u8; 2048];
        let mut plain = [0u8; 2048];
        let caps = if self.audio.is_some() { CAP_AUDIO } else { 0 };
        // Handshake state (see docs/AUTH.md): a session is "alive" while datagrams
        // that verify keep arriving; otherwise we (re)handshake, keeping the old
        // session until the new one is ready.
        let mut initiator: Option<(Initiator, Vec<u8>)> = None;
        let mut next_hs = 0u64;
        let mut window = ReplayWindow::default();
        let mut last_rx = 0u64;
        let mut ever_connected = false;
        // What the banner says, so it's only sent when it changes.
        let mut status: Option<String> = None;
        // Settings go to the server once a session exists and whenever they change.
        let mut sent_prefs = 0u32;
        let mut sent_for_session = false;
        let mut alive_since = 0u64;
        println!("connecting to {}", self.server);

        while !stop.load(Relaxed) {
            let now = now_ns();
            let alive = self.link.session().is_some() && now - last_rx < SESSION_DEAD_NS;
            if !alive {
                if !ever_connected && now - begin > 5 * SEC {
                    return Err(format!(
                        "no reply from {}: the PC may be off or unreachable (is udp/{} allowed through its firewall?), or it no longer has this Mac paired.",
                        self.server,
                        self.server.port()
                    ));
                }
                if now >= next_hs {
                    if initiator.is_none() {
                        let hello = secure::ClientHello { version: VERSION, caps };
                        initiator = Some(Initiator::start(self.link.key(), hello, unix_ms()).map_err(|e| e.to_string())?);
                    }
                    if let Some((_, hs1)) = &initiator {
                        self.link.send_raw(hs1);
                    }
                    next_hs = now + HELLO_INTERVAL_NS;
                }
            }
            let streaming = last_video > 0 && now - last_video < VIDEO_STALL_NS;
            let want = if !alive {
                Some(if ever_connected { "Connection lost: reconnecting…".to_string() } else { format!("Connecting to {}…", self.server) })
            } else if !got_video && now - alive_since > 700 * MS {
                Some("Waiting for video…".to_string())
            } else {
                None
            };
            if want != status {
                let _ = self.proxy.send_event(UserEvent::Status(want.clone()));
                status = want;
            }
            let version = self.telemetry.prefs_version.load(Relaxed);
            if alive && (version != sent_prefs || !sent_for_session) {
                let prefs = *self.telemetry.prefs.lock().unwrap();
                self.send(Msg::Settings(prefs.for_server()));
                if let Some(p) = &self.audio {
                    p.set_gain(prefs.gain());
                }
                self.telemetry.transfers.set_clipboard(prefs.clipboard);
                (sent_prefs, sent_for_session) = (version, true);
            }
            if !alive {
                sent_for_session = false;
            }
            // Hello makes the server send a frame (a keyframe of an unchanged screen too):
            // needed to get the first one, not while the screen is just still.
            if alive && !got_video && now >= next_hello {
                self.send(Msg::Hello { version: VERSION, caps });
                next_hello = now + HELLO_INTERVAL_NS;
            }
            if now >= next_ping {
                self.send(Msg::Ping { id: ping_id, client_ns: now });
                ping_id = ping_id.wrapping_add(1);
                next_ping = now + PING_INTERVAL_NS;
            }
            self.pump_mic(alive);
            self.input.heartbeat();
            if alive {
                self.pads.poll();
            }
            if self.reassembler.expire(now, PARTIAL_MAX_AGE_NS) {
                self.need_keyframe = true;
            }
            if streaming && self.need_keyframe && now - self.last_keyframe_request > KEYFRAME_RETRY_NS {
                self.send(Msg::KeyframeRequest);
                self.last_keyframe_request = now;
            }
            if now >= next_bin {
                next_bin = now + BIN_NS;
                if let Some(rtt) = self.clock.recent_rtt() {
                    self.binner.rtt(rtt);
                }
                let (delay, cushion) = self.audio.as_ref().and_then(|a| a.delay()).unwrap_or((0.0, 0.0));
                self.binner.audio(delay, cushion);
                if let Some(bin) = self.binner.tick(now) {
                    self.telemetry.history.lock().unwrap().push(bin);
                    if self.telemetry.graph_on.load(Relaxed) {
                        let _ = self.proxy.send_event(UserEvent::Graph);
                    }
                }
            }
            if let Some((mut line, title)) = self.stats.tick(now, &self.clock) {
                if let Some(p) = &mut self.audio {
                    line += &format!("  {}", p.summary());
                }
                println!("{line}");
                let _ = self.proxy.send_event(UserEvent::Title(format!("DisplayWire: {}  ·  {title}", self.server)));
            }

            let n = match self.link.recv(&mut buf) {
                Ok(n) => n,
                Err(e) if is_transient(&e) => continue,
                Err(e) => return Err(e.to_string()),
            };
            let rx = now_ns();
            let datagram = &buf[..n];
            match secure::kind(datagram) {
                Some(secure::HS2) => {
                    if let Some((init, _)) = initiator.take()
                        && let Ok((session, hello)) = init.finish(datagram)
                    {
                        if hello.version != VERSION {
                            return Err(format!("the server speaks protocol {}, this client {VERSION}: update the older one", hello.version));
                        }
                        self.link.set_session(Some(Arc::new(session)));
                        window = ReplayWindow::default();
                        (last_rx, ever_connected, alive_since, got_video) = (rx, true, rx, false);
                        // The server starts again from a keyframe; don't wait for the stall timer.
                        next_hello = 0;
                    }
                    continue;
                }
                Some(secure::SEC) => {}
                _ => continue,
            }
            let Some(session) = self.link.session() else { continue };
            let Some(len) = session.open(datagram, &mut window, &mut plain) else { continue };
            last_rx = rx;
            match Msg::decode(&plain[..len]) {
                Some(Msg::Video(..)) if self.sim_loss.as_mut().is_some_and(SimLoss::drop_this) => {}
                Some(Msg::Video(v, payload)) => {
                    last_video = rx;
                    got_video = true;
                    self.stats.bytes += n as u64;
                    self.binner.bytes(n);
                    if let Some(frame) = self.reassembler.push(v, payload, rx) {
                        self.on_frame(frame);
                    }
                }
                Some(Msg::Audio(a, payload)) => {
                    if let Some(p) = &mut self.audio {
                        let age = self.clock.offset().map(|o| rx as i64 + o - a.capture_ns as i64);
                        p.push(a.seq, payload, age);
                    }
                }
                Some(m @ (Msg::Xfer { .. } | Msg::XferAck { .. } | Msg::XferCancel { .. })) => self.telemetry.transfers.handle(&m),
                Some(Msg::Pong { client_ns, server_ns, .. }) => self.clock.update(rx, client_ns, server_ns),
                Some(Msg::Rumble { slot, large, small }) => self.pads.rumble(slot, large, small),
                Some(Msg::Light { slot, r, g, b }) => self.pads.light(slot, r, g, b),
                _ => {}
            }
        }
        for _ in 0..3 {
            self.send(Msg::Bye);
        }
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) {
        if frame.skipped > 0 {
            self.stats.lost += frame.skipped;
            self.binner.lost(frame.skipped);
            self.need_keyframe = true;
        }
        if self.need_keyframe && !frame.keyframe {
            self.stats.dropped += 1;
            self.binner.lost(1);
            return;
        }
        if frame.repaired > 0 {
            self.stats.repaired += 1;
        }
        let sample = hevc::to_sample(&frame.data);
        if let Some(ps) = sample.param_sets
            && self.decoder.as_ref().is_none_or(|d| d.param_sets != ps)
                && let Err(e) = self.new_decoder(ps) {
                    eprintln!("decoder: {e}");
                    self.need_keyframe = true;
                    return;
                }
        let Some(decoder) = &self.decoder else {
            self.need_keyframe = true;
            return;
        };

        let start = now_ns();
        let submitted = decoder.session.decode(0, &sample.data, false);
        let decoded_ns = now_ns();
        let result = self.decoded.lock().unwrap().take();
        match (submitted, result) {
            (Ok(()), Some(Ok(image))) => {
                self.layer.show(&image);
                self.shown = Some(image);
                if frame.keyframe {
                    self.need_keyframe = false;
                }
                self.stats.frames += 1;
                self.stats.decode.push((decoded_ns - start) as i64);
                // Server clock → client clock: server = client + offset.
                let latency = self.clock.offset().map(|o| decoded_ns as i64 + o - frame.capture_ns as i64);
                if let Some(l) = latency {
                    self.stats.latency.push(l);
                }
                self.binner.frame((decoded_ns - start) as i64, latency);
                if let Some(t) = &mut self.trace {
                    use std::io::Write;
                    let _ = writeln!(
                        t,
                        "{},{},{},{},{},{}",
                        frame.id,
                        frame.keyframe as u8,
                        frame.data.len(),
                        frame.capture_ns,
                        decoded_ns as i64 + self.clock.offset().unwrap_or(0),
                        decoded_ns - start
                    );
                }
            }
            (r, d) => {
                let status = match (&r, &d) {
                    (Err(e), _) => e.clone(),
                    (_, Some(Err(s))) => format!("OSStatus {s}"),
                    _ => "no output".into(),
                };
                eprintln!("decode failed on frame {}: {status}", frame.id);
                self.stats.errors += 1;
                self.binner.lost(1);
                self.need_keyframe = true;
            }
        }
    }

    fn new_decoder(&mut self, param_sets: [Vec<u8>; 3]) -> Result<(), String> {
        let format = Format::hevc(&param_sets)?;
        let (w, h) = format.dimensions();
        let decoded = self.decoded.clone();
        let session = Session::new(
            format,
            Some(PIXEL_FORMAT_NV12),
            Box::new(move |out: Output| {
                let r = match out.image {
                    Some(img) if out.status == 0 => Ok(img),
                    _ => Err(out.status),
                };
                *decoded.lock().unwrap() = Some(r);
            }),
        )?;
        println!("stream {w}x{h}");
        let _ = self.proxy.send_event(UserEvent::VideoSize(w as u32, h as u32));
        self.decoder = Some(Decoder { param_sets, session });
        Ok(())
    }
}

/// Server clock offset from ping/pong pairs, taken from the lowest-RTT
/// recent sample (least queueing, so the most symmetric path).
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

    fn best(&self) -> Option<(u64, i64)> {
        self.samples.iter().copied().min_by_key(|s| s.0)
    }

    fn offset(&self) -> Option<i64> {
        self.best().map(|b| b.1)
    }

    fn recent_rtt(&self) -> Option<u64> {
        let n = self.samples.len().min(50);
        let mut v: Vec<u64> = self.samples.iter().rev().take(n).map(|s| s.0).collect();
        v.sort_unstable();
        v.get(v.len() / 2).copied()
    }
}

/// Drops a fixed share of packets at random (a debugging aid, see `sim_loss`).
struct SimLoss {
    percent: f64,
    state: u64,
}

impl SimLoss {
    fn new(percent: f64) -> SimLoss {
        println!("simulating {percent}% video packet loss");
        SimLoss { percent, state: 0x9E37_79B9_7F4A_7C15 }
    }

    fn drop_this(&mut self) -> bool {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        (self.state >> 11) as f64 / (1u64 << 53) as f64 * 100.0 < self.percent
    }
}

struct Stats {
    window_start: u64,
    /// Frames that lost packets but were rebuilt from repair packets.
    repaired: u32,
    frames: u32,
    bytes: u64,
    dropped: u32,
    lost: u32,
    errors: u32,
    decode: Samples,
    latency: Samples,
}

impl Stats {
    fn new() -> Self {
        Stats {
            window_start: now_ns(),
            repaired: 0,
            frames: 0,
            bytes: 0,
            dropped: 0,
            lost: 0,
            errors: 0,
            decode: Samples::default(),
            latency: Samples::default(),
        }
    }

    /// Once a second: the full stats line for the console, and the short one for the title bar.
    fn tick(&mut self, now: u64, clock: &Clock) -> Option<(String, String)> {
        let elapsed = now - self.window_start;
        if elapsed < SEC {
            return None;
        }
        let lat = self.latency.summary();
        let dec = self.decode.summary();
        let mbps = self.bytes as f64 * 8.0 / elapsed as f64 * 1e3;
        let rtt = clock.recent_rtt().map_or(0.0, |r| ms(r as i64));
        let title = format!("{} fps  ·  {mbps:.0} Mbps  ·  rtt {rtt:.0} ms", self.frames);
        let mut line = format!(
            "{} fps  {mbps:.0} Mbps  capture→decoded {:.1} ms (p99 {:.1})  decode {:.1} ms  rtt {rtt:.1} ms",
            self.frames,
            ms(lat.p50),
            ms(lat.p99),
            ms(dec.p50),
        );
        if self.lost + self.dropped + self.errors > 0 {
            line += &format!("  lost {} dropped {} errors {}", self.lost, self.dropped, self.errors);
        }
        if self.repaired > 0 {
            line += &format!("  fec-repaired {}", self.repaired);
        }
        *self = Stats { window_start: now, ..Stats::new() };
        Some((line, title))
    }
}
