//! displaywire-server: captures the desktop, encodes HEVC and streams it to one
//! client over UDP.
//!
//!   displaywire-server                       # Windows: stream the primary monitor
//!   displaywire-server --file clip.h265      # any OS: loop an Annex B file (testing)

mod audio;
mod auth;
mod clients;
mod file;
mod mic;
mod pairing;
mod rate;
mod transfer;
#[cfg(any(windows, feature = "ui-dev"))]
mod ui;
mod uiserver;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
mod macos;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::collections::{HashMap, VecDeque};
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
use windows::gamepad::Gamepads;
#[cfg(windows)]
use windows::input::Injector;
#[cfg(target_os = "macos")]
use macos::input::Injector;

/// Input is only injected on Windows and macOS.
#[cfg(not(any(windows, target_os = "macos")))]
struct Injector;

#[cfg(not(any(windows, target_os = "macos")))]
impl Injector {
    fn new() -> Injector {
        Injector
    }
    fn reset(&mut self) {}
    fn tick(&mut self, _now_ns: u64) {}
    fn apply(&mut self, _input: &proto::stream::Input, _now_ns: u64) {}
}

/// Virtual gamepads are only made on Windows.
#[cfg(not(windows))]
struct Gamepads {
    feedback: std::sync::mpsc::Receiver<Feedback>,
}

#[cfg(not(windows))]
struct Feedback {
    slot: u8,
    large: u8,
    small: u8,
}

#[cfg(not(windows))]
impl Gamepads {
    fn new() -> Gamepads {
        Gamepads { feedback: std::sync::mpsc::channel().1 }
    }
    fn reset(&mut self) {}
    fn tick(&mut self, _now_ns: u64) {}
    fn apply(&mut self, _pads: &proto::stream::Pads, _now_ns: u64) {}
}

#[derive(Parser, Clone)]
#[command(about = "Stream this machine's screen to a displaywire client")]
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
    /// Stream a moving test picture instead of the screen (Mac; needs no permissions).
    #[arg(long, hide = true)]
    test_pattern: bool,
    /// Stream an HEVC Annex B file (with AUDs) in a loop instead of the screen.
    #[arg(long)]
    file: Option<std::path::PathBuf>,
    /// Leave Windows Wi-Fi background scanning and power settings alone.
    #[arg(long)]
    no_wifi_tuning: bool,
    /// Repair packets per frame, as a percentage of its data packets (0 turns FEC off).
    #[arg(long, default_value_t = 10)]
    fec: u32,
    /// Where the paired devices are kept (default: next to the server's data, readable only by administrators).
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// List the paired devices and exit.
    #[arg(long)]
    list_clients: bool,
    /// Pair a new device from this console, over SSH for instance: asks here to allow it and shows the PIN.
    /// Needs the server running and administrator rights.
    #[arg(long)]
    pair: bool,
    /// Remove a paired device (by id or name), destroying its key, and exit. A running server drops it at once.
    #[arg(long, value_name = "ID_OR_NAME")]
    remove_client: Option<String>,
    /// Wait for Enter before exiting (for a window opened just to show some output).
    #[arg(long, hide = true)]
    pause: bool,
    /// Only answer clients (pairing, handshakes) and serve the management window: no video or audio. For development.
    #[arg(long, hide = true)]
    control_only: bool,
    /// The device-management window (started by the tray icon): the server's loopback port and token.
    #[arg(long, hide = true, num_args = 2, value_names = ["PORT", "TOKEN"])]
    ui: Option<Vec<String>>,
    /// Windows: no notification-area icon.
    #[arg(long)]
    no_tray: bool,
    /// Don't share the clipboard with the client.
    #[arg(long)]
    no_clipboard: bool,
    /// Don't accept files from the client.
    #[arg(long)]
    no_files: bool,
    /// Largest file accepted, in megabytes.
    #[arg(long, default_value_t = 4096)]
    max_upload_mb: u64,
    /// Don't answer discovery queries (clients then need the address typed in).
    #[arg(long)]
    no_discovery: bool,
    /// Don't send desktop audio.
    #[arg(long)]
    no_audio: bool,
    /// Don't play the client's microphone.
    #[arg(long)]
    no_mic: bool,
    /// Windows: the audio output the client's microphone is played into, by (part of) its name; `|` separates
    /// alternatives, tried in order. A virtual cable such as VB-Cable ("CABLE Input", whose other end,
    /// "CABLE Output", apps then record from) or Steam's virtual microphone.
    #[arg(long, default_value = "CABLE Input|Steam Streaming Microphone")]
    mic_device: String,
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
    /// Windows: install to Program Files, with the service, firewall rule and an entry in Installed apps
    /// (what double-clicking offers; needs administrator rights).
    #[arg(long)]
    setup: bool,
    /// Windows: look for a newer release and say whether there is one.
    #[arg(long)]
    check_update: bool,
    /// Windows: install the newest release if it is newer than this one (administrator rights needed).
    #[arg(long)]
    update: bool,
    /// Windows: remove everything `--setup` added (`--purge` also deletes the paired devices and logs).
    #[arg(long)]
    uninstall: bool,
    #[arg(long, requires = "uninstall")]
    purge: bool,
    /// Run the server in this window, without the installer prompt.
    #[arg(long)]
    run: bool,
    /// Run as the service (started by Windows; not for manual use).
    #[arg(long, hide = true)]
    service: bool,
}

/// Where the pointer is, for clients that draw it (`CAP_CURSOR`): 0..=65535 across the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerState {
    pub visible: bool,
    pub x: u16,
    pub y: u16,
    /// `PointerImage::id` of its current image.
    pub shape: u32,
}

