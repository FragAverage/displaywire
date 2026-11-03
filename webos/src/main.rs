//! DisplayWire for LG webOS TVs (prototype). Milestone 1 is headless: pair with a PC, connect,
//! and report what arrives, so the protocol and the cross-built binary can be checked on the TV
//! before any of LG's video APIs are involved.
//!
//!   displaywire-tv pair 192.168.1.20            # then type the PIN the PC shows
//!   displaywire-tv connect 192.168.1.20         # stats once a second
//!   displaywire-tv connect 192.168.1.20 --dump stream.hevc --seconds 10
//!
//! The dump is the raw HEVC stream (Annex B), playable with `ffplay stream.hevc`.
//!
//! Milestone 2, on the TV only: `connect --show` puts the stream on screen through LG's NDL,
//! and `play stream.hevc` does the same with a dump, to try NDL without a PC.

#[cfg(all(target_arch = "arm", target_env = "gnu"))]
mod compat;
#[cfg(target_os = "linux")]
mod dl;
#[cfg(target_os = "linux")]
mod lgnc;
#[cfg(target_os = "linux")]
mod ndl;
#[cfg(target_os = "linux")]
mod screen;
mod session;
mod store;
#[cfg(target_os = "linux")]
mod tv;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use clap::{Parser, Subcommand};
use pairing::{Pairing, Progress};

const DEFAULT_PORT: u16 = 9000;

#[derive(Parser)]
#[command(version, about = "DisplayWire client for LG webOS TVs (prototype)")]
struct Cli {
    /// Where paired PCs and their keys are kept (default: $HOME/.displaywire-tv).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a PC: choose "Pair new device…" in its tray menu first.
    Pair {
        address: String,
        /// What the PC's device list calls this TV.
        #[arg(long, default_value = "LG TV")]
        name: String,
    },
    /// Connect to a paired PC and print stream stats.
    Connect {
        address: String,
        /// Write the received HEVC stream to this file.
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Stop after this many seconds (default: run until killed).
        #[arg(long)]
        seconds: Option<u64>,
        /// Show the stream on the TV's screen.
        #[arg(long)]
        show: bool,
        #[command(flatten)]
        video: VideoArgs,
    },
    /// Play an Annex B file (a `connect --dump`, or H.264) on the TV's screen, in a loop.
    Play {
        file: PathBuf,
        #[command(flatten)]
        video: VideoArgs,
        #[arg(long, default_value_t = 60)]
        fps: u32,
    },
    /// List paired PCs.
    List,
    /// Only open the app's (transparent) window, for decoding from another process.
    #[command(hide = true)]
    Window,
}

/// Which of LG's decoders to use.
#[derive(clap::Args, Clone, Copy)]
struct VideoArgs {
    #[cfg(target_os = "linux")]
    #[arg(long, value_enum, default_value = "lgnc")]
    backend: tv::Backend,
    /// NDL video source type (1 is H.264, the only one LG's headers name).
    #[arg(long, default_value_t = 1)]
    source: i32,
}

