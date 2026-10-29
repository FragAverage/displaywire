//! streamy-server: captures the desktop, encodes HEVC and streams it to one
//! client over UDP.
//!
//!   streamy-server                       # Windows: stream the primary monitor
//!   streamy-server --file clip.h265      # any OS: loop an Annex B file (testing)

mod audio;
mod auth;
mod file;
#[cfg(windows)]
mod windows;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use auth::Link;
use proto::stream::{Msg, VERSION};
use secure::ReplayWindow;
use timing::{now_ns, sleep_until};
use transport::{is_transient, udp_socket, video, wlan};

const SEC: u64 = 1_000_000_000;
/// Client is dropped after this long without a ping.
const CLIENT_TIMEOUT_NS: u64 = 3 * SEC;

#[cfg(windows)]
use windows::input::Injector;

/// Input is only injected on Windows.
#[cfg(not(windows))]
struct Injector;

#[cfg(not(windows))]
impl Injector {
    fn new() -> Injector {
        Injector
    }
    fn reset(&mut self) {}
    fn tick(&mut self, _now_ns: u64) {}
    fn apply(&mut self, _input: &proto::stream::Input, _now_ns: u64) {}
}

#[derive(Parser)]
#[command(about = "Stream this machine's screen to a streamy client")]
pub struct Cli {
    #[arg(long, default_value_t = 9000)]
    port: u16,
    /// Target frame rate (the encoder's rate-control assumption; capture follows the display).
    #[arg(long, default_value_t = 120)]
    pub fps: u32,
    /// Encoder bitrate.
    #[arg(long, default_value_t = 50.0)]
    pub mbps: f64,
    /// Rate at which a frame's packets are paced onto the network.
    #[arg(long, default_value_t = 800.0)]
    pace_mbps: f64,
    /// Stream an HEVC Annex B file (with AUDs) in a loop instead of the screen.
    #[arg(long)]
    file: Option<std::path::PathBuf>,
    /// Leave Windows Wi-Fi background scanning and power settings alone.
    #[arg(long)]
    no_wifi_tuning: bool,
    /// Repair packets per frame, as a percentage of its data packets (0 turns FEC off).
    #[arg(long, default_value_t = 10)]
    fec: u32,
    /// Where the pairing key lives (default: next to the server's data, readable only by administrators).
    #[arg(long)]
    key_file: Option<std::path::PathBuf>,
    /// Print the pairing key and exit.
    #[arg(long)]
    show_key: bool,
    /// Replace the pairing key and exit. Every paired client needs the new one.
    #[arg(long)]
    new_key: bool,
    /// Don't send desktop audio.
    #[arg(long)]
    no_audio: bool,
    /// Send a test tone instead of the desktop's audio.
    #[arg(long)]
    tone: bool,
    /// Windows video encoder: `auto` (direct NVENC where possible, else Media Foundation),
    /// `nvenc` (NVIDIA only) or `mft` (Media Foundation).
    #[arg(long, default_value = "auto")]
    pub encoder: String,
    /// Windows: time each stage of the video pipeline (waits for the GPU after
    /// each frame's conversion, so it costs a little latency) and print it every second.
    #[arg(long)]
    pub profile: bool,
    /// Windows: install and start a service that runs this server as SYSTEM
    /// (streams UAC prompts and the lock screen), with the other options given.
    #[arg(long)]
    install_service: bool,
    /// Windows: stop and remove the service.
    #[arg(long)]
    uninstall_service: bool,
    /// Run as the service (started by Windows; not for manual use).
    #[arg(long, hide = true)]
    service: bool,
}

pub struct EncodedFrame {
    /// HEVC Annex B access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub capture_ns: u64,
}

pub trait Source {
    /// Waits up to about `timeout_ms` for the next frame. With `force_keyframe`,
    /// the next frame returned must be a keyframe (re-encoding the last image if
    /// nothing changed).
    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String>;
}

#[derive(Default)]
struct Shared {
    /// The authenticated client, if any.
    link: Mutex<Option<Link>>,
    last_seen_ns: AtomicU64,
    keyframe: AtomicBool,
    /// `CAP_*` flags the client asked for in its hello.
    caps: AtomicU8,
}

