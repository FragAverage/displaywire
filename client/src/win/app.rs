//! The stream window on Windows: one connection, its window, and a window more for every other
//! monitor of the remote PC that is opened. The flow follows the Mac client's (`main.rs`), with
//! Windows' own video output, menu and pointer capture.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;
use timing::history::{Bin, History};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorGrabMode, CustomCursor, Fullscreen, Window, WindowId};

use super::menu::{self, Menu};
use super::video::{Layer, OverlayHandle};
use super::{dialog, gamepad, hook, launcher};
use crate::link::Link;
use crate::{input, keys, receiver, servers, settings, transfer};

/// The stream ended because the PC never answered: the launcher offers to pair again.
pub const EXIT_NO_REPLY: i32 = 3;

#[derive(Parser)]
#[command(about = "View a displaywire server's screen")]
pub struct Cli {
    /// Server address, `host` or `host:port`. Without one, the list of PCs opens.
    server: Option<String>,
    /// Pairing key for this server (saved for next time). Only needed once.
    #[arg(long)]
    key: Option<String>,
    /// Pair with a PC from the terminal: asks for the PIN the PC shows, then saves the key.
    #[arg(long, value_name = "SERVER")]
    pair: Option<String>,
    /// Save a pairing key without connecting: `--set-key <server> <key>`.
    #[arg(long, num_args = 2, value_names = ["SERVER", "KEY"])]
    set_key: Option<Vec<String>>,
    /// Send these files to the PC once connected.
    #[arg(long, value_name = "FILE")]
    send: Vec<std::path::PathBuf>,
    /// Open every monitor of the PC, each in a window of its own.
    #[arg(long)]
    all_monitors: bool,
    /// Start in full screen.
    #[arg(long)]
    fullscreen: bool,
    /// Watch only: no keyboard, mouse or gamepad input goes to the PC.
    #[arg(long)]
    view_only: bool,
    /// Started by the list of PCs: errors go back to it instead of into a message box.
    #[arg(long, hide = true)]
    from_launcher: bool,
    /// Only the settings window.
    #[arg(long, hide = true)]
    settings: bool,
}

pub fn main() {
    // A terminal this was started from gets the output.
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let cli = Cli::parse();
    timing::now_ns();
    let from_launcher = cli.from_launcher;
    let result = if cli.settings {
        launcher::run_settings()
    } else if let Some(server) = &cli.pair {
        pair_from_terminal(server)
    } else if let Some(args) = &cli.set_key {
        secure::Key::parse(&args[1]).map_err(|e| e.to_string()).and_then(|key| keys::save(&args[0], &key)).map(|()| println!("saved the pairing key for {}", args[0]))
    } else if cli.server.is_none() {
        launcher::run()
    } else {
        run(cli)
    };
    if let Err(e) = result {
        // Written for the Mac client first.
        let e = e.replace("this Mac", "this computer");
        eprintln!("error: {e}");
        if !from_launcher {
            dialog::show_message("DisplayWire", &e);
        }
        std::process::exit(if e.starts_with("no reply") { EXIT_NO_REPLY } else { 1 });
    }
}

/// Sent from the receiver thread to the UI thread.
pub enum UserEvent {
    /// Stream `0` (the main window) or another's video size.
    VideoSize(u8, u32, u32),
    /// The connection came back as a new session: extra streams have to be asked for again.
    Reconnected,
    /// A new graph sample is ready (only sent while the graph is showing).
    Graph,
    /// What the banner should say; `None` hides it.
    Status(Option<String>),
    /// A menu item was chosen.
    Menu(menu::Action),
    /// Transfer progress or outcome for the banner; `None` clears it.
    Notice(Option<String>),
    /// Clears the banner's notice if it still says this (a timed notice ending).
    NoticeExpired(String),
    /// The PC's pointer: a new image, and where it is (0..=65535 across the video) and which image it shows.
    CursorImage(Arc<crate::cursor::Image>),
    Cursor { stream: u8, visible: bool, x: u16, y: u16, shape: u32 },
    /// What a game asked of a gamepad.
    Rumble { slot: u8, large: u8, small: u8 },
    /// The PC's monitors, and which is streaming.
    Monitors(Box<proto::stream::Monitors>),
    /// A light bar colour: XInput pads have none.
    #[allow(dead_code)]
    Light { slot: u8, r: u8, g: u8, b: u8 },
    Title(String),
    Fatal(String),
}

/// One connection and its window.
struct Session {
    /// Which PC this is, as saved, and what it is called.
    address: String,
    name: String,
    window: Window,
    menu: Menu,
    input: Arc<input::Sender>,
    /// Stream health over the last minute, drawn over the picture.
    telemetry: Arc<receiver::Telemetry>,
    overlay: OverlayHandle,
    /// Until the first picture the window says what is happening; the banner takes over after that.
    waiting: bool,
    /// What the banner shows about the connection, and about transfers; the first wins.
    conn_status: Option<String>,
    notice: Option<String>,
    stop: Arc<AtomicBool>,
    receiver: Option<JoinHandle<()>>,
    sized: bool,
    video: Option<(u32, u32)>,
    /// Holds Windows' Wi-Fi background scans off while connected, if the setting is on.
    _wifi: Option<transport::wlan::Tuning>,
    /// The PC's pointer images by id, the cursor made from the current one (with its size), the PC's
    /// pointer (visible, position as a fraction of the video, image), and where this computer's pointer
    /// was last moved to and when.
    cursor_images: HashMap<u32, Arc<crate::cursor::Image>>,
    cursor_made: Option<(u32, u32, CustomCursor)>,
    pc_pointer: Option<(u8, bool, f64, f64, u32)>,
    local_pointer: Option<(u8, (f64, f64), Instant)>,
    /// Other monitors in windows of their own, and how the receiver hears of them.
    extra: Vec<ExtraWindow>,
    stream_windows: std::sync::mpsc::Sender<(u8, Option<Layer>)>,
    pads: gamepad::Poller,
    link: Arc<Link>,
    /// The PC's monitors as last told, and whether what was open last time has been asked for.
    monitors: Option<proto::stream::Monitors>,
    monitors_restored: bool,
    _hook: Option<hook::Hook>,
    /// The settings file as last read: the settings window is a process of its own.
    settings_stamp: Option<SystemTime>,
    next_settings_check: Instant,
}

