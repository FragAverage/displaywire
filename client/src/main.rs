//! streamy-client: receives the server's HEVC stream and shows it in a window.
//!
//!   streamy-client <server-ip>
//!   streamy-client                # or Streamy.app: asks for the server

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod dialog;
#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod keys;
#[cfg(target_os = "macos")]
mod link;
#[cfg(target_os = "macos")]
mod overlay;
#[cfg(target_os = "macos")]
mod receiver;

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
    use crate::{dialog, display, input, keys, overlay, receiver};

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
    }

    /// Sent from the receiver thread to the UI thread.
    pub enum UserEvent {
        VideoSize(u32, u32),
        /// A new graph sample is ready (only sent while the graph is showing).
        Graph,
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
        stop: Arc<AtomicBool>,
        receiver: Option<JoinHandle<()>>,
        sized: bool,
        video: Option<(u32, u32)>,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // The receiver draws into the window's layer: stop it first.
            self.stop.store(true, Relaxed);
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
        proxy: EventLoopProxy<UserEvent>,
        session: Option<Session>,
        modifiers: ModifiersState,
        /// Pointer captured: relative motion for games (toggled with ⌃⌥M).
        locked: bool,
        error: Option<String>,
    }

    impl App {
        /// Connects to the command-line server, or asks until a connection
        /// starts or the user quits.
        fn connect(&mut self, el: &ActiveEventLoop, mut problem: Option<String>) {
            loop {
                let server = match self.arg.take() {
                    Some(s) => s,
                    None if self.interactive => match dialog::ask_server(problem.as_deref()) {
                        Some(s) => s,
                        None => return el.exit(),
                    },
                    None => {
                        self.error = problem;
                        return el.exit();
                    }
                };
                match self.start(el, &server) {
                    Ok(session) => {
                        self.session = Some(session);
                        return;
                    }
                    Err(e) => problem = Some(format!("Can't connect to {server}: {e}")),
                }
            }
        }

        fn start(&self, el: &ActiveEventLoop, server: &str) -> Result<Session, String> {
            let addr: SocketAddr = transport::resolve(server, 9000).map_err(|e| e.to_string())?;
            let sock = transport::udp_socket(transport::any_local(addr)).map_err(|e| e.to_string())?;
            sock.connect(addr).map_err(|e| e.to_string())?;
            let key = match &self.key {
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
            let telemetry = Arc::new(receiver::Telemetry::new(graph));
            let overlay = overlay::Overlay::attach(&window, graph);
            let (proxy, stop2, input2, telemetry2) = (self.proxy.clone(), stop.clone(), input.clone(), telemetry.clone());
            let receiver = std::thread::Builder::new()
                .name("receiver".into())
                .spawn(move || receiver::run(addr, link, input2, layer, proxy, telemetry2, stop2))
                .map_err(|e| e.to_string())?;
            Ok(Session { window, input, telemetry, overlay, stop, receiver: Some(receiver), sized: false, video: None })
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
            let Some(s) = &mut self.session else { return };
            match event {
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
            arg: cli.server,
            proxy: event_loop.create_proxy(),
            session: None,
            modifiers: ModifiersState::empty(),
            locked: false,
            error: None,
        };
        event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
        app.session = None;
        app.error.map_or(Ok(()), Err)
    }
}