/// The pointer's image: RGBA (straight alpha), `id` changing with it.
pub struct PointerImage {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub hot: (u32, u32),
    pub rgba: Vec<u8>,
}

pub struct EncodedFrame {
    /// HEVC Annex B access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub capture_ns: u64,
}

/// `--control-only`: never produces a frame.
struct NoSource;

impl Source for NoSource {
    fn next_frame(&mut self, _force_keyframe: bool, _timeout_ms: u32) -> Result<Option<EncodedFrame>, String> {
        Ok(None)
    }
}

pub trait Source {
    /// Waits up to about `timeout_ms` for the next frame. With `force_keyframe`,
    /// the next frame returned must be a keyframe (re-encoding the last image if
    /// nothing changed).
    fn next_frame(&mut self, force_keyframe: bool, timeout_ms: u32) -> Result<Option<EncodedFrame>, String>;
    /// Changes the video bitrate while running. Sources that can't just ignore it.
    fn set_bitrate(&mut self, _bps: u32) {}
    /// Whether the pointer is drawn into the video (off while the client draws it).
    fn set_pointer_in_video(&mut self, _on: bool) {}
    /// Stops the frames captured at `capture_ns` being used as references (the client lost them).
    /// False if that can't be done, and a keyframe is needed instead.
    fn invalidate(&mut self, _capture_ns: &[u64]) -> bool {
        false
    }
    /// Encodes the current image again even if nothing changed (not as a keyframe).
    fn repaint(&mut self) {}
    /// The monitors there are to stream, and which one is; None for sources that aren't a screen.
    fn monitors(&self) -> Option<proto::stream::Monitors> {
        None
    }
    fn choose_monitor(&mut self, _index: u8) -> Result<(), String> {
        Err("this source has no monitors".into())
    }
    /// The pointer, for a client that draws it; None from sources without one.
    fn pointer(&self) -> Option<PointerState> {
        None
    }
    fn pointer_image(&self) -> Option<PointerImage> {
        None
    }
}

struct Shared {
    /// The paired devices.
    clients: Mutex<clients::Store>,
    /// Pairing a new device with a PIN.
    pairing: pairing::Pairing,
    /// The authenticated client, if any.
    link: Mutex<Option<Link>>,
    last_seen_ns: AtomicU64,
    /// The video streams running: 0 always, others while the client shows more monitors.
    streams: Mutex<HashMap<u8, Arc<StreamCtl>>>,
    /// `CAP_*` flags the client asked for in its hello.
    caps: AtomicU8,
    /// The client's stream settings (see `proto::stream::Settings`); the command line's until it sends them.
    fec_percent: AtomicU8,
    fps_cap: AtomicU16,
    bitrate_kbps: AtomicU32,
    /// For the tray tooltip: what was sent over the last second.
    fps: AtomicU32,
    mbps_x10: AtomicU32,
    /// Device-management windows connected (see `uiserver`), and a request for the open one to come to the front.
    ui_open: AtomicUsize,
    ui_front: AtomicBool,
    /// How the window reaches the server, once `uiserver` is listening.
    ui_handle: std::sync::OnceLock<uiserver::Handle>,
}

/// What the control thread tells one stream's loop.
#[derive(Default)]
struct StreamCtl {
    keyframe: AtomicBool,
    /// Earliest frame the client reported lost and not yet dealt with (`Msg::FrameLost`).
    lost: Mutex<Option<u32>>,
    /// A monitor the client asked this stream to show (`Msg::ShowMonitor`), not yet switched to.
    monitor: Mutex<Option<u8>>,
    /// The client closed this stream's window, or went away.
    stop: AtomicBool,
}

impl Shared {
    fn stream(&self, id: u8) -> Option<Arc<StreamCtl>> {
        self.streams.lock().unwrap().get(&id).cloned()
    }

    /// Every stream starts again from a keyframe (a new session, a new hello).
    fn keyframe_all(&self) {
        for ctl in self.streams.lock().unwrap().values() {
            ctl.keyframe.store(true, Relaxed);
        }
    }

    /// Stops every stream but the main one.
    fn close_extra_streams(&self) {
        self.streams.lock().unwrap().retain(|&id, ctl| {
            if id != 0 {
                ctl.stop.store(true, Relaxed);
            }
            id == 0
        });
    }

    fn new(clients: clients::Store, pairing: pairing::Pairing) -> Shared {
        Shared {
            clients: Mutex::new(clients),
            pairing,
            link: Mutex::new(None),
            last_seen_ns: AtomicU64::new(0),
            streams: Mutex::new(HashMap::from([(0, Arc::new(StreamCtl::default()))])),
            caps: AtomicU8::new(0),
            fec_percent: AtomicU8::new(0),
            fps_cap: AtomicU16::new(0),
            bitrate_kbps: AtomicU32::new(0),
            fps: AtomicU32::new(0),
            mbps_x10: AtomicU32::new(0),
            ui_open: AtomicUsize::new(0),
            ui_front: AtomicBool::new(false),
            ui_handle: std::sync::OnceLock::new(),
        }
    }
}

fn main() {
    // Keep the console open on a crash too, so a double-clicked exe shows why it stopped.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        wait_for_enter();
    }));
    // Double-clicked (or typed with no options): offer to install instead of just running.
    #[cfg(windows)]
    if std::env::args_os().len() == 1 && !windows::setup::interactive() {
        return;
    }
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