/// A window showing another of the PC's monitors (stream 1 and up).
struct ExtraWindow {
    stream: u8,
    monitor: u8,
    window: Window,
    overlay: OverlayHandle,
    video: Option<(u32, u32)>,
    sized: bool,
}

impl Session {
    /// The window and video size of stream `stream`.
    fn stream_window(&self, stream: u8) -> Option<(&Window, Option<(u32, u32)>)> {
        if stream == 0 {
            return Some((&self.window, self.video));
        }
        self.extra.iter().find(|e| e.stream == stream).map(|e| (&e.window, e.video))
    }

    fn window_by_id(&self, id: WindowId) -> Option<&Window> {
        if self.window.id() == id {
            return Some(&self.window);
        }
        self.extra.iter().find(|e| e.window.id() == id).map(|e| &e.window)
    }

    /// Shows the connection status if there is one, else the transfer notice.
    fn refresh_banner(&self) {
        let (waiting, status, notice, name) = (self.waiting, self.conn_status.clone(), self.notice.clone(), self.name.clone());
        self.overlay.update(|o| {
            if waiting {
                o.placeholder = Some((name, status.unwrap_or_else(|| "Starting the stream…".into())));
                o.banner = None;
            } else {
                o.placeholder = None;
                o.banner = status.or(notice);
            }
        });
    }

    fn refresh_graph(&self) {
        let rows = self.telemetry.graph_on.load(Relaxed).then(|| graph_rows(&self.telemetry.history.lock().unwrap()));
        self.overlay.update(|o| o.graph = rows);
    }

    fn open_monitors(&self) -> Vec<u8> {
        self.extra.iter().map(|e| e.monitor).collect()
    }