fn main() {
    // Keep the console open on a crash too, so a double-clicked exe shows why it stopped.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        wait_for_enter();
    }));
    let cli = Cli::parse();
    now_ns();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        wait_for_enter();
        std::process::exit(1);
    }
}

/// Lets the error be read before a double-clicked console window closes.
fn wait_for_enter() {
    eprintln!("press Enter to exit");
    let _ = std::io::stdin().read_line(&mut String::new());
}

/// `--show-key` and `--new-key`.
fn key_command(cli: &Cli, path: &std::path::Path) -> Option<Result<(), String>> {
    if cli.new_key {
        return Some(auth::write_new(path).map(|k| {
            println!("new pairing key (every client needs it): {k}");
        }));
    }
    cli.show_key.then(|| {
        let (k, created) = auth::load_or_create(path)?;
        println!("{}pairing key: {k}", if created { "created " } else { "" });
        println!("stored at {}", path.display());
        Ok(())
    })
}

#[cfg(windows)]
fn service_command(cli: &Cli, key_path: &std::path::Path) -> Option<Result<(), String>> {
    use windows::service;
    if cli.install_service {
        let (key, created) = match auth::load_or_create(key_path) {
            Ok(k) => k,
            Err(e) => return Some(Err(e)),
        };
        println!("{}pairing key: {key}", if created { "created " } else { "" });
        println!("Enter it once in Streamy on each client (or: streamy-client --set-key <this PC> <key>).");
        let args: Vec<_> = std::env::args_os().skip(1).filter(|a| a != "--install-service").collect();
        Some(service::install(&args))
    } else if cli.uninstall_service {
        Some(service::uninstall())
    } else if cli.service {
        Some(service::run())
    } else {
        None
    }
}

#[cfg(not(windows))]
fn service_command(cli: &Cli, _key_path: &std::path::Path) -> Option<Result<(), String>> {
    (cli.install_service || cli.uninstall_service || cli.service).then(|| Err("the service is Windows only".into()))
}

fn run(cli: Cli) -> Result<(), String> {
    let key_path = cli.key_file.clone().unwrap_or_else(auth::default_key_path);
    if let Some(result) = key_command(&cli, &key_path) {
        return result;
    }
    if let Some(result) = service_command(&cli, &key_path) {
        return result;
    }
    let (key, created) = auth::load_or_create(&key_path)?;
    if created {
        println!("created pairing key {key}; stored at {}", key_path.display());
    }
    let _tuning = (!cli.no_wifi_tuning).then(wlan::tune).flatten();
    let mut source: Box<dyn Source> = match &cli.file {
        Some(path) => Box::new(file::FileSource::open(path, cli.fps)?),
        None => open_screen(&cli)?,
    };

    let sock = udp_socket(SocketAddr::from(([0, 0, 0, 0], cli.port))).map_err(|e| format!("bind udp/{}: {e}", cli.port))?;
    let shared = Arc::new(Shared::default());
    {
        let sock = sock.try_clone().map_err(|e| e.to_string())?;
        let shared = shared.clone();
        std::thread::spawn(move || control(&sock, &shared, &key));
    }
    if !cli.no_audio {
        let source: Option<Box<dyn audio::AudioSource>> = if cli.tone || cli.file.is_some() {
            Some(Box::new(audio::Tone::new()))
        } else {
            open_audio()
        };
        if let Some(source) = source {
            audio::spawn(sock.try_clone().map_err(|e| e.to_string())?, shared.clone(), source);
        }
    }
    println!("streamy-server on udp/{}, waiting for a client", cli.port);

    let pace_ns_per_byte = 8e3 / cli.pace_mbps;
    let mut frame_id = 0u32;
    let mut stats = Stats::new();
    let mut sealed = [0u8; proto::MAX_DATAGRAM];
    loop {
        let link = shared.link.lock().unwrap().clone();
        let Some(link) = link else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        if now_ns().saturating_sub(shared.last_seen_ns.load(Relaxed)) > CLIENT_TIMEOUT_NS {
            println!("{}: timed out", link.addr);
            shared.link.lock().unwrap().take_if(|l| l.addr == link.addr);
            continue;
        }

        let force = shared.keyframe.swap(false, Relaxed);
        let Some(frame) = source.next_frame(force, 100)? else {
            if force {
                shared.keyframe.store(true, Relaxed);
            }
            continue;
        };
        if force && !frame.keyframe {
            shared.keyframe.store(true, Relaxed);
        }

        let encoded_ns = now_ns();
        let mut next_send = encoded_ns;
        video::packetize(frame_id, frame.keyframe, frame.capture_ns, &frame.data, cli.fec, |pkt| {
            sleep_until(next_send);
            let wire = link.send(&sock, pkt, &mut sealed);
            next_send = next_send.max(now_ns()) + (wire as f64 * pace_ns_per_byte) as u64;
        });
        frame_id = frame_id.wrapping_add(1);
        stats.frame(&frame, encoded_ns, now_ns());
    }
}

