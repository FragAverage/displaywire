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
        /// NDL video source type (1 is H.264, the only one LG's headers name).
        #[arg(long, default_value_t = 1)]
        source: i32,
    },
    /// Play a dump from `connect --dump` on the TV's screen, in a loop.
    Play {
        file: PathBuf,
        #[arg(long, default_value_t = 1)]
        source: i32,
        #[arg(long, default_value_t = 60)]
        fps: u32,
    },
    /// List paired PCs.
    List,
    /// Only open the app's (transparent) window, for decoding from another process.
    #[command(hide = true)]
    Window,
}

fn main() {
    let cli = Cli::parse();
    let store = store::Store::new(cli.state_dir);
    let result = match cli.cmd {
        Cmd::Pair { address, name } => pair(&store, &address, &name),
        Cmd::Connect { address, dump, seconds, show, source } => connect(&store, &address, dump, seconds, show.then_some(source)),
        Cmd::Play { file, source, fps } => play(&file, source, fps),
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

/// `show`: the NDL source type to show the stream with.
fn connect(store: &store::Store, address: &str, dump: Option<PathBuf>, seconds: Option<u64>, show: Option<i32>) -> Result<(), String> {
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
    if let Some(source) = show {
        return show_stream(addr, server.key, source, stop);
    }
    let sink: Box<dyn session::VideoSink + Send> = match dump {
        Some(path) => Box::new(session::Dump::create(&path)?),
        None => Box::new(session::Discard),
    };
    session::run(addr, server.key, sink, &stop)
}

#[cfg(target_os = "linux")]
fn show_stream(addr: std::net::SocketAddr, key: secure::Key, source: i32, stop: Arc<AtomicBool>) -> Result<(), String> {
    tv::show(source, stop, move |sink, stop| session::run(addr, key, sink, stop))
}

#[cfg(not(target_os = "linux"))]
fn show_stream(_: std::net::SocketAddr, _: secure::Key, _: i32, _: Arc<AtomicBool>) -> Result<(), String> {
    Err("--show only works on the TV".into())
}

#[cfg(target_os = "linux")]
fn play(file: &std::path::Path, source: i32, fps: u32) -> Result<(), String> {
    tv::play(file, source, fps)
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
fn play(_: &std::path::Path, _: i32, _: u32) -> Result<(), String> {
    Err("play only works on the TV".into())
}