    fn refresh_menu(&self, locked: bool) {
        let prefs = *self.telemetry.prefs.lock().unwrap();
        self.menu.set_checks(
            self.window.fullscreen().is_some(),
            locked,
            self.telemetry.graph_on.load(Relaxed),
            prefs.muted,
            self.telemetry.mic_on.load(Relaxed),
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The receiver draws into the windows: stop it first.
        self.stop.store(true, Relaxed);
        self.telemetry.transfers.shutdown();
        if let Some(r) = self.receiver.take() {
            let _ = r.join();
        }
        hook::set_active(false);
    }
}

/// The stats graph's rows: a line of text and the last minute of values each.
fn graph_rows(history: &History) -> Vec<(String, Vec<f32>)> {
    let bins: Vec<Bin> = history.iter().copied().collect();
    let Some(last) = bins.last() else { return vec![("Waiting for the stream…".into(), Vec::new())] };
    let recent = &bins[bins.len().saturating_sub(10)..];
    let avg = |f: fn(&Bin) -> f32| recent.iter().map(f).sum::<f32>() / recent.len() as f32;
    let min = |f: fn(&Bin) -> f32| bins.iter().map(f).fold(f32::MAX, f32::min);
    let max = |f: fn(&Bin) -> f32| bins.iter().map(f).fold(0.0, f32::max);
    let series = |f: fn(&Bin) -> f32| bins.iter().map(f).collect::<Vec<f32>>();
    let lost: u32 = bins.iter().map(|b| b.lost).sum();
    vec![
        (format!("FPS {:.0}   min {:.0}   lost {lost}", avg(|b| b.fps), min(|b| b.fps)), series(|b| b.fps)),
        (format!("Bitrate {:.0} Mbps   peak {:.0}", avg(|b| b.mbps), max(|b| b.mbps)), series(|b| b.mbps)),
        (
            format!("Latency {:.1} ms (p99 {:.1})   rtt {:.1}   decode {:.1}", avg(|b| b.latency_p50_ms), last.latency_p99_ms, last.rtt_ms, avg(|b| b.decode_ms)),
            series(|b| b.latency_p99_ms),
        ),
        (format!("Audio {:.0} ms   cushion {:.0}   peak {:.0}", avg(|b| b.audio_ms), avg(|b| b.audio_cushion_ms), max(|b| b.audio_ms)), series(|b| b.audio_ms)),
    ]
}

/// One line of a per-PC state file (`address<TAB>value`).
fn remembered(file: &str, address: &str) -> Option<String> {
    let text = std::fs::read_to_string(dialog::state_file(file)?).ok()?;
    text.lines().find_map(|l| l.split_once('\t').filter(|(a, _)| *a == address).map(|(_, n)| n.to_string()))
}

fn remember(file: &str, address: &str, value: &str) {
    let Some(path) = dialog::state_file(file) else { return };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text: String = old.lines().filter(|l| !l.starts_with(&format!("{address}\t"))).map(|l| format!("{l}\n")).collect();
    text += &format!("{address}\t{value}\n");
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(path, text);
}

fn send_msg(link: &Link, msg: proto::stream::Msg) {
    let mut buf = [0u8; 64];
    let n = msg.encode(&mut buf);
    link.send(&buf[..n]);
}

fn hwnd_of(window: &Window) -> isize {
    match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

struct App {
    /// Server from the command line.
    arg: Option<String>,
    /// `--key`, used for the connection.
    key: Option<String>,
    /// `--send`: files to send as soon as there is a connection.
    pending_sends: Vec<std::path::PathBuf>,
    all_monitors: bool,
    start_fullscreen: bool,
    view_only: bool,
    proxy: EventLoopProxy<UserEvent>,
    session: Option<Session>,
    modifiers: ModifiersState,
    /// Pointer captured: relative motion for games (toggled with Ctrl+Alt+M).
    locked: bool,
    /// The click that captured the pointer (auto capture): its release isn't sent either.
    swallow_release: Option<MouseButton>,
    /// Which stream's window has the keyboard.
    focused: Option<WindowId>,
    error: Option<String>,
}

impl App {
    fn start(&self, el: &ActiveEventLoop, server: &str, typed_key: Option<&str>) -> Result<Session, String> {
        let addr: SocketAddr = transport::resolve(server, 9000).map_err(|e| e.to_string())?;
        let sock = transport::udp_socket(transport::any_local(addr)).map_err(|e| e.to_string())?;
        sock.connect(addr).map_err(|e| e.to_string())?;
        let key = match typed_key {
            Some(k) => {
                let key = secure::Key::parse(k).map_err(|e| e.to_string())?;
                keys::save(server, &key)?;
                key
            }
            None => keys::load(server)
                .ok_or_else(|| format!("this computer isn't paired with {server} yet: open DisplayWire and add it, or run  displaywire-client --pair {server}"))?,
        };
        let link = Arc::new(Link::new(sock, key));
        let input = Arc::new(input::Sender::new(link.clone()));
        let name = servers::find(server).map(|s| s.name).filter(|n| !n.is_empty()).unwrap_or_else(|| server.to_string());
        let attrs = super::icon::stream_window(
            Window::default_attributes().with_title(format!("DisplayWire: {name}")).with_inner_size(LogicalSize::new(1280.0, 720.0)),
        );
        let window = el.create_window(attrs).map_err(|e| format!("create window: {e}"))?;
        let menu = Menu::install(&window, self.proxy.clone());
        let pads = gamepad::Poller::new(link.clone(), input.focused.clone());
        let layer = Layer::attach(&window);
        let overlay = layer.overlay();
        let stop = Arc::new(AtomicBool::new(false));
        let graph = load_graph_pref();
        let prefs = settings::Prefs::load();
        let transfers = transfer::Transfers::new(link.clone(), self.proxy.clone(), prefs.clipboard);
        transfers.spawn();
        let telemetry = Arc::new(receiver::Telemetry::new(graph, prefs, transfers));
        let (stream_windows, windows_rx) = std::sync::mpsc::channel();
        let (proxy, stop2, input2, telemetry2, link2) = (self.proxy.clone(), stop.clone(), input.clone(), telemetry.clone(), link.clone());
        let receiver = std::thread::Builder::new()
            .name("receiver".into())
            .spawn(move || receiver::run(addr, link2, input2, receiver::Screens { main: layer, others: windows_rx }, proxy, telemetry2, stop2))
            .map_err(|e| e.to_string())?;
        let session = Session {
            address: server.to_string(),
            name,
            window,
            menu,
            _hook: if self.view_only { None } else { hook::Hook::install(input.clone()) },
            input,
            telemetry,
            overlay,
            waiting: true,
            conn_status: Some("Connecting…".into()),
            notice: None,
            stop,
            receiver: Some(receiver),
            sized: false,
            video: None,
            _wifi: prefs.quiet_wifi.then(transport::wlan::tune).flatten(),
            cursor_images: HashMap::new(),
            cursor_made: None,
            pc_pointer: None,
            local_pointer: None,
            extra: Vec::new(),
            stream_windows,
            pads,
            link,
            monitors: None,
            monitors_restored: false,
            settings_stamp: settings_stamp(),
            next_settings_check: Instant::now(),
        };
        session.refresh_banner();
        session.refresh_graph();
        session.refresh_menu(false);
        Ok(session)
    }

    /// Sends a file to the PC, saying so in the banner if it can't.
    fn send_file(&mut self, path: &std::path::Path) {
        let Some(s) = &mut self.session else { return };
        if let Err(e) = s.telemetry.transfers.send_file(path) {
            eprintln!("{e}");
            s.notice = Some(e);
            s.refresh_banner();
        }
    }

    fn on_menu(&mut self, el: &ActiveEventLoop, action: menu::Action) {
        use menu::Action;
        match action {
            Action::Disconnect => {
                self.session = None;
                el.exit();
            }
            Action::ToggleStats => self.toggle_graph(),
            Action::Settings => {
                // The settings window is a process of its own; what it saves is picked up from the file.
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("--settings").spawn();
                }
            }
            Action::Microphone => self.toggle_mic(),
            Action::FullScreen => self.toggle_fullscreen(),
            Action::FullScreenAll => self.toggle_fullscreen_all(),
            Action::Capture if self.view_only => {}
            Action::Capture => self.set_locked(!self.locked),
            Action::Monitor(index) => {
                if let Some(s) = &self.session {
                    send_msg(&s.link, proto::stream::Msg::ShowMonitor { stream: 0, monitor: index });
                    if let Some(mon) = s.monitors.as_ref().and_then(|m| m.iter().nth(index as usize)) {
                        remember("monitors", &s.address, mon.name());
                    }
                }
            }
            Action::OpenMonitor(monitor) => {
                self.open_monitor(el, monitor);
                self.remember_windows();
            }
            Action::OpenAllMonitors => {
                self.open_all_monitors(el);
                self.remember_windows();
            }
            Action::Mute => {
                self.change_prefs(|p| p.muted = !p.muted);
                let muted = self.session.as_ref().is_some_and(|s| s.telemetry.prefs.lock().unwrap().muted);
                self.flash_notice(if muted { "Audio muted" } else { "Audio on" }, 1500);
            }
            Action::Shortcuts => dialog::show_message(
                "Keyboard shortcuts",
                "Ctrl+Alt+F    Full screen\n\
                 Ctrl+Shift+W  Full screen for every window of the PC, or back to windows\n\
                 Ctrl+Alt+M    Capture or release the pointer (for games)\n\
                 Ctrl+Alt+A    Open every monitor of the PC, each in its own window\n\
                 Ctrl+Alt+S    Show or hide the stats graph\n\
                 Ctrl+Alt+V    Microphone on or off\n\
                 Ctrl+Alt+D    Disconnect\n\n\
                 Everything else goes to the PC while a stream window has focus. In full screen, or with the pointer captured, \
                 that includes the Windows key, Alt+Tab and Alt+F4. In an ordinary window those stay with this computer.",
            ),
            Action::SendFile => {
                let owner = self.session.as_ref().map_or(0, |s| hwnd_of(&s.window));
                for path in dialog::choose_files(owner) {
                    self.send_file(&path);
                }
            }
        }
        if let Some(s) = &self.session {
            s.refresh_menu(self.locked);
        }
    }

    /// Applies `change` to the settings, saves them and tells the receiver.
    fn change_prefs(&mut self, change: impl FnOnce(&mut settings::Prefs)) {
        let Some(s) = &mut self.session else { return };
        let mut p = s.telemetry.prefs.lock().unwrap();
        change(&mut p);
        p.save();
        drop(p);
        s.telemetry.prefs_version.fetch_add(1, Relaxed);
        s.settings_stamp = settings_stamp();
    }

    /// Takes up settings saved by the settings window.
    fn reload_settings(&mut self) {
        let Some(s) = &mut self.session else { return };
        let now = Instant::now();
        if now < s.next_settings_check {
            return;
        }
        s.next_settings_check = now + Duration::from_millis(400);
        let stamp = settings_stamp();
        if stamp == s.settings_stamp {
            return;
        }
        s.settings_stamp = stamp;
        let new = settings::Prefs::load();
        let mut p = s.telemetry.prefs.lock().unwrap();
        if *p != new {
            *p = new;
            drop(p);
            s.telemetry.prefs_version.fetch_add(1, Relaxed);
            s.refresh_menu(self.locked);
        }
    }

    /// Turns the microphone on or off, and says which for a moment.
    fn toggle_mic(&mut self) {
        let Some(s) = &self.session else { return };
        let on = !s.telemetry.mic_on.load(Relaxed);
        s.telemetry.mic_on.store(on, Relaxed);
        self.flash_notice(if on { "Microphone on" } else { "Microphone off" }, 1500);
    }

    /// Shows `text` in the banner for `ms`, clearing it then only if nothing newer replaced it.
    fn flash_notice(&mut self, text: &str, ms: u64) {
        let Some(s) = &mut self.session else { return };
        s.notice = Some(text.to_string());
        s.refresh_banner();
        let (proxy, text) = (self.proxy.clone(), text.to_string());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            let _ = proxy.send_event(UserEvent::NoticeExpired(text));
        });
    }

    fn toggle_graph(&mut self) {
        let Some(s) = &self.session else { return };
        let on = !s.telemetry.graph_on.load(Relaxed);
        s.telemetry.graph_on.store(on, Relaxed);
        s.refresh_graph();
        save_graph_pref(on);
    }

    /// Full screen for the stream window that has focus (each monitor's window can fill a display of its own).
    fn toggle_fullscreen(&mut self) {
        let Some(s) = &self.session else { return };
        let window = self.focused.and_then(|id| s.window_by_id(id)).unwrap_or(&s.window);
        let full = window.fullscreen().is_none();
        if window.id() == s.window.id() {
            s.menu.set_visible(!full);
        }
        window.set_fullscreen(full.then_some(Fullscreen::Borderless(None)));
        self.update_hook();
    }

    /// Full screen for every stream window at once, each on the monitor it's on; if any is
    /// windowed they all go full screen, otherwise they all go back to windows.
    fn toggle_fullscreen_all(&mut self) {
        let Some(s) = &self.session else { return };
        let windows: Vec<&Window> = std::iter::once(&s.window).chain(s.extra.iter().map(|e| &e.window)).collect();
        let full = windows.iter().any(|w| w.fullscreen().is_none());
        s.menu.set_visible(!full);
        for w in windows {
            w.set_fullscreen(full.then(|| Fullscreen::Borderless(w.current_monitor())));
        }
        self.update_hook();
    }

    /// Whether Windows' own keys go to the PC: with the pointer captured, or in full screen.
    fn update_hook(&self) {
        let immersive = self.session.as_ref().is_some_and(|s| {
            s.input.focused.load(Relaxed) && (self.locked || self.focused.and_then(|id| s.window_by_id(id)).is_some_and(|w| w.fullscreen().is_some()))
        });
        hook::set_active(immersive);
    }

    fn set_locked(&mut self, locked: bool) {
        let Some(s) = &self.session else { return };
        let window = self.focused.and_then(|id| s.window_by_id(id)).unwrap_or(&s.window);
        if locked {
            let size = window.inner_size();
            let _ = window.set_cursor_position(PhysicalPosition::new(size.width / 2, size.height / 2));
            if let Err(e) = window.set_cursor_grab(CursorGrabMode::Confined) {
                eprintln!("can't capture the pointer: {e}");
                return;
            }
            window.set_cursor_visible(false);
        } else {
            for w in std::iter::once(&s.window).chain(s.extra.iter().map(|e| &e.window)) {
                let _ = w.set_cursor_grab(CursorGrabMode::None);
                w.set_cursor_visible(true);
            }
        }
        self.locked = locked;
        println!("pointer {}", if locked { "captured (Ctrl+Alt+M to release)" } else { "released" });
        self.update_hook();
        if let Some(s) = &self.session {
            s.refresh_menu(self.locked);
        }
    }

    /// The auto capture setting: capture whenever the stream window has focus.
    fn auto_capture(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.telemetry.prefs.lock().unwrap().auto_capture)
    }

    /// Window position (physical pixels) → fraction of the video, which is letterboxed to keep its aspect ratio.
    fn to_video(&self, stream: u8, pos: PhysicalPosition<f64>) -> Option<(f64, f64)> {
        let (window, video) = self.session.as_ref()?.stream_window(stream)?;
        let (vw, vh) = video?;
        let (scale, x0, y0) = video_fit(window, video)?;
        Some(((pos.x - x0) / (vw as f64 * scale), (pos.y - y0) / (vh as f64 * scale)))
    }

    /// A position outside stream `stream`'s video (only seen while a button is held) that is over
    /// another stream's video: that stream, and the position in its window.
    fn under_drag(&self, stream: u8, pos: PhysicalPosition<f64>) -> Option<(u8, PhysicalPosition<f64>)> {
        let inside = |(x, y): (f64, f64)| (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
        if self.to_video(stream, pos).is_none_or(inside) {
            return None;
        }
        let s = self.session.as_ref()?;
        let origin = s.stream_window(stream)?.0.inner_position().ok()?;
        let (sx, sy) = (origin.x as f64 + pos.x, origin.y as f64 + pos.y);
        (0..proto::stream::MAX_STREAMS as u8).filter(|&id| id != stream).find_map(|id| {
            let o = s.stream_window(id)?.0.inner_position().ok()?;
            let p = PhysicalPosition::new(sx - o.x as f64, sy - o.y as f64);
            self.to_video(id, p).filter(|&v| inside(v)).map(|_| (id, p))
        })
    }

    /// Shows monitor `monitor` in a new window (a stream of its own).
    fn open_monitor(&mut self, el: &ActiveEventLoop, monitor: u8) {
        let Some(s) = &mut self.session else { return };
        if s.extra.iter().any(|e| e.monitor == monitor) {
            return;
        }
        let Some(stream) = (1..proto::stream::MAX_STREAMS as u8).find(|id| !s.extra.iter().any(|e| e.stream == *id)) else {
            return dialog::show_message("Too many windows", "Close one of the other monitor windows first.");
        };
        let name = s.monitors.as_ref().and_then(|m| m.iter().nth(monitor as usize)).map_or(String::new(), |m| m.name().to_string());
        let mut attrs = super::icon::stream_window(
            Window::default_attributes().with_title(format!("DisplayWire: {} · {name}", s.name)).with_inner_size(LogicalSize::new(1280.0, 720.0)),
        );
        // On a display of this computer that has no stream window yet, when there is one.
        let taken: Vec<_> = std::iter::once(&s.window).chain(s.extra.iter().map(|e| &e.window)).filter_map(|w| w.current_monitor()).collect();
        if let Some(free) = el.available_monitors().find(|m| !taken.contains(m)) {
            let p = free.position();
            attrs = attrs.with_position(PhysicalPosition::new(p.x + 80, p.y + 80));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => w,
            Err(e) => return eprintln!("monitor window: {e}"),
        };
        let layer = Layer::attach(&window);
        let overlay = layer.overlay();
        overlay.update(|o| o.placeholder = Some((name, "Starting the stream…".into())));
        let _ = s.stream_windows.send((stream, Some(layer)));
        send_msg(&s.link, proto::stream::Msg::ShowMonitor { stream, monitor });
        s.extra.push(ExtraWindow { stream, monitor, window, overlay, video: None, sized: false });
        s.menu.set_monitors(s.monitors.as_ref(), &s.open_monitors());
        show_pc_pointer(s, el, self.locked);
    }

    /// Every monitor the main window isn't showing, each in a window of its own.
    fn open_all_monitors(&mut self, el: &ActiveEventLoop) {
        let Some(m) = self.session.as_ref().and_then(|s| s.monitors) else { return };
        for i in 0..m.count {
            if i != m.streaming {
                self.open_monitor(el, i);
            }
        }
    }

    fn close_monitor(&mut self, stream: u8) {
        let Some(s) = &mut self.session else { return };
        s.extra.retain(|e| e.stream != stream);
        let _ = s.stream_windows.send((stream, None));
        send_msg(&s.link, proto::stream::Msg::CloseStream { stream });
        s.menu.set_monitors(s.monitors.as_ref(), &s.open_monitors());
        self.remember_windows();
    }

    /// Which monitors have windows of their own, to open them again next time.
    fn remember_windows(&self) {
        let Some(s) = &self.session else { return };
        let Some(m) = &s.monitors else { return };
        let names: Vec<&str> = s.extra.iter().filter_map(|e| m.iter().nth(e.monitor as usize)).map(|mon| mon.name()).collect();
        remember("monitor-windows", &s.address, &names.join("\t"));
    }
}