#[cfg(windows)]
fn open_screen(cli: &Cli) -> Result<Box<dyn Source>, String> {
    Ok(Box::new(windows::ScreenSource::open(cli)?))
}

#[cfg(windows)]
fn open_audio() -> Option<Box<dyn audio::AudioSource>> {
    match windows::loopback::Loopback::open() {
        Ok(l) => Some(Box::new(l)),
        Err(e) => {
            eprintln!("audio capture unavailable, streaming video only: {e}");
            None
        }
    }
}

#[cfg(not(windows))]
fn open_audio() -> Option<Box<dyn audio::AudioSource>> {
    None
}

#[cfg(not(windows))]
fn open_screen(_cli: &Cli) -> Result<Box<dyn Source>, String> {
    Err("screen capture is only implemented on Windows; use --file".into())
}

/// Handshakes accepted per second, so a flood of (cheap to check) forgeries stays cheap.
struct HandshakeLimiter {
    tokens: f64,
    last_ns: u64,
}

impl HandshakeLimiter {
    const PER_SEC: f64 = 20.0;
    const BURST: f64 = 40.0;

    fn allow(&mut self, now: u64) -> bool {
        self.tokens = (self.tokens + (now - self.last_ns) as f64 / SEC as f64 * Self::PER_SEC).min(Self::BURST);
        self.last_ns = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A client that has completed a handshake but not yet sent anything encrypted.
struct Pending {
    addr: SocketAddr,
    session: Arc<secure::Session>,
    window: ReplayWindow,
    caps: u8,
    created_ns: u64,
}

/// Pending handshakes kept, and for how long.
const MAX_PENDING: usize = 16;
const PENDING_TTL_NS: u64 = 10 * SEC;

/// Handles client datagrams: handshakes, then (encrypted) session start/stop,
/// pings, keyframe requests and input. Anything else is dropped silently.
fn control(sock: &UdpSocket, shared: &Shared, key: &secure::Key) {
    let mut buf = [0u8; 2048];
    let mut plain = [0u8; 2048];
    let mut out = [0u8; proto::MAX_DATAGRAM];
    let mut injector = Injector::new();
    let mut active: Option<(Link, ReplayWindow)> = None;
    let mut pending: Vec<Pending> = Vec::new();
    let mut limiter = HandshakeLimiter { tokens: HandshakeLimiter::BURST, last_ns: now_ns() };
    let mut last_reject_log = 0u64;
    // Wake up regularly so held keys are released if the client goes quiet.
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    loop {
        let now = now_ns();
        injector.tick(now);
        if shared.link.lock().unwrap().is_none() {
            active = None;
        }
        pending.retain(|p| now - p.created_ns < PENDING_TTL_NS);
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if is_transient(&e) => continue,
            Err(e) => {
                eprintln!("control socket: {e}");
                return;
            }
        };
        let datagram = &buf[..n];
        match secure::kind(datagram) {
            Some(secure::HS1) => {
                if !limiter.allow(now) {
                    continue;
                }
                match secure::accept(key, datagram, auth::unix_ms(), VERSION) {
                    Ok((hs2, session, hello)) => {
                        let _ = sock.send_to(&hs2, from);
                        if hello.version != VERSION {
                            eprintln!("{from}: client speaks protocol {}, this server {VERSION}; update one of them", hello.version);
                            continue;
                        }
                        if pending.len() == MAX_PENDING {
                            pending.remove(0);
                        }
                        pending.push(Pending { addr: from, session: Arc::new(session), window: ReplayWindow::default(), caps: hello.caps, created_ns: now });
                    }
                    Err(_) => {
                        if now - last_reject_log > 10 * SEC {
                            last_reject_log = now;
                            eprintln!("{from}: handshake rejected (wrong pairing key, or clocks more than 5 minutes apart)");
                        }
                    }
                }
            }
            Some(secure::SEC) => {
                let Some((id, _)) = secure::parse_header(datagram) else { continue };
                let opened = if let Some((link, window)) = active.as_mut().filter(|(l, _)| l.addr == from && l.session.id() == id) {
                    link.session.open(datagram, window, &mut plain)
                } else if let Some(i) = pending.iter().position(|p| p.addr == from && p.session.id() == id) {
                    // First datagram that proves the client holds the key: it becomes the active one.
                    let p = &mut pending[i];
                    let Some(len) = p.session.open(datagram, &mut p.window, &mut plain) else { continue };
                    let p = pending.swap_remove(i);
                    pending.retain(|o| o.addr != from);
                    let link = Link { addr: from, session: p.session };
                    println!("{from}: connected");
                    *shared.link.lock().unwrap() = Some(link.clone());
                    shared.caps.store(p.caps, Relaxed);
                    shared.last_seen_ns.store(now, Relaxed);
                    shared.keyframe.store(true, Relaxed);
                    injector.reset();
                    active = Some((link, p.window));
                    Some(len)
                } else {
                    None
                };
                let Some(len) = opened else { continue };
                match Msg::decode(&plain[..len]) {
                    Some(Msg::Hello { caps, .. }) => {
                        shared.caps.store(caps, Relaxed);
                        shared.last_seen_ns.store(now_ns(), Relaxed);
                        shared.keyframe.store(true, Relaxed);
                    }
                    Some(Msg::Ping { id, client_ns }) => {
                        shared.last_seen_ns.store(now_ns(), Relaxed);
                        let mut pong = [0u8; 64];
                        let m = Msg::Pong { id, client_ns, server_ns: now_ns() }.encode(&mut pong);
                        if let Some((link, _)) = &active {
                            link.send(sock, &pong[..m], &mut out);
                        }
                    }
                    Some(Msg::KeyframeRequest) => shared.keyframe.store(true, Relaxed),
                    Some(Msg::Input(input)) => injector.apply(&input, now_ns()),
                    Some(Msg::Bye) => {
                        println!("{from}: disconnected");
                        *shared.link.lock().unwrap() = None;
                        active = None;
                        injector.reset();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

struct Stats {
    window_start: u64,
    frames: u32,
    keyframes: u32,
    bytes: u64,
    encode_ns: u64,
    send_ns: u64,
}

impl Stats {
    fn new() -> Self {
        Stats { window_start: now_ns(), frames: 0, keyframes: 0, bytes: 0, encode_ns: 0, send_ns: 0 }
    }

    fn frame(&mut self, f: &EncodedFrame, encoded_ns: u64, sent_ns: u64) {
        self.frames += 1;
        self.keyframes += f.keyframe as u32;
        self.bytes += f.data.len() as u64;
        self.encode_ns += encoded_ns.saturating_sub(f.capture_ns);
        self.send_ns += sent_ns - encoded_ns;
        let elapsed = sent_ns - self.window_start;
        if elapsed >= SEC {
            let n = self.frames.max(1) as f64;
            println!(
                "{:>4} fps  {:>6.1} Mbps  capture→encoded {:.2} ms  send {:.2} ms  keyframes {}",
                self.frames,
                self.bytes as f64 * 8.0 / elapsed as f64 * 1e3,
                self.encode_ns as f64 / n / 1e6,
                self.send_ns as f64 / n / 1e6,
                self.keyframes
            );
            *self = Stats { window_start: sent_ns, ..Stats::new() };
        }
    }
}
