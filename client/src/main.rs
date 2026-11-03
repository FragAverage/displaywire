//! displaywire-client: receives the server's HEVC stream and shows it in a window.
//!
//!   displaywire-client <server-ip>
//!   displaywire-client                # or DisplayWire.app: asks for the server

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod dialog;
#[cfg(target_os = "macos")]
mod discovery;
#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod gamepad;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod keys;
#[cfg(target_os = "macos")]
mod launcher;
#[cfg(target_os = "macos")]
mod link;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(target_os = "macos")]
mod mic;
#[cfg(target_os = "macos")]
mod overlay;
#[cfg(target_os = "macos")]
mod pairui;
#[cfg(target_os = "macos")]
mod receiver;
#[cfg(target_os = "macos")]
mod resample;
#[cfg(target_os = "macos")]
mod servers;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod transfer;
#[cfg(target_os = "macos")]
mod updater;
#[cfg(target_os = "macos")]
mod wifi;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("displaywire-client currently runs on macOS only");
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
    use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
    use winit::window::{CursorGrabMode, Window, WindowId};

    use crate::link::Link;
    use crate::{dialog, discovery, display, input, keys, menu, overlay, receiver, servers, settings, transfer, wifi};

    #[derive(Parser)]
    #[command(about = "View a displaywire server's screen")]
    pub struct Cli {
        /// Server address, `host` or `host:port`. Asked for in a dialog if omitted.
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
        /// Something was clicked in the launcher window.
        Launcher(crate::launcher::Action),
        /// The launcher's tiles have new information.
        LauncherRows(Vec<crate::launcher::Row>),
        /// Transfer progress or outcome for the banner; `None` clears it.
        Notice(Option<String>),
        /// Clears the banner's notice if it still says this (a timed notice ending).
        NoticeExpired(String),
        Title(String),
        Fatal(String),
        /// A check for a newer version finished.
        UpdateFound(crate::updater::Found),
        /// The new version is downloaded and checked: quit so it can be swapped in (or why it can't be).
        UpdateReady(Result<(), String>),
    }

    /// One connection and its window.
    struct Session {
        /// Which PC this is, as saved.
        address: String,
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
        /// Holds AWDL down while connected, once set up and if the setting is on.
        wifi: Option<wifi::Quiet>,
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

    /// The tiles: every saved PC (online if it answered), then any found on the network that aren't saved.
    fn rows_from(saved: &[servers::Server], answered: &[(String, String)], found: &[discovery::Found]) -> Vec<crate::launcher::Row> {
        let probed = !answered.is_empty() || !found.is_empty() || saved.is_empty();
        let mut rows: Vec<crate::launcher::Row> = saved
            .iter()
            .map(|s| crate::launcher::Row {
                address: s.address.clone(),
                name: s.name.clone(),
                paired: s.key.is_some(),
                online: if probed || !answered.is_empty() { Some(answered.iter().any(|(a, _)| a == &s.address)) } else { None },
                saved: true,
            })
            .collect();
        for f in found {
            // The same PC can be saved by one address (Tailscale) and found by another (its LAN address): one tile.
            let known = |s: &servers::Server| {
                (servers::same_host(&s.address, &f.address) && servers::port(&s.address) == servers::port(&f.address)) || s.name == f.name
            };
            if !saved.iter().any(known) {
                rows.push(crate::launcher::Row { address: f.address.clone(), name: f.name.clone(), paired: false, online: Some(true), saved: false });
            }
        }
        rows
    }

    /// Checks every few seconds which saved PCs answer, and what else is on the network, while the launcher shows.
    fn spawn_probe(proxy: EventLoopProxy<UserEvent>, probing: Arc<AtomicBool>) {
        let _ = std::thread::Builder::new().name("probe".into()).spawn(move || {
            loop {
                if !probing.load(Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    continue;
                }
                let saved = servers::load();
                let addresses: Vec<String> = saved.iter().map(|s| s.address.clone()).collect();
                let answered = discovery::probe(&addresses, std::time::Duration::from_millis(600));
                let found = discovery::browse(std::time::Duration::from_millis(400));
                // A PC that has said its name gets it, unless it was named by hand.
                for s in &saved {
                    if let Some((_, name)) = answered.iter().find(|(a, _)| a == &s.address)
                        && s.name == s.address
                    {
                        let _ = servers::upsert(&s.address, Some(name), None);
                    }
                }
                let saved = servers::load();
                if probing.load(Relaxed) {
                    let _ = proxy.send_event(UserEvent::LauncherRows(rows_from(&saved, &answered, &found)));
                }
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
        });
    }

    #[cfg(debug_assertions)]
    static SCRIPT_STARTED: AtomicBool = AtomicBool::new(false);

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
        /// The click that captured the pointer (auto capture): its release isn't sent either.
        swallow_release: Option<MouseButton>,
        error: Option<String>,
        menu_installed: bool,
        launcher: Option<crate::launcher::Launcher>,
        /// Whether the launcher's status checks should run (only while it is showing).
        probing: Arc<AtomicBool>,
        probe_started: bool,
        /// The version already offered this run, so "Not now" isn't asked again.
        update_offered: Option<String>,
        updating: bool,
    }

    impl App {
        /// Connects to the server given on the command line. (Otherwise the launcher window does it.)
        fn connect(&mut self, el: &ActiveEventLoop, problem: Option<String>) {
            let Some(server) = self.arg.take() else {
                self.error = problem;
                return el.exit();
            };
            let key = self.key.take();
            match self.start(el, &server, key.as_deref()) {
                Ok(session) => self.begin(&server, session),
                Err(e) => {
                    self.error = Some(format!("Can't connect to {server}: {e}"));
                    el.exit();
                }
            }
        }

        /// A session has started: remember the PC, send queued files, put the launcher away.
        fn begin(&mut self, server: &str, session: Session) {
            let _ = servers::upsert(server, None, None);
            servers::touch(server);
            self.session = Some(session);
            self.hide_launcher();
            for path in std::mem::take(&mut self.pending_sends) {
                self.send_file(&path);
            }
        }

        /// Shows the list of PCs, and keeps its online/offline marks fresh while it shows.
        fn show_launcher(&mut self) {
            let Some(mtm) = objc2::MainThreadMarker::new() else { return };
            let launcher = self.launcher.get_or_insert_with(|| crate::launcher::Launcher::new(mtm, self.proxy.clone()));
            launcher.set_rows(rows_from(&servers::load(), &[], &[]));
            launcher.show();
            self.probing.store(true, Relaxed);
            if !self.probe_started {
                self.probe_started = true;
                spawn_probe(self.proxy.clone(), self.probing.clone());
                crate::updater::spawn_check(self.proxy.clone(), false);
            }
        }

        /// A check for a newer version finished: offer it, or (if it was asked for) say why not.
        fn on_update_found(&mut self, found: crate::updater::Found) {
            match found.result {
                Ok(Some(a)) => {
                    // Not in the middle of a stream, and not the same version twice in a run.
                    if (!found.manual && self.session.is_some()) || self.update_offered.as_deref() == Some(&a.version) && !found.manual {
                        return;
                    }
                    self.update_offered = Some(a.version.clone());
                    let text = format!(
                        "You have {}. DisplayWire downloads the new version, checks it is genuine and restarts, which takes a few seconds. Your PCs and settings are kept.",
                        crate::updater::VERSION
                    );
                    #[cfg(debug_assertions)]
                    let auto = std::env::var_os("DISPLAYWIRE_TEST_AUTOINSTALL").is_some();
                    #[cfg(not(debug_assertions))]
                    let auto = false;
                    if auto || dialog::confirm(&format!("DisplayWire {} is available", a.version), &text, "Install and Restart", "Not Now") {
                        self.install_update(a);
                    }
                }
                Ok(None) if found.manual => {
                    dialog::show_message("DisplayWire is up to date", &format!("You have the latest version ({}).", crate::updater::VERSION))
                }
                Err(e) if found.manual => dialog::show_message("Can't check for updates", &e),
                _ => {}
            }
        }

        fn install_update(&mut self, a: update::Available) {
            if self.updating {
                return;
            }
            self.updating = true;
            let proxy = self.proxy.clone();
            let _ = std::thread::Builder::new().name("update-install".into()).spawn(move || {
                let _ = proxy.send_event(UserEvent::UpdateReady(crate::updater::prepare(&a)));
            });
        }

        fn hide_launcher(&mut self) {
            self.probing.store(false, Relaxed);
            if let Some(l) = &self.launcher {
                l.hide();
            }
        }

        /// Redraws the list now (after a rename, a pairing...).
        fn refresh_launcher(&self) {
            if let Some(l) = &self.launcher {
                l.set_rows(rows_from(&servers::load(), &[], &[]));
            }
        }

        fn on_launcher(&mut self, el: &ActiveEventLoop, action: crate::launcher::Action) {
            use crate::launcher::Action;
            match action {
                Action::Connect(address) => self.connect_to(el, &address),
                Action::Pair(address) => self.pair_and_connect(el, &address),
                Action::Add => {
                    if let Some(address) = dialog::ask_address() {
                        let _ = servers::upsert(&address, None, None);
                        self.pair_and_connect(el, &address);
                    }
                }
                Action::Rename(address) => {
                    let current = servers::find(&address).map(|s| s.name).unwrap_or_default();
                    if let Some(name) = dialog::ask_text("Rename PC", &format!("A name for {address}."), &current, "Name", "Rename") {
                        let _ = servers::upsert(&address, Some(&name), None);
                        self.refresh_launcher();
                    }
                }
                Action::Forget(address) => {
                    let name = servers::find(&address).map(|s| s.name).unwrap_or_else(|| address.clone());
                    if dialog::confirm(
                        &format!("Forget {name}?"),
                        "This Mac will forget the PC and its key. To use it again, pair again from the PC. The PC still lists this Mac until you remove it there.",
                        "Forget",
                        "Cancel",
                    ) {
                        let _ = servers::forget(&address);
                        self.refresh_launcher();
                    }
                }
                Action::Closed => {
                    if self.session.is_none() {
                        el.exit();
                    }
                }
            }
        }

        /// Connects to a saved PC, pairing first if this Mac has no key for it.
        fn connect_to(&mut self, el: &ActiveEventLoop, address: &str) {
            if servers::key_for(address).is_none() {
                return self.pair_and_connect(el, address);
            }
            match self.start(el, address, None) {
                Ok(session) => self.begin(address, session),
                Err(e) => dialog::show_message("Can't connect", &format!("{address}: {e}")),
            }
        }

        /// The PIN window, then (if it worked) a connection.
        fn pair_and_connect(&mut self, el: &ActiveEventLoop, address: &str) {
            let name = servers::find(address).map_or_else(|| address.to_string(), |s| s.name);
            match crate::pairui::pair(address, &name) {
                crate::pairui::Outcome::Paired { key, server_name } => {
                    if let Err(e) = servers::upsert(address, Some(&server_name), Some(key)) {
                        dialog::show_message("Couldn't save the key", &e);
                        return;
                    }
                    self.refresh_launcher();
                    self.connect_to(el, address);
                }
                crate::pairui::Outcome::Cancelled => self.refresh_launcher(),
                crate::pairui::Outcome::Failed(e) => dialog::show_message("Pairing failed", &e),
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
                    format!("this Mac isn't paired with {server} yet: open DisplayWire and add the PC, or run  displaywire-client --pair {server}")
                })?,
            };
            let link = Arc::new(Link::new(sock, key));
            let input = Arc::new(input::Sender::new(link.clone()));
            let attrs = Window::default_attributes()
                .with_title(format!("DisplayWire: {server}"))
                .with_inner_size(LogicalSize::new(1280.0, 720.0));
            let window = el.create_window(attrs).map_err(|e| format!("create window: {e}"))?;
            let layer = display::Layer::attach(&window);
            let stop = Arc::new(AtomicBool::new(false));
            let graph = load_graph_pref();
            let prefs = settings::Prefs::load();
            let quiet_wifi = prefs.quiet_wifi && wifi::ready();
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
                address: server.to_string(),
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
                wifi: quiet_wifi.then(wifi::Quiet::start),
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

        fn on_menu(&mut self, _el: &ActiveEventLoop, action: menu::Action) {
            match action {
                menu::Action::Connect | menu::Action::Disconnect => {
                    // Back to the list of PCs.
                    self.locked = false;
                    self.session = None;
                    self.interactive = true;
                    self.show_launcher();
                }
                menu::Action::ToggleStats => self.toggle_graph(),
                menu::Action::Settings => self.edit_settings(),
                menu::Action::Microphone => self.toggle_mic(),
                menu::Action::Mute => self.change_prefs(|p| p.muted = !p.muted),
                menu::Action::Shortcuts => dialog::show_shortcuts(),
                menu::Action::WifiSetup => self.setup_wifi(),
                menu::Action::CheckUpdates => crate::updater::spawn_check(self.proxy.clone(), true),
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

        /// One-time setup for pausing AWDL while streaming (see `wifi`).
        fn setup_wifi(&mut self) {
            if wifi::ready() {
                dialog::show_message("Wi-Fi tuning is already set up", "DisplayWire pauses AirDrop and AirPlay's Wi-Fi link while you stream, and restores it when you disconnect. Turn it off in Settings.");
                return;
            }
            match wifi::install_rule() {
                Ok(()) => {
                    // Take effect on the connection that's open now.
                    let on = settings::Prefs::load().quiet_wifi;
                    if let Some(s) = &mut self.session
                        && on
                        && s.wifi.is_none()
                    {
                        s.wifi = Some(wifi::Quiet::start());
                    }
                    dialog::show_message("Wi-Fi tuning is on", "While you stream, DisplayWire now pauses AirDrop and AirPlay's Wi-Fi link, which adds bursts of delay, and brings it back when you disconnect or quit.");
                }
                Err(e) if e == "cancelled" => {}
                Err(e) => dialog::show_message("Couldn't set up Wi-Fi tuning", &e),
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
                // macOS freezes the cursor where it is and still clicks whatever is under it, such as the Dock icon
                // that just focused the window: park it over the stream first. (Warping lets go of a grab, so before.)
                let size = s.window.inner_size();
                let _ = s.window.set_cursor_position(PhysicalPosition::new(size.width / 2, size.height / 2));
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

        /// The auto capture setting: capture whenever the stream window has focus.
        fn auto_capture(&self) -> bool {
            self.session.as_ref().is_some_and(|s| s.telemetry.prefs.lock().unwrap().auto_capture)
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
            #[cfg(debug_assertions)]
            if let Ok(script) = std::env::var("DISPLAYWIRE_TEST_SCRIPT")
                && !SCRIPT_STARTED.swap(true, Relaxed)
            {
                // Debug builds only: `connect:ADDR,wait:5,disconnect,wait:2,quit` sends what clicks would, so flows can be tested without a mouse.
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    use crate::launcher::Action;
                    for step in script.split(',') {
                        let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
                        let event = match cmd {
                            "wait" => {
                                std::thread::sleep(std::time::Duration::from_secs(arg.parse().unwrap_or(1)));
                                continue;
                            }
                            "connect" => UserEvent::Launcher(Action::Connect(arg.to_string())),
                            "pair" => UserEvent::Launcher(Action::Pair(arg.to_string())),
                            "disconnect" => UserEvent::Menu(menu::Action::Disconnect),
                            _ => UserEvent::Launcher(Action::Closed),
                        };
                        let _ = proxy.send_event(event);
                    }
                });
            }
            if self.session.is_none() && self.launcher.is_none() {
                if self.interactive {
                    self.show_launcher();
                } else {
                    self.connect(el, None);
                }
            }
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            let Some(input) = self.session.as_ref().map(|s| s.input.clone()) else { return };
            match event {
                WindowEvent::CloseRequested => {
                    // Back to the list of PCs (⌘Q quits); with a server given on the command line, close means done.
                    if self.interactive {
                        self.locked = false;
                        self.session = None;
                        self.show_launcher();
                    } else {
                        el.exit();
                    }
                }
                WindowEvent::Focused(false) => {
                    input.focused.store(false, Relaxed);
                    input.release_all();
                    if self.locked {
                        self.set_locked(false);
                    }
                }
                WindowEvent::Focused(true) => {
                    input.focused.store(true, Relaxed);
                    if !self.locked && self.auto_capture() {
                        self.set_locked(true);
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
                    let down = state == ElementState::Pressed;
                    // Auto capture, released with ⌃⌥M: a click in the window captures again, and isn't sent.
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
            match event {
                UserEvent::Menu(action) => return self.on_menu(el, action),
                UserEvent::Launcher(action) => return self.on_launcher(el, action),
                UserEvent::UpdateFound(found) => return self.on_update_found(found),
                UserEvent::UpdateReady(result) => {
                    self.updating = false;
                    match result {
                        // Leave the event loop (not `terminate`, which re-enters it); the swap script waits for us to exit.
                        Ok(()) => el.exit(),
                        Err(e) => dialog::show_message("The update didn't install", &e),
                    }
                    return;
                }
                UserEvent::LauncherRows(rows) => {
                    if self.session.is_none()
                        && let Some(l) = &self.launcher
                    {
                        l.set_rows(rows);
                    }
                    return;
                }
                _ => {}
            }
            let proxy = self.proxy.clone();
            let Some(s) = &mut self.session else { return };
            match event {
                UserEvent::Menu(_) | UserEvent::Launcher(_) | UserEvent::LauncherRows(_) | UserEvent::UpdateFound(_) | UserEvent::UpdateReady(_) => {}
                UserEvent::Status(text) => {
                    s.conn_status = text;
                    s.refresh_banner();
                }
                UserEvent::Notice(text) => {
                    s.notice = text;
                    s.refresh_banner();
                }
                UserEvent::NoticeExpired(text) => {
                    if s.notice.as_deref() == Some(text.as_str()) {
                        s.notice = None;
                        s.refresh_banner();
                    }
                }
                // One video pixel per screen pixel on first frame; the user can resize after. A later
                // change of size (the PC's resolution, or a game's) keeps the window and fits the picture.
                UserEvent::VideoSize(w, h) => {
                    if !s.sized {
                        let _ = s.window.request_inner_size(PhysicalSize::new(w, h));
                        s.sized = true;
                    }
                    if s.video.is_some_and(|v| v != (w, h)) {
                        let text = format!("PC is now {w}×{h}");
                        s.notice = Some(text.clone());
                        s.refresh_banner();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            let _ = proxy.send_event(UserEvent::NoticeExpired(text));
                        });
                    }
                    s.video = Some((w, h));
                }
                UserEvent::Graph => s.overlay.update(&s.telemetry.history.lock().unwrap()),
                UserEvent::Title(t) => s.window.set_title(&t),
                UserEvent::Fatal(e) => {
                    eprintln!("error: {e}");
                    self.locked = false;
                    let address = self.session.as_ref().map(|s| s.address.clone());
                    self.session = None;
                    if self.interactive || address.is_some() && self.launcher.is_some() {
                        self.interactive = true;
                        self.show_launcher();
                        if let Some(address) = address {
                            // No reply at all most likely means the PC no longer knows this Mac.
                            if e.starts_with("no reply") {
                                if dialog::confirm(
                                    "Can't reach the PC",
                                    &format!("{e}\n\nIf this Mac was removed on the PC, pair it again."),
                                    "Pair again",
                                    "Cancel",
                                ) {
                                    let _ = servers::unpair(&address);
                                    self.pair_and_connect(el, &address);
                                }
                            } else {
                                dialog::show_message("Disconnected", &e);
                            }
                        }
                    } else {
                        self.error = Some(e);
                        el.exit();
                    }
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

    /// `--pair <server>`: the same exchange as the pairing window, in the terminal.
    fn pair_from_terminal(server: &str) -> Result<(), String> {
        use std::io::Write;
        let addr: SocketAddr = transport::resolve(server, 9000).map_err(|e| e.to_string())?;
        let name = std::env::var("HOSTNAME").unwrap_or_else(|_| "Mac".into());
        let pairing = pairing::Pairing::start(addr, &name).map_err(|e| e.to_string())?;
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
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    pub fn run(cli: Cli) -> Result<(), String> {
        if let Some(which) = &cli.preview {
            match which.as_str() {
                "connect" => drop(dialog::ask_address()),
                "settings" => drop(dialog::ask_settings(settings::Prefs::load())),
                "shortcuts" => dialog::show_shortcuts(),
                "pair" => match crate::pairui::pair(&cli.server.clone().unwrap_or_else(|| "127.0.0.1:9100".into()), "Test PC") {
                    crate::pairui::Outcome::Paired { server_name, .. } => println!("paired with {server_name}"),
                    crate::pairui::Outcome::Cancelled => println!("cancelled"),
                    crate::pairui::Outcome::Failed(e) => println!("failed: {e}"),
                },
                other => return Err(format!("unknown dialog {other}")),
            }
            return Ok(());
        }
        if let Some(server) = &cli.pair {
            return pair_from_terminal(server);
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
            swallow_release: None,
            error: None,
            menu_installed: false,
            launcher: None,
            probing: Arc::new(AtomicBool::new(false)),
            probe_started: false,
            update_offered: None,
            updating: false,
        };
        event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
        app.session = None;
        app.error.map_or(Ok(()), Err)
    }
}