/// Physical pixels per video pixel in the window (the video is letterboxed to fit), and the video's top-left corner.
fn video_fit(window: &Window, video: Option<(u32, u32)>) -> Option<(f64, f64, f64)> {
    let (vw, vh) = video?;
    let size = window.inner_size();
    let scale = (size.width as f64 / vw as f64).min(size.height as f64 / vh as f64);
    Some((scale, (size.width as f64 - vw as f64 * scale) / 2.0, (size.height as f64 - vh as f64 * scale) / 2.0))
}

/// Fraction of the video → window position (physical pixels).
fn from_video(window: &Window, video: Option<(u32, u32)>, fx: f64, fy: f64) -> Option<PhysicalPosition<f64>> {
    let (vw, vh) = video?;
    let (scale, x0, y0) = video_fit(window, video)?;
    Some(PhysicalPosition::new(x0 + fx * vw as f64 * scale, y0 + fy * vh as f64 * scale))
}

/// One video pixel per screen pixel when that fits the display the window is on, else as large as
/// fits; and moved, if need be, so the whole window is on that display.
fn size_to_video(window: &Window, w: u32, h: u32) {
    let Some(m) = window.current_monitor() else {
        let _ = window.request_inner_size(PhysicalSize::new(w, h));
        return;
    };
    let (screen, origin) = (m.size(), m.position());
    let fit = (screen.width as f64 * 0.9 / w as f64).min(screen.height as f64 * 0.82 / h as f64).min(1.0);
    let inner = PhysicalSize::new((w as f64 * fit).round() as u32, (h as f64 * fit).round() as u32);
    // The title bar, menu and borders stay as they are.
    let (old_inner, old_outer) = (window.inner_size(), window.outer_size());
    let outer = (inner.width + old_outer.width.saturating_sub(old_inner.width), inner.height + old_outer.height.saturating_sub(old_inner.height));
    let _ = window.request_inner_size(inner);
    if let Ok(at) = window.outer_position() {
        let place = |at: i32, size: u32, origin: i32, screen: u32| at.min(origin + screen as i32 - size as i32 - 40).max(origin);
        let moved = PhysicalPosition::new(place(at.x, outer.0, origin.x, screen.width), place(at.y, outer.1, origin.y, screen.height));
        if moved != at {
            window.set_outer_position(moved);
        }
    }
}