fn main() {
    let cli = match launched_as_app() {
        Some(args) => Cli::parse_from(args),
        None => Cli::parse(),
    };
    let store = store::Store::new(cli.state_dir);
    let result = match cli.cmd {
        Cmd::Pair { address, name } => pair(&store, &address, &name),
        Cmd::Connect { address, dump, seconds, show, video } => connect(&store, &address, dump, seconds, show.then_some(video)),
        Cmd::Play { file, video, fps } => play(&file, video, fps),
        Cmd::Window => window(),
        Cmd::List => store.load().map(|servers| {
            for s in servers {
                println!("{}  {}", s.address, s.name);
            }
        }),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn pair(store: &store::Store, address: &str, name: &str) -> Result<(), String> {
    let addr = transport::resolve(address, DEFAULT_PORT).map_err(|e| e.to_string())?;
    let pairing = Pairing::start(addr, name).map_err(|e| e.to_string())?;
    let mut shown = None;
    loop {
        let progress = pairing.progress();
        if shown.as_ref() != Some(&progress) {
            match &progress {
                Progress::Contacting => println!("asking {addr} to pair…"),
                Progress::NoReply => println!("no reply yet: is \"Pair new device…\" open on the PC?"),
                Progress::WaitingForAllow => println!("click Allow on the PC"),
                Progress::NeedPin => {
                    print!("PIN shown on the PC: ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
                    pairing.submit_pin(line.trim());
                }
                Progress::Checking => println!("checking…"),
                Progress::WrongPin { tries_left } => {
                    println!("wrong PIN, {tries_left} tries left");
                    pairing.retry();
                }
                Progress::Denied => return Err("the PC said no".into()),
                Progress::Busy => println!("another device is pairing with the PC; waiting"),
                Progress::Locked => return Err("too many wrong PINs, or the PIN expired: start again on the PC".into()),
                Progress::Cancelled => return Err("cancelled".into()),
                Progress::Failed(e) => return Err(e.clone()),
                Progress::Done { key, server_name } => {
                    store.upsert(address, server_name, key)?;
                    println!("paired with {server_name}");
                    return Ok(());
                }
            }
            shown = Some(progress);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `show`: how to show the stream on screen.
fn connect(store: &store::Store, address: &str, dump: Option<PathBuf>, seconds: Option<u64>, show: Option<VideoArgs>) -> Result<(), String> {
    let server = store
        .find(address)?
        .ok_or_else(|| format!("{address} isn't paired: run `displaywire-tv pair {address}` first"))?;
    let addr = transport::resolve(address, DEFAULT_PORT).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(s) = seconds {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(s));
            stop.store(true, Relaxed);
        });
    }
    if let Some(video) = show {
        return show_stream(addr, server.key, video, stop);
    }
    let sink: Box<dyn session::VideoSink + Send> = match dump {
        Some(path) => Box::new(session::Dump::create(&path)?),
        None => Box::new(session::Discard),
    };
    session::run(addr, server.key, 0, sink, &stop)
}

#[cfg(target_os = "linux")]
fn show_stream(addr: std::net::SocketAddr, key: secure::Key, video: VideoArgs, stop: Arc<AtomicBool>) -> Result<(), String> {
    let caps = video.backend.caps();
    tv::show(video.backend, video.source, stop, move |sink, stop| session::run(addr, key, caps, sink, stop))
}

#[cfg(not(target_os = "linux"))]
fn show_stream(_: std::net::SocketAddr, _: secure::Key, _: VideoArgs, _: Arc<AtomicBool>) -> Result<(), String> {
    Err("--show only works on the TV".into())
}

#[cfg(target_os = "linux")]
fn play(file: &std::path::Path, video: VideoArgs, fps: u32) -> Result<(), String> {
    tv::play(file, video.backend, video.source, fps)
}

#[cfg(target_os = "linux")]
fn window() -> Result<(), String> {
    tv::window()
}

#[cfg(not(target_os = "linux"))]
fn window() -> Result<(), String> {
    Err("window only works on the TV".into())
}

#[cfg(not(target_os = "linux"))]
fn play(_: &std::path::Path, _: VideoArgs, _: u32) -> Result<(), String> {
    Err("play only works on the TV".into())
}

/// When the TV launches us as an app, the only argument is the launch parameters (JSON). The
/// prototype then takes its command line from `args` (one per line) and environment from `env`
/// (`KEY=value` lines) beside the binary, and logs to `DW_LOG` or the developer temp folder.
/// The binary has to be the app's `main` itself: LG's services identify the app by its executable.
fn launched_as_app() -> Option<Vec<String>> {
    let first = std::env::args().nth(1)?;
    if !first.starts_with('{') {
        return None;
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let _ = std::env::set_current_dir(&dir);
    for line in std::fs::read_to_string("env").unwrap_or_default().lines() {
        if let Some((k, v)) = line.trim().strip_prefix("export ").unwrap_or(line.trim()).split_once('=') {
            // SAFETY: still single-threaded.
            unsafe { std::env::set_var(k, v) };
        }
    }
    let log = std::env::var("DW_LOG").unwrap_or_else(|_| "/media/developer/temp/dw/app.log".into());
    redirect_output(&log);
    println!("=== launched as an app: {first}");
    let mut args = vec!["displaywire-tv".to_string()];
    args.extend(std::fs::read_to_string("args").unwrap_or_default().lines().map(str::to_string).filter(|l| !l.is_empty()));
    Some(args)
}

#[cfg(unix)]
fn redirect_output(path: &str) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn dup2(old: i32, new: i32) -> i32;
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        // SAFETY: plain fd duplication onto stdout and stderr.
        unsafe {
            dup2(f.as_raw_fd(), 1);
            dup2(f.as_raw_fd(), 2);
        }
    }
}

#[cfg(not(unix))]
fn redirect_output(_: &str) {}