/// `--pair`: the running server's window, driven from the console.
fn pair_command(cli: &Cli, dir: &std::path::Path) -> Option<Result<(), String>> {
    if !cli.pair {
        return None;
    }
    Some((|| {
        let mut ui = uiserver::Client::connect_from_file(dir)?;
        ui.request("pair-start")?;
        println!("Listening for a new device for two minutes. On the other computer, open DisplayWire and choose this PC.");
        let (mut asked, mut showed_pin, mut seen_active) = (false, false, false);
        let deadline = std::time::Instant::now() + Duration::from_secs(5 * 60);
        while std::time::Instant::now() < deadline {
            let lines = ui.request("state")?;
            let field = |l: &str, i: usize| l.split('\t').nth(i).unwrap_or("").to_string();
            let pairing = lines.iter().find(|l| l.starts_with("pairing")).cloned().unwrap_or_default();
            match pairing.split('\t').next().unwrap_or("").trim_start_matches("pairing ") {
                "asking" if !asked => {
                    asked = true;
                    seen_active = true;
                    print!("\"{}\" ({}) wants to pair. Allow it? [y/N] ", field(&pairing, 2), field(&pairing, 3));
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    let mut answer = String::new();
                    let _ = std::io::stdin().read_line(&mut answer);
                    ui.request(if answer.trim().eq_ignore_ascii_case("y") { "pair-allow" } else { "pair-deny" })?;
                }
                "pin" if !showed_pin => {
                    showed_pin = true;
                    seen_active = true;
                    let pin = field(&pairing, 4);
                    println!("Type this PIN on {}: {} {}", field(&pairing, 2), &pin[..pin.len().min(3)], pin.get(3..).unwrap_or(""));
                }
                "off" if seen_active => {
                    match lines.iter().find_map(|l| l.strip_prefix("result ")) {
                        Some(r) => println!("{}", r.replace('\t', ": ")),
                        None => println!("Pairing ended."),
                    }
                    return Ok(());
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        let _ = ui.request("pair-stop");
        Err("timed out".into())
    })())
}

/// `--list-clients` and `--remove-client`.
fn client_command(cli: &Cli, dir: &std::path::Path) -> Option<Result<(), String>> {
    if !cli.list_clients && cli.remove_client.is_none() {
        return None;
    }
    Some((|| {
        let mut store = clients::Store::open(dir)?;
        if let Some(who) = &cli.remove_client {
            match store.remove(who)? {
                Some(c) => println!("removed \"{}\" ({}); its key no longer works", c.name, c.id),
                None => return Err(format!("no paired device with the id or name {who:?} (see --list-clients)")),
            }
        } else if store.list().is_empty() {
            println!("no devices are paired yet: choose Pair new device in the tray menu");
        } else {
            println!("{:<10} {:<28} {:<16} paired", "id", "name", "last seen from");
            for c in store.list() {
                let seen = if c.last_seen == 0 { "never".to_string() } else { c.last_addr.clone() };
                println!("{:<10} {:<28} {:<16} {}", c.id, c.name, seen, clients::format_date(c.created));
            }
        }
        if cli.pause {
            wait_for_enter();
        }
        Ok(())
    })())
}

#[cfg(windows)]
fn service_command(cli: &Cli, data_dir: &std::path::Path) -> Option<Result<(), String>> {
    use windows::{service, setup, updater};
    if cli.check_update || cli.update {
        Some(updater::command(cli.update))
    } else if cli.setup {
        let r = setup::setup();
        if cli.pause {
            if let Err(e) = &r {
                eprintln!("error: {e}");
            }
            wait_for_enter();
        }
        Some(r)
    } else if cli.uninstall {
        let r = setup::uninstall(cli.purge);
        if cli.pause {
            wait_for_enter();
        }
        Some(r)
    } else if cli.install_service {
        if let Err(e) = clients::Store::open(data_dir) {
            return Some(Err(e));
        }
        println!("Pair devices from the tray icon: Pair new device.");
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
fn service_command(cli: &Cli, _data_dir: &std::path::Path) -> Option<Result<(), String>> {
    (cli.install_service || cli.uninstall_service || cli.service || cli.check_update || cli.update).then(|| Err("the service and updates are Windows only".into()))
}

fn run(cli: Cli) -> Result<(), String> {
    if let Some(a) = &cli.ui {
        let port: u16 = a[0].parse().map_err(|_| "bad --ui port")?;
        #[cfg(any(windows, feature = "ui-dev"))]
        {
            // No console to read errors from: keep them in a file the user can find.
            let log = std::env::temp_dir().join("displaywire-ui.log");
            let hook_log = log.clone();
            std::panic::set_hook(Box::new(move |info| {
                let _ = std::fs::write(&hook_log, format!("the window crashed: {info}\n"));
            }));
            let result = ui::run(port, a[1].clone(), false);
            if let Err(e) = &result {
                let _ = std::fs::write(&log, format!("the window could not start: {e}\n"));
            }
            return result;
        }
        #[cfg(not(any(windows, feature = "ui-dev")))]
        return Err(format!("the management window is only built for Windows (port {port})"));
    }
    let data_dir = cli.data_dir.clone().unwrap_or_else(auth::default_dir);
    if let Some(result) = client_command(&cli, &data_dir) {
        return result;
    }
    if let Some(result) = pair_command(&cli, &data_dir) {
        return result;
    }
    if let Some(result) = service_command(&cli, &data_dir) {
        return result;
    }
    let store = clients::Store::open(&data_dir)?;
    match store.list().len() {
        0 => println!("no devices are paired yet: choose Pair new device in the tray menu"),
        n => println!("{n} paired device{}", if n == 1 { "" } else { "s" }),
    }
    let server_name = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "displaywire-server".into());
    let _tuning = (!cli.no_wifi_tuning).then(wlan::tune).flatten();
    let source: Box<dyn Source> = match &cli.file {
        _ if cli.control_only => Box::new(NoSource),
        Some(path) => Box::new(file::FileSource::open(path, cli.fps)?),
        #[cfg(target_os = "macos")]
        None if cli.test_pattern => macos::open_pattern(&cli)?,
        None => open_screen(&cli, 0, None)?,
    };

    let sock = udp_socket(SocketAddr::from(([0, 0, 0, 0], cli.port))).map_err(|e| format!("bind udp/{}: {e}", cli.port))?;
    let shared = Arc::new(Shared::new(store, pairing::Pairing::new(&server_name)));
    shared.fec_percent.store(cli.fec.min(proto::stream::Settings::MAX_FEC_PERCENT as u32) as u8, Relaxed);
    shared.bitrate_kbps.store((cli.mbps * 1000.0) as u32, Relaxed);
    match uiserver::start(shared.clone(), &data_dir) {
        Ok(handle) => {
            let _ = shared.ui_handle.set(handle);
        }
        Err(e) => eprintln!("device window unavailable: {e}"),
    }
    {
        let sock = sock.try_clone().map_err(|e| e.to_string())?;
        let shared = shared.clone();
        let announcement = (!cli.no_discovery).then(|| proto::discover::Announcement {
            port: cli.port,
            version: VERSION,
            address: None,
            name: server_name.clone(),
        });
        let transfers = transfer::Transfers::new(transfer::Options {
            clipboard: !cli.no_clipboard,
            files: !cli.no_files,
            max_file_bytes: cli.max_upload_mb << 20,
        });
        transfers.spawn(sock.try_clone().map_err(|e| e.to_string())?, shared.clone());
        let mic = open_mic(&cli);
        #[cfg(windows)]
        if !cli.no_tray && cli.file.is_none() {
            windows::tray::start(shared.clone());
            windows::updater::start();
        }
        let opener = stream_opener(&cli, &shared, &sock)?;
        std::thread::spawn(move || control(&sock, &shared, announcement.as_ref(), &transfers, mic, &opener));
    }
    if cli.control_only {
        match shared.ui_handle.get() {
            Some(h) => println!("control only: window is  displaywire-server --ui {} {}", h.port, h.token),
            None => println!("control only"),
        }
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
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
    println!("displaywire-server on udp/{}, waiting for a client", cli.port);
    let main = shared.stream(0).ok_or("no main stream")?;
    stream_loop(0, source, main, &shared, &sock, 8e3 / cli.pace_mbps, None)
}

/// Starts stream `id` showing `monitor` for the session at the given address, in a thread of its own
/// (capture has to happen on the thread that opened it).
type StreamOpener = Box<dyn Fn(u8, u8, SocketAddr) + Send>;

fn stream_opener(cli: &Cli, shared: &Arc<Shared>, sock: &UdpSocket) -> Result<StreamOpener, String> {
    let (cli, shared, sock) = (Arc::new(cli.clone()), shared.clone(), sock.try_clone().map_err(|e| e.to_string())?);
    Ok(Box::new(move |id, monitor, owner| {
        let ctl = Arc::new(StreamCtl::default());
        shared.streams.lock().unwrap().insert(id, ctl.clone());
        let (cli, shared, sock) = (cli.clone(), shared.clone(), sock.try_clone());
        let spawned = std::thread::Builder::new().name(format!("stream-{id}")).spawn(move || {
            let result = sock.map_err(|e| e.to_string()).and_then(|sock| {
                let source = open_screen(&cli, id, Some(monitor))?;
                println!("stream {id}: showing monitor {monitor}");
                stream_loop(id, source, ctl.clone(), &shared, &sock, 8e3 / cli.pace_mbps, Some(owner))
            });
            match result {
                Ok(()) => println!("stream {id}: closed"),
                Err(e) => println!("stream {id}: {e}"),
            }
            let mut streams = shared.streams.lock().unwrap();
            if streams.get(&id).is_some_and(|c| Arc::ptr_eq(c, &ctl)) {
                streams.remove(&id);
            }
        });
        if let Err(e) = spawned {
            println!("stream {id}: {e}");
        }
    }))
}

/// One video stream: captures, encodes and sends frames of `source` to the client, following
/// what the control thread asks (`ctl`). The main stream (0) runs for good; another stops with
/// `ctl.stop` or when the session `owner` that opened it ends.
fn stream_loop(
    id: u8,
    mut source: Box<dyn Source>,
    ctl: Arc<StreamCtl>,
    shared: &Shared,
    sock: &UdpSocket,
    pace_ns_per_byte: f64,
    owner: Option<SocketAddr>,
) -> Result<(), String> {
    let mut frame_id = 0u32;
    let mut stats = Stats::new();
    let mut sealed = [0u8; proto::MAX_DATAGRAM];
    let mut applied_kbps = 0;
    let mut pointer = PointerSender::new(id);
    // The monitor list last sent, to whom and when, and when it was last looked at.
    let mut monitors_sent: Option<(SocketAddr, proto::stream::Monitors, u64)> = None;
    let mut monitors_checked = 0u64;
    // Recent frames (id, capture time), to find what the encoder knows a lost frame by; and the newest
    // frame invalidated, to announce with the next one (`Msg::Recovered`).
    let mut recent: VecDeque<(u32, u64)> = VecDeque::new();
    let mut recovered_upto: Option<u32> = None;
    let mut last_frame_ns = 0u64;
    loop {
        let link = shared.link.lock().unwrap().clone();
        // A stream other than the main one belongs to the session that opened it.
        if ctl.stop.load(Relaxed) || owner.is_some_and(|o| link.as_ref().is_none_or(|l| l.addr != o)) {
            return Ok(());
        }
        let Some(link) = link else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        if id == 0 && now_ns().saturating_sub(shared.last_seen_ns.load(Relaxed)) > CLIENT_TIMEOUT_NS {
            println!("{}: timed out", link.addr);
            shared.link.lock().unwrap().take_if(|l| l.addr == link.addr);
            continue;
        }

        // Rate control moves in small steps; the encoder is told about real changes only. Open
        // streams share the bitrate.
        let streams = shared.streams.lock().unwrap().len().max(1) as u32;
        let kbps = shared.bitrate_kbps.load(Relaxed) / streams;
        if kbps.abs_diff(applied_kbps) * 50 >= applied_kbps.max(1) {
            applied_kbps = kbps;
            source.set_bitrate(kbps * 1000);
        }
        if let Some(index) = ctl.monitor.lock().unwrap().take() {
            match source.choose_monitor(index) {
                Ok(()) => monitors_checked = 0,
                Err(e) => println!("stream {id}: monitor {index}: {e}"),
            }
        }
        // The monitors on connecting, whenever they change, and every 10 s in case one was lost (the
        // main stream says, listing which monitor it shows).
        let now = now_ns();
        if id == 0 && monitors_sent.as_ref().is_none_or(|(to, ..)| *to != link.addr) || now - monitors_checked > 2 * SEC {
            monitors_checked = now;
            if let Some(m) = source.monitors()
                && monitors_sent.as_ref().is_none_or(|&(to, last, at)| to != link.addr || last != m || now - at > 10 * SEC)
            {
                let mut plain = [0u8; proto::MAX_PLAIN];
                let n = Msg::Monitors(m).encode(&mut plain);
                link.send(sock, &plain[..n], &mut sealed);
                monitors_sent = Some((link.addr, m, now));
            }
        }
        let client_cursor = shared.caps.load(Relaxed) & proto::stream::CAP_CURSOR != 0;
        source.set_pointer_in_video(!client_cursor);
        if client_cursor {
            pointer.send(&*source, &link, sock, &mut sealed);
        }
        // The client lost frames: it drops everything from the first until told it can carry on, so
        // everything from there to the newest frame stops being a reference. Too far back, or an
        // encoder that can't, and a keyframe does it.
        if let Some(first) = ctl.lost.lock().unwrap().take() {
            let since: Vec<u64> = recent.iter().filter(|&&(id, _)| id.wrapping_sub(first) < 1 << 31).map(|&(_, c)| c).collect();
            let known = recent.front().is_some_and(|&(oldest, _)| first.wrapping_sub(oldest) < 1 << 31);
            if known && !since.is_empty() && source.invalidate(&since) {
                recovered_upto = recent.back().map(|&(id, _)| id);
                stats.recovered += 1;
                // The client is waiting for the frame that carries on; on a still screen there
                // may not be one for a while.
                source.repaint();
            } else {
                ctl.keyframe.store(true, Relaxed);
                stats.fell_back += 1;
            }
        }
        let force = ctl.keyframe.swap(false, Relaxed);
        // Frame-rate cap: wait before taking the next frame (which is then the latest
        // image), so skipped frames cost no encoding.
        let cap = shared.fps_cap.load(Relaxed);
        if cap > 0 && !force {
            sleep_until(last_frame_ns + SEC / cap as u64);
        }
        let Some(frame) = source.next_frame(force, 100)? else {
            if force {
                ctl.keyframe.store(true, Relaxed);
            }
            continue;
        };
        if force && !frame.keyframe {
            ctl.keyframe.store(true, Relaxed);
        }

        if frame.keyframe {
            recovered_upto = None;
        } else if let Some(upto) = recovered_upto.take() {
            let mut plain = [0u8; 16];
            let n = Msg::Recovered { stream: id, from: frame_id, lost_upto: upto }.encode(&mut plain);
            // Twice: it's small, and losing it costs a keyframe.
            for _ in 0..2 {
                link.send(sock, &plain[..n], &mut sealed);
            }
        }
        if recent.len() == 32 {
            recent.pop_front();
        }
        recent.push_back((frame_id, frame.capture_ns));

        let encoded_ns = now_ns();
        let mut next_send = encoded_ns;
        last_frame_ns = now_ns();
        video::packetize(id, frame_id, frame.keyframe, frame.capture_ns, &frame.data, shared.fec_percent.load(Relaxed) as u32, |pkt| {
            sleep_until(next_send);
            let wire = link.send(sock, pkt, &mut sealed);
            next_send = next_send.max(now_ns()) + (wire as f64 * pace_ns_per_byte) as u64;
        });
        frame_id = frame_id.wrapping_add(1);
        stats.frame(id, &frame, encoded_ns, now_ns(), shared);
    }
}

/// The rate controller (`rate`) and what it last gave the stream loop through `Shared`.
struct Rate {
    ctl: rate::RateControl,
    /// The user's settings: ceiling kbit/s, base repair %, adaptive.
    settings: (u32, u8, bool),
    published: (u32, u8),
    logged_ns: u64,
}

impl Rate {
    fn new(shared: &Shared) -> Rate {
        let (kbps, fec) = (shared.bitrate_kbps.load(Relaxed), shared.fec_percent.load(Relaxed));
        Rate { ctl: rate::RateControl::new(kbps, fec, true), settings: (kbps, fec, true), published: (kbps, fec), logged_ns: 0 }
    }

    fn settings(&self) -> (u32, u8, bool) {
        self.settings
    }

    fn set(&mut self, kbps: u32, fec: u8, adaptive: bool) {
        self.settings = (kbps, fec, adaptive);
        self.ctl.set(kbps, fec, adaptive);
    }

    fn reset(&mut self) {
        self.ctl.reset();
    }

    fn on_feedback(&mut self, now_ns: u64, f: &proto::stream::Feedback) {
        self.ctl.on_feedback(now_ns, f);
    }

    /// Hands the current bitrate and repair level to the stream loop, and logs real moves.
    fn publish(&mut self, shared: &Shared) {
        let (kbps, fec) = self.ctl.current();
        shared.bitrate_kbps.store(kbps, Relaxed);
        shared.fec_percent.store(fec, Relaxed);
        let (was_kbps, was_fec) = self.published;
        let now = now_ns();
        if (kbps.abs_diff(was_kbps) * 20 > was_kbps || fec != was_fec) && now - self.logged_ns > SEC {
            let why = match self.ctl.last_cut.take() {
                Some((queue_us, loss)) if kbps < was_kbps => format!(" (queue {} ms, loss {:.1}%)", queue_us / 1000, loss * 100.0),
                _ => String::new(),
            };
            println!("rate: {:.1} Mbps, loss protection {fec}%{why}", kbps as f64 / 1000.0);
            (self.published, self.logged_ns) = ((kbps, fec), now);
        }
    }
}

/// Sends the pointer to a client that draws it: its state on every change and every second, its
/// image when it changes and every two seconds (so a lost piece mends itself).
#[derive(Default)]
struct PointerSender {
    stream: u8,
    to: Option<SocketAddr>,
    state: Option<PointerState>,
    state_ns: u64,
    image: Option<u32>,
    image_ns: u64,
}

impl PointerSender {
    fn new(stream: u8) -> PointerSender {
        PointerSender { stream, ..Default::default() }
    }

    fn send(&mut self, source: &dyn Source, link: &auth::Link, sock: &UdpSocket, sealed: &mut [u8]) {
        if self.to != Some(link.addr) {
            *self = PointerSender { stream: self.stream, to: Some(link.addr), ..Default::default() };
        }
        let Some(p) = source.pointer() else { return };
        let now = now_ns();
        let mut plain = [0u8; proto::MAX_PLAIN];
        if self.image != Some(p.shape) || now - self.image_ns > 2 * SEC {
            if let Some(img) = source.pointer_image().filter(|i| i.id == p.shape) {
                let max = proto::stream::MAX_CURSOR_SIZE as u32;
                if img.width <= max && img.height <= max {
                    let shape = proto::stream::CursorShape {
                        id: img.id,
                        width: img.width as u16,
                        height: img.height as u16,
                        hot_x: img.hot.0 as u16,
                        hot_y: img.hot.1 as u16,
                        offset: 0,
                    };
                    for (i, chunk) in img.rgba.chunks(proto::stream::CURSOR_PAYLOAD).enumerate() {
                        let shape = proto::stream::CursorShape { offset: (i * proto::stream::CURSOR_PAYLOAD) as u32, ..shape };
                        let n = Msg::CursorShape(shape, chunk).encode(&mut plain);
                        link.send(sock, &plain[..n], sealed);
                    }
                }
            }
            (self.image, self.image_ns) = (Some(p.shape), now);
        }
        if self.state != Some(p) || now - self.state_ns > SEC {
            let n = Msg::Cursor { stream: self.stream, visible: p.visible, x: p.x, y: p.y, shape: p.shape }.encode(&mut plain);
            link.send(sock, &plain[..n], sealed);
            (self.state, self.state_ns) = (Some(p), now);
        }
    }
}

#[cfg(windows)]
fn open_screen(cli: &Cli, stream: u8, monitor: Option<u8>) -> Result<Box<dyn Source>, String> {
    Ok(Box::new(windows::ScreenSource::open(cli, stream, monitor)?))
}

/// Starts playing the client's microphone, if there's somewhere to play it.
#[cfg(windows)]
fn open_mic(cli: &Cli) -> Option<mic::Sink> {
    if cli.no_mic || cli.file.is_some() {
        return None;
    }
    let (sink, consumer) = mic::channel();
    match windows::micout::start(&cli.mic_device, consumer) {
        Ok(()) => Some(sink),
        Err(e) => {
            eprintln!("microphone passthrough unavailable: {e}");
            None
        }
    }
}

#[cfg(not(windows))]
fn open_mic(_cli: &Cli) -> Option<mic::Sink> {
    None
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

#[cfg(target_os = "macos")]
fn open_screen(cli: &Cli, stream: u8, monitor: Option<u8>) -> Result<Box<dyn Source>, String> {
    Ok(Box::new(macos::ScreenSource::open(cli, stream, monitor)?))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn open_screen(_cli: &Cli, _stream: u8, _monitor: Option<u8>) -> Result<Box<dyn Source>, String> {
    Err("screen capture is only implemented on Windows; use --file".into())
}

/// A token bucket: `per_sec` on average, up to `burst` at once.
struct HandshakeLimiter {
    per_sec: f64,
    burst: f64,
    tokens: f64,
    last_ns: u64,
}

impl HandshakeLimiter {
    fn new(per_sec: f64, burst: f64) -> HandshakeLimiter {
        HandshakeLimiter { per_sec, burst, tokens: burst, last_ns: now_ns() }
    }

    fn allow(&mut self, now: u64) -> bool {
        self.tokens = (self.tokens + (now - self.last_ns) as f64 / SEC as f64 * self.per_sec).min(self.burst);
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
    /// The paired device whose key opened the handshake.
    client: Arc<str>,
}

/// Pending handshakes kept (in all, and from one address), and for how long.
const MAX_PENDING: usize = 16;
const MAX_PENDING_PER_SOURCE: usize = 2;
const PENDING_TTL_NS: u64 = 10 * SEC;

/// Handshakes that verify, per source address: a device reconnecting needs one or two, and one
/// address can't keep replacing everyone else's pending sessions. Forgeries never get this far.
struct PerSource(std::collections::HashMap<std::net::IpAddr, HandshakeLimiter>);

impl PerSource {
    fn allow(&mut self, ip: std::net::IpAddr, now: u64) -> bool {
        if self.0.len() > 256 {
            self.0.retain(|_, l| now - l.last_ns < 60 * SEC);
        }
        self.0.entry(ip).or_insert_with(|| HandshakeLimiter::new(2.0, 5.0)).allow(now)
    }
}

/// Addresses on this network (or this machine) that may discover the server: private, link-local
/// and loopback. Discovery is a local broadcast; anything else asking is probing from outside.
fn is_local(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// This computer's IPv4 address on the network that reaches `peer`: what the peer should connect to.
fn local_ip_toward(peer: SocketAddr) -> Option<[u8; 4]> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(peer).ok()?;
    match probe.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip.octets()),
        _ => None,
    }
}

/// Handles client datagrams: handshakes, then (encrypted) session start/stop,
/// pings, keyframe requests and input. Anything else is dropped silently.
fn control(
    sock: &UdpSocket,
    shared: &Shared,
    announcement: Option<&proto::discover::Announcement>,
    transfers: &transfer::Transfers,
    mut mic: Option<mic::Sink>,
    open_stream: &StreamOpener,
) {
    let mut buf = [0u8; 2048];
    let mut plain = [0u8; 2048];
    let mut out = [0u8; proto::MAX_DATAGRAM];
    let mut injector = Injector::new();
    let mut gamepads = Gamepads::new();
    let mut rate = Rate::new(shared);
    let mut active: Option<(Link, ReplayWindow)> = None;
    let mut pending: Vec<Pending> = Vec::new();
    // Checking a forged handshake is hashing only; this bounds what a flood of them costs.
    let mut verify_limiter = HandshakeLimiter::new(2000.0, 4000.0);
    let mut per_source = PerSource(Default::default());
    let mut pair_limiter = HandshakeLimiter::new(20.0, 40.0);
    let mut last_check = 0u64;
    let mut last_reject_log = 0u64;
    let mut discovery_limiter = HandshakeLimiter::new(20.0, 40.0);
    // Wake up regularly so held keys are released if the client goes quiet.
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    loop {
        let now = now_ns();
        injector.tick(now);
        gamepads.tick(now);
        // What games asked of the pads goes back to the active client.
        while let Ok(f) = gamepads.feedback.try_recv() {
            if let Some((link, _)) = &active {
                let mut plain = [0u8; 16];
                let m = Msg::Rumble { slot: f.slot, large: f.large, small: f.small }.encode(&mut plain);
                link.send(sock, &plain[..m], &mut out);
            }
        }
        if shared.link.lock().unwrap().is_none() {
            active = None;
        }
        pending.retain(|p| now - p.created_ns < PENDING_TTL_NS);
        shared.pairing.tick(now);
        if now - last_check > 200_000_000 {
            last_check = now;
            // Devices removed (here, in the UI or with --remove-client) lose their sessions at once.
            let mut store = shared.clients.lock().unwrap();
            store.reload_if_changed();
            pending.retain(|p| store.get(&p.client).is_some());
            if active.as_ref().is_some_and(|(l, _)| store.get(&l.client).is_none()) {
                println!("{}: its device was removed, disconnected", active.as_ref().map_or_else(String::new, |(l, _)| l.addr.to_string()));
                *shared.link.lock().unwrap() = None;
                active = None;
                injector.reset();
                gamepads.reset();
            }
        }
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if is_transient(&e) => continue,
            Err(e) => {
                eprintln!("control socket: {e}");
                return;
            }
        };
        let datagram = &buf[..n];
        if let Some(nonce) = proto::discover::parse_query(datagram) {
            if let Some(a) = announcement.filter(|_| is_local(from.ip()) && discovery_limiter.allow(now)) {
                let mut a = a.clone();
                a.address = local_ip_toward(from);
                let m = proto::discover::reply(nonce, &a, &mut out);
                let _ = sock.send_to(&out[..m], from);
            }
            continue;
        }
        match secure::kind(datagram) {
            Some(secure::HS1) => {
                if !verify_limiter.allow(now) {
                    continue;
                }
                // Whichever device's key opens it is who is connecting.
                let keys: Vec<(Arc<str>, secure::Key)> =
                    shared.clients.lock().unwrap().list().iter().map(|c| (Arc::from(c.id.as_str()), c.key.clone())).collect();
                let accepted = keys.iter().find_map(|(id, key)| secure::accept(key, datagram, auth::unix_ms(), VERSION).ok().map(|r| (id.clone(), r)));
                match accepted {
                    Some((client, (hs2, session, hello))) => {
                        if !per_source.allow(from.ip(), now) {
                            continue;
                        }
                        let _ = sock.send_to(&hs2, from);
                        if hello.version != VERSION {
                            eprintln!("{from}: client speaks protocol {}, this server {VERSION}; update one of them", hello.version);
                            continue;
                        }
                        if let Some(i) = pending.iter().position(|p| p.addr.ip() == from.ip())
                            && pending.iter().filter(|p| p.addr.ip() == from.ip()).count() >= MAX_PENDING_PER_SOURCE
                        {
                            pending.remove(i);
                        } else if pending.len() == MAX_PENDING {
                            pending.remove(0);
                        }
                        pending.push(Pending { addr: from, session: Arc::new(session), window: ReplayWindow::default(), caps: hello.caps, created_ns: now, client });
                    }
                    None => {
                        if now - last_reject_log > 10 * SEC {
                            last_reject_log = now;
                            eprintln!("{from}: handshake rejected (not a paired device, or clocks more than 5 minutes apart)");
                        }
                    }
                }
            }
            Some(k) if secure::pair::is_pairing(k) => {
                if pair_limiter.allow(now) {
                    let reply = shared.pairing.handle(now, from, datagram, &mut shared.clients.lock().unwrap());
                    if let Some(reply) = reply {
                        let _ = sock.send_to(&reply, from);
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
                    let link = Link { addr: from, session: p.session, client: p.client.clone() };
                    {
                        let mut store = shared.clients.lock().unwrap();
                        let name = store.get(&p.client).map_or_else(String::new, |c| c.name.clone());
                        println!("{from}: connected ({name})");
                        store.touch(&p.client, from);
                    }
                    *shared.link.lock().unwrap() = Some(link.clone());
                    shared.caps.store(p.caps, Relaxed);
                    shared.last_seen_ns.store(now, Relaxed);
                    // A new session starts with the main stream only; the client reopens the rest.
                    shared.close_extra_streams();
                    shared.keyframe_all();
                    injector.reset();
                    gamepads.reset();
                    rate.reset();
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
                        shared.keyframe_all();
                    }
                    Some(Msg::Ping { id, client_ns }) => {
                        shared.last_seen_ns.store(now_ns(), Relaxed);
                        let mut pong = [0u8; 64];
                        let m = Msg::Pong { id, client_ns, server_ns: now_ns() }.encode(&mut pong);
                        if let Some((link, _)) = &active {
                            link.send(sock, &pong[..m], &mut out);
                        }
                    }
                    Some(Msg::Settings(s)) => {
                        let s = s.clamped();
                        let changed = shared.fps_cap.swap(s.fps_cap, Relaxed) != s.fps_cap || rate.settings() != (s.bitrate_kbps, s.fec_percent, s.adaptive);
                        rate.set(s.bitrate_kbps, s.fec_percent, s.adaptive);
                        rate.publish(shared);
                        if changed {
                            println!(
                                "{from}: settings {} Mbps{}, fps cap {}, loss protection {}%",
                                s.bitrate_kbps / 1000,
                                if s.adaptive { " (most)" } else { " (fixed)" },
                                s.fps_cap,
                                s.fec_percent
                            );
                        }
                    }
                    // The main stream's reports steer the shared bitrate.
                    Some(Msg::Feedback(f)) if f.stream == 0 => {
                        rate.on_feedback(now_ns(), &f);
                        rate.publish(shared);
                    }
                    Some(msg @ (Msg::Xfer { .. } | Msg::XferAck { .. } | Msg::XferCancel { .. })) => {
                        if let Some((link, _)) = &active {
                            let mut reply = [0u8; proto::MAX_DATAGRAM];
                            for r in transfers.handle(now_ns(), &msg) {
                                let m = transport::transfer::encode(&r, &mut reply);
                                let mut sealed = [0u8; proto::MAX_DATAGRAM];
                                link.send(sock, &reply[..m], &mut sealed);
                            }
                        }
                    }
                    Some(Msg::KeyframeRequest { stream }) => {
                        if let Some(ctl) = shared.stream(stream) {
                            ctl.keyframe.store(true, Relaxed);
                        }
                    }
                    Some(Msg::ShowMonitor { stream, monitor }) => match shared.stream(stream) {
                        Some(ctl) => *ctl.monitor.lock().unwrap() = Some(monitor),
                        None => open_stream(stream, monitor, from),
                    },
                    Some(Msg::CloseStream { stream }) if stream != 0 => {
                        if let Some(ctl) = shared.streams.lock().unwrap().remove(&stream) {
                            ctl.stop.store(true, Relaxed);
                        }
                    }
                    Some(Msg::FrameLost { stream, first, .. }) => {
                        if let Some(ctl) = shared.stream(stream) {
                            let mut lost = ctl.lost.lock().unwrap();
                            *lost = Some(lost.map_or(first, |l| l.min(first)));
                        }
                    }
                    Some(Msg::Input(input)) => injector.apply(&input, now_ns()),
                    Some(Msg::Pads(pads)) => gamepads.apply(&pads, now_ns()),
                    Some(Msg::Mic(a, payload)) => {
                        shared.last_seen_ns.store(now_ns(), Relaxed);
                        if let Some(m) = &mut mic {
                            m.push(a.seq, payload);
                        }
                    }
                    Some(Msg::Bye) => {
                        println!("{from}: disconnected");
                        *shared.link.lock().unwrap() = None;
                        active = None;
                        injector.reset();
                        gamepads.reset();
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
    /// Losses repaired by invalidating references, and ones that needed a keyframe.
    recovered: u32,
    fell_back: u32,
    bytes: u64,
    encode_ns: u64,
    send_ns: u64,
}

impl Stats {
    fn new() -> Self {
        Stats { window_start: now_ns(), frames: 0, keyframes: 0, recovered: 0, fell_back: 0, bytes: 0, encode_ns: 0, send_ns: 0 }
    }

    fn frame(&mut self, stream: u8, f: &EncodedFrame, encoded_ns: u64, sent_ns: u64, shared: &Shared) {
        self.frames += 1;
        self.keyframes += f.keyframe as u32;
        self.bytes += f.data.len() as u64;
        self.encode_ns += encoded_ns.saturating_sub(f.capture_ns);
        self.send_ns += sent_ns - encoded_ns;
        let elapsed = sent_ns - self.window_start;
        if elapsed >= SEC {
            if stream == 0 {
                shared.fps.store(self.frames, Relaxed);
                shared.mbps_x10.store((self.bytes as f64 * 8.0 / elapsed as f64 * 1e4) as u32, Relaxed);
            }
            let n = self.frames.max(1) as f64;
            let losses = if self.recovered + self.fell_back > 0 {
                format!("  losses repaired {} / by keyframe {}", self.recovered, self.fell_back)
            } else {
                String::new()
            };
            let which = if stream == 0 { String::new() } else { format!("[stream {stream}] ") };
            println!(
                "{which}{:>4} fps  {:>6.1} Mbps  capture→encoded {:.2} ms  send {:.2} ms  keyframes {}{losses}",
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