/// Shows the PC's pointer as the stream windows' cursor, at the size the PC shows it in the video.
/// While the pointer is captured (games) the cursor stays hidden.
fn show_pc_pointer(s: &mut Session, el: &ActiveEventLoop, locked: bool) {
    let Some((_, visible, _, _, shape)) = s.pc_pointer else { return };
    if locked {
        return;
    }
    s.window.set_cursor_visible(visible);
    for e in &s.extra {
        e.window.set_cursor_visible(visible);
    }
    let (Some(image), Some((scale, _, _))) = (s.cursor_images.get(&shape), video_fit(&s.window, s.video)) else { return };
    let key = (scale * 100.0).round() as u32;
    if let Some(m) = s.cursor_made.as_ref().filter(|m| (m.0, m.1) == (shape, key)) {
        // A window opened since gets it too.
        for e in &s.extra {
            e.window.set_cursor(m.2.clone());
        }
        return;
    }
    let (rgba, w, h, hx, hy) = crate::cursor::scaled(image, scale);
    match CustomCursor::from_rgba(rgba, w, h, hx, hy) {
        Ok(source) => {
            let cursor = el.create_custom_cursor(source);
            s.window.set_cursor(cursor.clone());
            for e in &s.extra {
                e.window.set_cursor(cursor.clone());
            }
            s.cursor_made = Some((shape, key, cursor));
        }
        Err(e) => eprintln!("cursor: {e}"),
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.session.is_some() {
            return;
        }
        let Some(server) = self.arg.take() else { return el.exit() };
        let key = self.key.take();
        match self.start(el, &server, key.as_deref()) {
            Ok(session) => {
                let _ = servers::upsert(&server, None, None);
                servers::touch(&server);
                self.focused = Some(session.window.id());
                self.session = Some(session);
                for path in std::mem::take(&mut self.pending_sends) {
                    self.send_file(&path);
                }
                if self.start_fullscreen {
                    self.toggle_fullscreen();
                }
            }
            Err(e) => {
                self.error = Some(format!("Can't connect to {server}: {e}"));
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(input) = self.session.as_ref().map(|s| s.input.clone()) else { return };
        // Which stream's window: 0 is the main one.
        let stream = match &self.session {
            Some(s) if s.window.id() == id => 0,
            Some(s) => match s.extra.iter().find(|e| e.window.id() == id) {
                Some(e) => e.stream,
                None => return,
            },
            None => return,
        };
        match event {
            WindowEvent::CloseRequested if stream != 0 => self.close_monitor(stream),
            WindowEvent::CloseRequested => {
                self.session = None;
                el.exit();
            }
            WindowEvent::Focused(false) => {
                // Another stream window may be taking the focus: its own event says so.
                if self.focused == Some(id) {
                    self.focused = None;
                    input.focused.store(false, Relaxed);
                    input.release_all();
                    if self.locked {
                        self.set_locked(false);
                    }
                    self.update_hook();
                }
            }
            WindowEvent::Focused(true) => {
                self.focused = Some(id);
                input.focused.store(true, Relaxed);
                if !self.locked && !self.view_only && self.auto_capture() {
                    self.set_locked(true);
                }
                self.update_hook();
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                // The pointer is drawn at the size the PC shows it, which follows the window.
                if let Some(s) = &mut self.session {
                    show_pc_pointer(s, el, self.locked);
                }
            }
            WindowEvent::DroppedFile(path) => self.send_file(&path),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let down = event.state == ElementState::Pressed;
                // Ctrl+Shift+W: every stream window in or out of full screen. Neither press nor
                // release goes to the PC.
                if code == KeyCode::KeyW && self.modifiers.control_key() && self.modifiers.shift_key() && !self.modifiers.alt_key() {
                    if down && !event.repeat {
                        self.on_menu(el, menu::Action::FullScreenAll);
                    }
                    return;
                }
                if self.modifiers.control_key() && self.modifiers.alt_key() {
                    let action = match code {
                        KeyCode::KeyM => Some(menu::Action::Capture),
                        KeyCode::KeyS => Some(menu::Action::ToggleStats),
                        KeyCode::KeyV => Some(menu::Action::Microphone),
                        KeyCode::KeyF => Some(menu::Action::FullScreen),
                        KeyCode::KeyD => Some(menu::Action::Disconnect),
                        KeyCode::KeyA => Some(menu::Action::OpenAllMonitors),
                        _ => None,
                    };
                    if let Some(action) = action {
                        if down && !event.repeat {
                            // The PC saw Ctrl and Alt go down; it mustn't see the letter's release alone.
                            self.on_menu(el, action);
                        }
                        return;
                    }
                }
                // In an ordinary window the Windows key is this computer's; in full screen or with the
                // pointer captured the keyboard hook has sent it already.
                if self.view_only || matches!(code, KeyCode::SuperLeft | KeyCode::SuperRight) {
                    return;
                }
                input.key(code, down, event.repeat);
            }
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } | WindowEvent::CursorMoved { .. } if self.view_only => {}
            WindowEvent::MouseInput { state, button, .. } => {
                let down = state == ElementState::Pressed;
                // Auto capture, released with Ctrl+Alt+M: a click in the window captures again, and isn't sent.
                if down && !self.locked && self.auto_capture() {
                    self.set_locked(true);
                    if self.locked {
                        self.swallow_release = Some(button);
                        return;
                    }
                }
                if !down && self.swallow_release == Some(button) {
                    self.swallow_release = None;
                    return;
                }
                input.button(button, down);
            }
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(x, y) => input.wheel_lines(x as f64, y as f64),
                MouseScrollDelta::PixelDelta(p) => input.wheel_pixels(p.x, p.y),
            },
            WindowEvent::CursorMoved { position, .. } if !self.locked => {
                // Dragging out of the window (Windows keeps sending the moves to it): over another
                // stream's window, the pointer goes on there, so a window can be dragged between the
                // other computer's screens.
                let (stream, position) = self.under_drag(stream, position).unwrap_or((stream, position));
                if let Some((x, y)) = self.to_video(stream, position) {
                    input.absolute(stream, x, y);
                    if let Some(s) = &mut self.session {
                        s.local_pointer = Some((stream, (x, y), Instant::now()));
                    }
                }
            }
            _ => {}
        }
    }

    /// While streaming, wake every 4 ms to read the gamepads; otherwise sleep until an event.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.reload_settings();
        match &mut self.session {
            Some(s) => {
                if !self.view_only {
                    s.pads.poll();
                }
                el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(4)));
            }
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
            && self.locked
            && let Some(s) = &self.session
        {
            s.input.motion(dx, dy);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        if let UserEvent::Menu(action) = event {
            return self.on_menu(el, action);
        }
        let proxy = self.proxy.clone();
        let Some(s) = &mut self.session else { return };
        match event {
            UserEvent::Menu(_) | UserEvent::Light { .. } => {}
            UserEvent::Status(text) => {
                s.conn_status = text;
                s.refresh_banner();
            }
            UserEvent::Notice(text) => {
                s.notice = text;
                s.refresh_banner();
            }
            UserEvent::Rumble { slot, large, small } => s.pads.rumble(slot, large, small),
            UserEvent::Monitors(m) => {
                s.monitors = Some(*m);
                s.menu.set_monitors(Some(&m), &s.open_monitors());
                if !s.monitors_restored {
                    s.monitors_restored = true;
                    // Back to the monitor chosen last time on this PC, and the windows that were open.
                    if let Some(name) = remembered("monitors", &s.address)
                        && let Some(i) = m.iter().position(|mon| mon.name() == name)
                        && i != m.streaming as usize
                    {
                        send_msg(&s.link, proto::stream::Msg::ShowMonitor { stream: 0, monitor: i as u8 });
                    }
                    let address = s.address.clone();
                    if self.all_monitors {
                        self.open_all_monitors(el);
                    } else if let Some(names) = remembered("monitor-windows", &address) {
                        for name in names.split('\t').filter(|n| !n.is_empty()) {
                            if let Some(i) = m.iter().position(|mon| mon.name() == name)
                                && i != m.streaming as usize
                            {
                                self.open_monitor(el, i as u8);
                            }
                        }
                    }
                }
            }
            UserEvent::CursorImage(image) => {
                // Pointers come in a handful of shapes; a long session can still pile them up.
                if s.cursor_images.len() > 32 {
                    s.cursor_images.clear();
                }
                s.cursor_images.insert(image.id, image);
                show_pc_pointer(s, el, self.locked);
            }
            UserEvent::Cursor { stream, visible, x, y, shape } => {
                let (fx, fy) = (x as f64 / 65535.0, y as f64 / 65535.0);
                s.pc_pointer = Some((stream, visible, fx, fy, shape));
                show_pc_pointer(s, el, self.locked);
                // The PC moved its pointer itself (a dialog centring it, a game): follow, unless this
                // computer's pointer moved lately, which the PC is just catching up with.
                if !self.locked
                    && s.input.focused.load(Relaxed)
                    && let Some((on, (lx, ly), at)) = s.local_pointer
                    && on == stream
                    && at.elapsed() > Duration::from_millis(250)
                    && (0.0..=1.0).contains(&lx)
                    && (0.0..=1.0).contains(&ly)
                    && ((fx - lx).abs() > 0.004 || (fy - ly).abs() > 0.004)
                    && let Some((window, video)) = s.stream_window(stream)
                    && let Some(pos) = from_video(window, video, fx, fy)
                {
                    let _ = window.set_cursor_position(pos);
                    s.local_pointer = Some((stream, (fx, fy), Instant::now()));
                }
            }
            UserEvent::NoticeExpired(text) => {
                if s.notice.as_deref() == Some(text.as_str()) {
                    s.notice = None;
                    s.refresh_banner();
                }
            }
            // On the first picture the window takes the video's size; the user can resize after. A later
            // change of size (the PC's resolution, or a game's) keeps the window and fits the picture.
            UserEvent::VideoSize(stream, w, h) if stream != 0 => {
                if let Some(e) = s.extra.iter_mut().find(|e| e.stream == stream) {
                    if !e.sized {
                        if e.window.fullscreen().is_none() {
                            size_to_video(&e.window, w, h);
                        }
                        e.sized = true;
                    }
                    e.video = Some((w, h));
                    e.overlay.update(|o| o.placeholder = None);
                }
            }
            UserEvent::Reconnected => {
                for e in &s.extra {
                    send_msg(&s.link, proto::stream::Msg::ShowMonitor { stream: e.stream, monitor: e.monitor });
                }
            }
            UserEvent::VideoSize(_, w, h) => {
                if !s.sized {
                    if s.window.fullscreen().is_none() {
                        size_to_video(&s.window, w, h);
                    }
                    s.sized = true;
                }
                let timed = |text: String, secs: u64| {
                    let proxy = proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(secs));
                        let _ = proxy.send_event(UserEvent::NoticeExpired(text));
                    });
                };
                if s.video.is_some_and(|v| v != (w, h)) {
                    let text = format!("PC is now {w}×{h}");
                    s.notice = Some(text.clone());
                    timed(text, 3);
                }
                s.video = Some((w, h));
                // The first picture is on its way to the screen.
                if std::mem::take(&mut s.waiting) && s.telemetry.prefs.lock().unwrap().muted {
                    // A mute left on from an earlier session is easy to miss.
                    let text = "Sound is muted · View › Mute Audio".to_string();
                    s.notice = Some(text.clone());
                    timed(text, 5);
                }
                s.refresh_banner();
                show_pc_pointer(s, el, self.locked);
            }
            UserEvent::Graph => s.refresh_graph(),
            UserEvent::Title(t) => {
                // "DisplayWire: <address>  ·  <stats>": the PC's name reads better than its address.
                let stats = t.split_once("  ·  ").map_or("", |(_, rest)| rest);
                s.window.set_title(&format!("DisplayWire: {}  ·  {stats}", s.name));
            }
            UserEvent::Fatal(e) => {
                self.locked = false;
                self.session = None;
                self.error = Some(e);
                el.exit();
            }
        }
    }
}

