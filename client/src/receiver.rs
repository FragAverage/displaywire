//! Network thread: receive → reassemble → decode → present, plus session
//! upkeep (hello, pings for RTT/clock offset, keyframe requests).

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
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
use crate::display::Layer;
use crate::input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const PING_INTERVAL_NS: u64 = 20 * MS;
const HELLO_INTERVAL_NS: u64 = 200 * MS;
/// Width of one graph sample, and how many to keep (60 s).
const BIN_NS: u64 = 100 * MS;
pub const HISTORY_BINS: usize = 600;
const KEYFRAME_RETRY_NS: u64 = 100 * MS;
/// Incomplete frames older than this are given up on.
const PARTIAL_MAX_AGE_NS: u64 = 100 * MS;
/// No video for this long: assume the server dropped us and say hello again.
const VIDEO_STALL_NS: u64 = 2 * SEC;

/// `sock` must be connected to `server`; `input` re-sends its state from this thread.
pub fn run(
    server: SocketAddr,
    sock: UdpSocket,
    input: Arc<input::Sender>,
    layer: Layer,
    proxy: EventLoopProxy<UserEvent>,
    history: Arc<Mutex<History>>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = Receiver::new(server, sock, input, layer, proxy.clone(), history).and_then(|mut r| r.run(&stop)) {
        let _ = proxy.send_event(UserEvent::Fatal(e));
    }
}

struct Decoder {
    param_sets: [Vec<u8>; 3],
    session: Session,
}

struct Receiver {
    server: SocketAddr,
    sock: UdpSocket,
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
    history: Arc<Mutex<History>>,
}

impl Receiver {
    fn new(
        server: SocketAddr,
        sock: UdpSocket,
        input: Arc<input::Sender>,
        layer: Layer,
        proxy: EventLoopProxy<UserEvent>,
        history: Arc<Mutex<History>>,
    ) -> Result<Receiver, String> {
        sock.set_read_timeout(Some(Duration::from_millis(2))).map_err(|e| e.to_string())?;
        Ok(Receiver {
            server,
            sock,
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
            history,
        })
    }

    fn send(&self, msg: Msg) {
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf);
        let _ = self.sock.send(&buf[..n]);
    }

    fn run(&mut self, stop: &AtomicBool) -> Result<(), String> {
        let begin = now_ns();
        let (mut next_hello, mut next_ping, mut next_bin, mut ping_id) = (0, 0, 0, 0u32);
        let mut last_video = 0u64;
        let mut buf = [0u8; 2048];
        println!("connecting to {}", self.server);

        while !stop.load(Relaxed) {
            let now = now_ns();
            let streaming = last_video > 0 && now - last_video < VIDEO_STALL_NS;
            if !streaming && now >= next_hello {
                if last_video == 0 && now - begin > 5 * SEC && self.clock.is_empty() {
                    return Err(format!(
                        "no reply from {}: is streamy-server running and udp/{} allowed through its firewall?",
                        self.server,
                        self.server.port()
                    ));
                }
                self.send(Msg::Hello { version: VERSION, caps: CAP_AUDIO });
                next_hello = now + HELLO_INTERVAL_NS;
            }
            if now >= next_ping {
                self.send(Msg::Ping { id: ping_id, client_ns: now });
                ping_id = ping_id.wrapping_add(1);
                next_ping = now + PING_INTERVAL_NS;
            }
            self.input.heartbeat();
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
                if let Some(bin) = self.binner.tick(now) {
                    self.history.lock().unwrap().push(bin);
                }
            }
            if let Some(line) = self.stats.tick(now, &self.clock) {
                println!("{line}");
                let _ = self.proxy.send_event(UserEvent::Title(format!("Streamy: {}  |  {line}", self.server)));
            }

            let n = match self.sock.recv(&mut buf) {
                Ok(n) => n,
                Err(e) if is_transient(&e) => continue,
                Err(e) => return Err(e.to_string()),
            };
            let rx = now_ns();
            match Msg::decode(&buf[..n]) {
                Some(Msg::Video(v, payload)) => {
                    last_video = rx;
                    self.stats.bytes += n as u64;
                    self.binner.bytes(n);
                    if let Some(frame) = self.reassembler.push(v, payload, rx) {
                        self.on_frame(frame);
                    }
                }
                Some(Msg::Pong { client_ns, server_ns, .. }) => self.clock.update(rx, client_ns, server_ns),
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

    fn is_empty(&self) -> bool {
        self.samples.is_empty()
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

struct Stats {
    window_start: u64,
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
            frames: 0,
            bytes: 0,
            dropped: 0,
            lost: 0,
            errors: 0,
            decode: Samples::default(),
            latency: Samples::default(),
        }
    }

    fn tick(&mut self, now: u64, clock: &Clock) -> Option<String> {
        let elapsed = now - self.window_start;
        if elapsed < SEC {
            return None;
        }
        let lat = self.latency.summary();
        let dec = self.decode.summary();
        let mut line = format!(
            "{} fps  {:.0} Mbps  capture→decoded {:.1} ms (p99 {:.1})  decode {:.1} ms  rtt {:.1} ms",
            self.frames,
            self.bytes as f64 * 8.0 / elapsed as f64 * 1e3,
            ms(lat.p50),
            ms(lat.p99),
            ms(dec.p50),
            clock.recent_rtt().map_or(0.0, |r| ms(r as i64)),
        );
        if self.lost + self.dropped + self.errors > 0 {
            line += &format!("  lost {} dropped {} errors {}", self.lost, self.dropped, self.errors);
        }
        *self = Stats { window_start: now, ..Stats::new() };
        Some(line)
    }
}
