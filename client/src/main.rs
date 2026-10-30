//! streamy-client: receives the server's HEVC stream and shows it in a window.
//!
//!   streamy-client <server-ip>
//!   streamy-client                # or Streamy.app: asks for the server

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod dialog;
#[cfg(target_os = "macos")]
mod discovery;
#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod keys;
#[cfg(target_os = "macos")]
mod link;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(target_os = "macos")]
mod mic;
#[cfg(target_os = "macos")]
mod overlay;
#[cfg(target_os = "macos")]
mod receiver;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod transfer;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("streamy-client currently runs on macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    use clap::Parser;
    let cli = app::Cli::parse();
    timing::now_ns();
    if let Err(e) = app::run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod app {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::thread::JoinHandle;


    use clap::Parser;
    use winit::application::ApplicationHandler;
    use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
    use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
    use winit::window::{CursorGrabMode, Window, WindowId};

    use crate::link::Link;
    use crate::{dialog, display, input, keys, menu, overlay, receiver, settings, transfer};

    #[derive(Parser)]
    #[command(about = "View a streamy server's screen")]
    pub struct Cli {
        /// Server address, `host` or `host:port`. Asked for in a dialog if omitted.
        server: Option<String>,
        /// Pairing key for this server (saved for next time). Only needed once.
        #[arg(long)]
        key: Option<String>,
        /// Save a pairing key without connecting: `--set-key <server> <key>`.
        #[arg(long, num_args = 2, value_names = ["SERVER", "KEY"])]
        set_key: Option<Vec<String>>,
        /// Send these files to the PC once connected.
        #[arg(long, value_name = "FILE")]
        send: Vec<std::path::PathBuf>,
        /// Show one dialog and exit, for checking how it looks: connect, settings or shortcuts.
        #[arg(long, hide = true, value_name = "DIALOG")]
        preview: Option<String>,
    }

    /// Sent from the receiver thread to the UI thread.
    pub enum UserEvent {
        VideoSize(u32, u32),
        /// A new graph sample is ready (only sent while the graph is showing).
        Graph,
        /// What the banner should say; `None` hides it.
        Status(Option<String>),
        /// A menu item was chosen.
        Menu(menu::Action),
        /// Transfer progress or outcome for the banner; `None` clears it.
        Notice(Option<String>),
        Title(String),
        Fatal(String),
    }

    /// One connection and its window.
    struct Session {
        window: Window,
        input: Arc<input::Sender>,
        /// Stream health over the last minute, drawn by `overlay`.
        telemetry: Arc<receiver::Telemetry>,
        overlay: overlay::Overlay,
        banner: overlay::Banner,
        /// What the banner shows about the connection, and about transfers; the first wins.
        conn_status: Option<String>,
        notice: Option<String>,
        stop: Arc<AtomicBool>,
        receiver: Option<JoinHandle<()>>,
        sized: bool,
        video: Option<(u32, u32)>,
    }

    impl Session {
        /// Shows the connection status if there is one, else the transfer notice.
        fn refresh_banner(&self) {
            match self.conn_status.as_ref().or(self.notice.as_ref()) {
                Some(text) => self.banner.show(text),
                None => self.banner.hide(),
            }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // The receiver draws into the window's layer: stop it first.
            self.stop.store(true, Relaxed);
            self.telemetry.transfers.shutdown();
            if let Some(r) = self.receiver.take() {
                let _ = r.join();
            }
        }
    }

    struct App {
        /// Server from the command line; without one, errors go back to the dialog.
        arg: Option<String>,
        interactive: bool,
        /// `--key`, used for the first connection.
        key: Option<String>,
        /// `--send`: files to send as soon as there is a connection.
        pending_sends: Vec<std::path::PathBuf>,
        proxy: EventLoopProxy<UserEvent>,
        session: Option<Session>,
        modifiers: ModifiersState,
        /// Pointer captured: relative motion for games (toggled with ⌃⌥M).
        locked: bool,
        error: Option<String>,
        menu_installed: bool,
    }

    impl App {
        /// Connects to the command-line server, or asks until a connection
        /// starts or the user quits.
        fn connect(&mut self, el: &ActiveEventLoop, mut problem: Option<String>) {
            loop {
                let (server, key) = match self.arg.take() {
                    Some(s) => (s, self.key.take()),
                    None if self.interactive => match dialog::ask_server(problem.as_deref()) {
                        Some(entered) => (entered.server, entered.key),
                        None => return el.exit(),
                    },
                    None => {
                        self.error = problem;
                        return el.exit();
                    }
                };
                match self.start(el, &server, key.as_deref()) {
                    Ok(session) => {
                        dialog::remember(&server);
                        self.session = Some(session);
                        for path in std::mem::take(&mut self.pending_sends) {
                            self.send_file(&path);
                        }
                        return;
                    }
                    Err(e) => problem = Some(format!("Can't connect to {server}: {e}")),
                }
            }
        }

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
                None => keys::load(server).ok_or_else(|| {
                    format!("no pairing key for {server}: get it from the PC (streamy-server --show-key) and run  streamy-client --set-key {server} <key>")
                })?,
            };
            let link = Arc::new(Link::new(sock, key));
            let input = Arc::new(input::Sender::new(link.clone()));
            let attrs = Window::default_attributes()
                .with_title(format!("Streamy: {server}"))
                .with_inner_size(LogicalSize::new(1280.0, 720.0));
            let window = el.create_window(attrs).map_err(|e| format!("create window: {e}"))?;
            let layer = display::Layer::attach(&window);
            let stop = Arc::new(AtomicBool::new(false));
            let graph = load_graph_pref();
            let prefs = settings::Prefs::load();
            let transfers = transfer::Transfers::new(link.clone(), self.proxy.clone(), prefs.clipboard);
            transfers.spawn();
            let telemetry = Arc::new(receiver::Telemetry::new(graph, prefs, transfers));
            let overlay = overlay::Overlay::attach(&window, graph);
            let banner = overlay::Banner::attach(&window);
            let (proxy, stop2, input2, telemetry2) = (self.proxy.clone(), stop.clone(), input.clone(), telemetry.clone());
            let receiver = std::thread::Builder::new()
                .name("receiver".into())
                .spawn(move || receiver::run(addr, link, input2, layer, proxy, telemetry2, stop2))
                .map_err(|e| e.to_string())?;
            Ok(Session {
                window,
                input,
                telemetry,
                overlay,
                banner,
                conn_status: None,
                notice: None,
                stop,
                receiver: Some(receiver),
                sized: false,
                video: None,
            })
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
            match action {
                menu::Action::Connect => {
                    // Cancelling keeps the current connection.
                    if let Some(entered) = dialog::ask_server(None) {
                        self.locked = false;
                        self.session = None;
                        self.arg = Some(entered.server);
                        self.key = entered.key;
                        self.interactive = true;
                        self.connect(el, None);
                    }
                }
                menu::Action::Disconnect => {
                    self.locked = false;
                    self.session = None;
                }
                menu::Action::ToggleStats => self.toggle_graph(),
                menu::Action::Settings => self.edit_settings(),
                menu::Action::Microphone => self.toggle_mic(),
                menu::Action::Mute => self.change_prefs(|p| p.muted = !p.muted),
                menu::Action::Shortcuts => dialog::show_shortcuts(),
                menu::Action::SendFile => {
                    for path in dialog::choose_files() {
                        self.send_file(&path);
                    }
                }
            }
        }

        /// Applies `change` to the settings, saves them and tells the receiver.
        fn change_prefs(&mut self, change: impl FnOnce(&mut settings::Prefs)) {
            let Some(s) = &self.session else { return };
            let mut p = s.telemetry.prefs.lock().unwrap();
            change(&mut p);
            p.save();
            drop(p);
            s.telemetry.prefs_version.fetch_add(1, Relaxed);
        }

        fn edit_settings(&mut self) {
            let current = match &self.session {
                Some(s) => *s.telemetry.prefs.lock().unwrap(),
                None => settings::Prefs::load(),
            };
            if let Some(new) = dialog::ask_settings(current) {
                if self.session.is_some() {
                    self.change_prefs(|p| *p = new);
                } else {
                    new.save();
                }
            }
        }

        /// Turns the microphone on or off, and says which for a moment.
        fn toggle_mic(&mut self) {
            let Some(s) = &self.session else { return };
            let on = !s.telemetry.mic_on.load(Relaxed);
            s.telemetry.mic_on.store(on, Relaxed);
            let proxy = self.proxy.clone();
            let _ = proxy.send_event(UserEvent::Notice(Some(if on { "Microphone on".into() } else { "Microphone off".into() })));
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                let _ = proxy.send_event(UserEvent::Notice(None));
            });
        }

        fn toggle_graph(&mut self) {
            let Some(s) = &self.session else { return };
            let on = !s.telemetry.graph_on.load(Relaxed);
            s.telemetry.graph_on.store(on, Relaxed);
            s.overlay.set_visible(on);
            if on {
                s.overlay.update(&s.telemetry.history.lock().unwrap());
            }
            save_graph_pref(on);
        }

        fn set_locked(&mut self, locked: bool) {
            let Some(s) = &self.session else { return };
            if locked {
                let grabbed = s.window.set_cursor_grab(CursorGrabMode::Locked);
                if let Err(e) = grabbed {
                    eprintln!("can't capture the pointer: {e}");
                    return;
                }
            } else {
                let _ = s.window.set_cursor_grab(CursorGrabMode::None);
            }
            s.window.set_cursor_visible(!locked);
            self.locked = locked;
            println!("pointer {}", if locked { "captured (⌃⌥M to release)" } else { "released" });
        }

        /// Window position (physical pixels) → fraction of the video, which
        /// is letterboxed to keep its aspect ratio.
        fn to_video(&self, pos: PhysicalPosition<f64>) -> Option<(f64, f64)> {
            let s = self.session.as_ref()?;
            let (vw, vh) = s.video?;
            let size = s.window.inner_size();
            let scale = (size.width as f64 / vw as f64).min(size.height as f64 / vh as f64);
            let (w, h) = (vw as f64 * scale, vh as f64 * scale);
            let (x0, y0) = ((size.width as f64 - w) / 2.0, (size.height as f64 - h) / 2.0);
            Some(((pos.x - x0) / w, (pos.y - y0) / h))
        }
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if !self.menu_installed {
                self.menu_installed = true;
                if let Some(mtm) = objc2::MainThreadMarker::new() {
                    menu::install(mtm, self.proxy.clone());
                }
            }
            if self.session.is_none() {
                self.connect(el, None);
            }
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            let Some(input) = self.session.as_ref().map(|s| s.input.clone()) else { return };
            match event {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::Focused(false) => {
                    input.release_all();
                    if self.locked {
                        self.set_locked(false);
                    }
                }
                WindowEvent::DroppedFile(path) => self.send_file(&path),
                WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
                WindowEvent::KeyboardInput { event, .. } => {
                    let PhysicalKey::Code(code) = event.physical_key else { return };
                    let down = event.state == ElementState::Pressed;
                    let hotkey = self.modifiers.control_key() && self.modifiers.alt_key() && code == KeyCode::KeyM;
                    if hotkey {
                        if down && !event.repeat {
                            self.set_locked(!self.locked);
                        }
                        return;
                    }
                    // ⌃⌥S: show or hide the stats graph (⌃⌥G is taken by window managers).
                    if self.modifiers.control_key() && self.modifiers.alt_key() && code == KeyCode::KeyS {
                        if down && !event.repeat {
                            self.toggle_graph();
                        }
                        return;
                    }
                    // ⌃⌥V: microphone on or off.
                    if self.modifiers.control_key() && self.modifiers.alt_key() && code == KeyCode::KeyV {
                        if down && !event.repeat {
                            self.toggle_mic();
                        }
                        return;
                    }
                    input.key(code, down, event.repeat);
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    input.button(button, state == ElementState::Pressed);
                }
                WindowEvent::MouseWheel { delta, .. } => match delta {
                    MouseScrollDelta::LineDelta(x, y) => input.wheel_lines(-x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => input.wheel_pixels(-p.x, p.y),
                },
                WindowEvent::CursorMoved { position, .. } if !self.locked => {
                    if let Some((x, y)) = self.to_video(position) {
                        input.absolute(x, y);
                    }
                }
                _ => {}
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
            let Some(s) = &mut self.session else { return };
            match event {
                UserEvent::Menu(_) => {}
                UserEvent::Status(text) => {
                    s.conn_status = text;
                    s.refresh_banner();
                }
                UserEvent::Notice(text) => {
                    s.notice = text;
                    s.refresh_banner();
                }
                // One video pixel per screen pixel on first frame; the user can resize after.
                UserEvent::VideoSize(w, h) => {
                    if !s.sized {
                        let _ = s.window.request_inner_size(PhysicalSize::new(w, h));
                        s.sized = true;
                    }
                    s.video = Some((w, h));
                }
                UserEvent::Graph => s.overlay.update(&s.telemetry.history.lock().unwrap()),
                UserEvent::Title(t) => s.window.set_title(&t),
                UserEvent::Fatal(e) => {
                    eprintln!("error: {e}");
                    self.locked = false;
                    self.session = None;
                    self.connect(el, Some(e));
                }
            }
        }
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

    pub fn run(cli: Cli) -> Result<(), String> {
        if let Some(which) = &cli.preview {
            match which.as_str() {
                "connect" => drop(dialog::ask_server(None)),
                "settings" => drop(dialog::ask_settings(settings::Prefs::load())),
                "shortcuts" => dialog::show_shortcuts(),
                other => return Err(format!("unknown dialog {other}")),
            }
            return Ok(());
        }
        if let Some(args) = &cli.set_key {
            let key = secure::Key::parse(&args[1]).map_err(|e| e.to_string())?;
            keys::save(&args[0], &key)?;
            println!("saved the pairing key for {}", args[0]);
            return Ok(());
        }
        let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
        let mut app = App {
            interactive: cli.server.is_none(),
            key: cli.key,
            pending_sends: cli.send,
            arg: cli.server,
            proxy: event_loop.create_proxy(),
            session: None,
            modifiers: ModifiersState::empty(),
            locked: false,
            error: None,
            menu_installed: false,
        };
        event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
        app.session = None;
        app.error.map_or(Ok(()), Err)
    }
}