fn settings_stamp() -> Option<SystemTime> {
    std::fs::metadata(dialog::state_file("settings")?).ok()?.modified().ok()
}

fn load_graph_pref() -> bool {
    dialog::state_file("graph").and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|s| s.trim() == "on")
}

fn save_graph_pref(on: bool) {
    if let Some(path) = dialog::state_file("graph") {
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, if on { "on" } else { "off" });
    }
}

/// What this computer calls itself, for the PC's list of paired devices.
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| "Windows PC".into())
}

/// `--pair <server>`: the same exchange as the pairing window, in the terminal.
fn pair_from_terminal(server: &str) -> Result<(), String> {
    use std::io::Write;
    let addr: SocketAddr = transport::resolve(server, 9000).map_err(|e| e.to_string())?;
    let pairing = pairing::Pairing::start(addr, &device_name()).map_err(|e| e.to_string())?;
    println!("Asking {server} to pair. On the PC: tray icon, Pair new device…");
    let mut shown = None;
    loop {
        let p = pairing.progress();
        if shown.as_ref() != Some(&p) {
            shown = Some(p.clone());
            match p {
                pairing::Progress::Contacting => println!("Contacting…"),
                pairing::Progress::NoReply => println!("No reply yet: is the PC listening (Pair new device…) and the address right?"),
                pairing::Progress::WaitingForAllow => println!("Waiting for you to allow this device on the PC…"),
                pairing::Progress::NeedPin => {
                    print!("PIN shown on the PC: ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    pairing.submit_pin(line.trim());
                }
                pairing::Progress::Checking => println!("Checking…"),
                pairing::Progress::WrongPin { tries_left } => {
                    println!("Wrong PIN, {tries_left} tries left.");
                    pairing.retry();
                }
                pairing::Progress::Denied => return Err("the PC denied the request".into()),
                pairing::Progress::Busy => return Err("another device is pairing with the PC right now".into()),
                pairing::Progress::Locked => return Err("pairing was stopped: too many wrong PINs, or the PIN expired".into()),
                pairing::Progress::Cancelled => return Err("cancelled".into()),
                pairing::Progress::Failed(e) => return Err(e),
                pairing::Progress::Done { key, server_name } => {
                    servers::upsert(server, Some(&server_name), Some(key))?;
                    println!("Paired with {server_name}. Connect with:  displaywire-client {server}");
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let mut app = App {
        key: cli.key,
        pending_sends: cli.send,
        arg: cli.server,
        all_monitors: cli.all_monitors,
        start_fullscreen: cli.fullscreen,
        view_only: cli.view_only,
        proxy: event_loop.create_proxy(),
        session: None,
        modifiers: ModifiersState::empty(),
        locked: false,
        swallow_release: None,
        focused: None,
        error: None,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.session = None;
    app.error.map_or(Ok(()), Err)
}
