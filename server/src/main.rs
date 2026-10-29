//! streamy-server: captures the desktop, encodes HEVC and streams it to one
//! client over UDP.
//!
//!   streamy-server                       # Windows: stream the primary monitor
//!   streamy-server --file clip.h265      # any OS: loop an Annex B file (testing)

mod file;
#[cfg(windows)]
mod windows;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use proto::stream::{Msg, VERSION};
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
    peer: Mutex<Option<SocketAddr>>,
    last_seen_ns: AtomicU64,
    keyframe: AtomicBool,
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

#[cfg(windows)]
fn service_command(cli: &Cli) -> Option<Result<(), String>> {
    use windows::service;
    if cli.install_service {
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
fn service_command(cli: &Cli) -> Option<Result<(), String>> {
    (cli.install_service || cli.uninstall_service || cli.service).then(|| Err("the service is Windows only".into()))
}

fn run(cli: Cli) -> Result<(), String> {
    if let Some(result) = service_command(&cli) {
        return result;
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
        std::thread::spawn(move || control(&sock, &shared));
    }
    println!("streamy-server on udp/{}, waiting for a client", cli.port);

    let pace_ns_per_byte = 8e3 / cli.pace_mbps;
    let mut frame_id = 0u32;
    let mut stats = Stats::new();
    loop {
        let peer = *shared.peer.lock().unwrap();
        let Some(peer) = peer else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        if now_ns().saturating_sub(shared.last_seen_ns.load(Relaxed)) > CLIENT_TIMEOUT_NS {
            println!("{peer}: timed out");
            shared.peer.lock().unwrap().take_if(|p| *p == peer);
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
        video::packetize(frame_id, frame.keyframe, frame.capture_ns, &frame.data, |pkt| {
            sleep_until(next_send);
            let _ = sock.send_to(pkt, peer);
            next_send = next_send.max(now_ns()) + (pkt.len() as f64 * pace_ns_per_byte) as u64;
        });
        frame_id = frame_id.wrapping_add(1);
        stats.frame(&frame, encoded_ns, now_ns());
    }
}

#[cfg(windows)]
fn open_screen(cli: &Cli) -> Result<Box<dyn Source>, String> {
    Ok(Box::new(windows::ScreenSource::open(cli)?))
}

#[cfg(not(windows))]
fn open_screen(_cli: &Cli) -> Result<Box<dyn Source>, String> {
    Err("screen capture is only implemented on Windows; use --file".into())
}

/// Handles client datagrams: session start/stop, pings, keyframe requests, input.
fn control(sock: &UdpSocket, shared: &Shared) {
    let mut buf = [0u8; 2048];
    let mut out = [0u8; 64];
    let mut injector = Injector::new();
    // Wake up regularly so held keys are released if the client goes quiet.
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    loop {
        injector.tick(now_ns());
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if is_transient(&e) => continue,
            Err(e) => {
                eprintln!("control socket: {e}");
                return;
            }
        };
        let is_peer = *shared.peer.lock().unwrap() == Some(from);
        match Msg::decode(&buf[..n]) {
            Some(Msg::Hello { version }) => {
                if version != VERSION {
                    eprintln!("{from}: protocol version {version}, expected {VERSION}; ignoring");
                    continue;
                }
                if !is_peer {
                    println!("{from}: connected");
                    *shared.peer.lock().unwrap() = Some(from);
                    injector.reset();
                }
                shared.last_seen_ns.store(now_ns(), Relaxed);
                shared.keyframe.store(true, Relaxed);
            }
            Some(Msg::Ping { id, client_ns }) if is_peer => {
                shared.last_seen_ns.store(now_ns(), Relaxed);
                let n = Msg::Pong { id, client_ns, server_ns: now_ns() }.encode(&mut out);
                let _ = sock.send_to(&out[..n], from);
            }
            Some(Msg::KeyframeRequest) if is_peer => shared.keyframe.store(true, Relaxed),
            Some(Msg::Input(input)) if is_peer => injector.apply(&input, now_ns()),
            Some(Msg::Bye) if is_peer => {
                println!("{from}: disconnected");
                *shared.peer.lock().unwrap() = None;
                injector.reset();
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
