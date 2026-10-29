//! streamy-client: receives the server's HEVC stream and shows it in a window.
//!
//!   streamy-client <server-ip>

#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod input;
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
    use std::net::{SocketAddr, UdpSocket};
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

    use crate::{display, input, receiver};

    #[derive(Parser)]
    #[command(about = "View a streamy server's screen")]
    pub struct Cli {
        /// Server address, `host` or `host:port`.
        server: String,
    }

    /// Sent from the receiver thread to the UI thread.
    pub enum UserEvent {
        VideoSize(u32, u32),
        Title(String),
        Fatal(String),
    }

    struct App {
        server: SocketAddr,
        /// Handed to the receiver thread when the window opens.
        sock: Option<UdpSocket>,
        input: Arc<input::Sender>,
        proxy: EventLoopProxy<UserEvent>,
        stop: Arc<AtomicBool>,
        window: Option<Window>,
        receiver: Option<JoinHandle<()>>,
        sized: bool,
        video: Option<(u32, u32)>,
        modifiers: ModifiersState,
        /// Pointer captured: relative motion for games (toggled with ⌃⌥M).
        locked: bool,
        error: Option<String>,
    }

    impl App {
        fn set_locked(&mut self, locked: bool) {
            let Some(window) = &self.window else { return };
            if locked {
                let grabbed = window.set_cursor_grab(CursorGrabMode::Locked);
                if let Err(e) = grabbed {
                    eprintln!("can't capture the pointer: {e}");
                    return;
                }
            } else {
                let _ = window.set_cursor_grab(CursorGrabMode::None);
            }
            window.set_cursor_visible(!locked);
            self.locked = locked;
            println!("pointer {}", if locked { "captured (⌃⌥M to release)" } else { "released" });
        }

        /// Window position (physical pixels) → fraction of the video, which
        /// is letterboxed to keep its aspect ratio.
        fn to_video(&self, pos: PhysicalPosition<f64>) -> Option<(f64, f64)> {
            let (vw, vh) = self.video?;
            let size = self.window.as_ref()?.inner_size();
            let scale = (size.width as f64 / vw as f64).min(size.height as f64 / vh as f64);
            let (w, h) = (vw as f64 * scale, vh as f64 * scale);
            let (x0, y0) = ((size.width as f64 - w) / 2.0, (size.height as f64 - h) / 2.0);
            Some(((pos.x - x0) / w, (pos.y - y0) / h))
        }
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attrs = Window::default_attributes()
                .with_title(format!("Streamy: {}", self.server))
                .with_inner_size(LogicalSize::new(1280.0, 720.0));
            let window = match el.create_window(attrs) {
                Ok(w) => w,
                Err(e) => {
                    self.error = Some(format!("create window: {e}"));
                    el.exit();
                    return;
                }
            };
            let layer = display::Layer::attach(&window);
            let (server, proxy, stop, input) = (self.server, self.proxy.clone(), self.stop.clone(), self.input.clone());
            let sock = self.sock.take().expect("socket");
            self.receiver = Some(
                std::thread::Builder::new()
                    .name("receiver".into())
                    .spawn(move || receiver::run(server, sock, input, layer, proxy, stop))
                    .expect("spawn receiver"),
            );
            self.window = Some(window);
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::Focused(false) => {
                    self.input.release_all();
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
                    self.input.key(code, down, event.repeat);
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    self.input.button(button, state == ElementState::Pressed);
                }
                WindowEvent::MouseWheel { delta, .. } => match delta {
                    MouseScrollDelta::LineDelta(x, y) => self.input.wheel_lines(-x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => self.input.wheel_pixels(-p.x, p.y),
                },
                WindowEvent::CursorMoved { position, .. } if !self.locked => {
                    if let Some((x, y)) = self.to_video(position) {
                        self.input.absolute(x, y);
                    }
                }
                _ => {}
            }
        }

        fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
            if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
                && self.locked
            {
                self.input.motion(dx, dy);
            }
        }

        fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
            let Some(window) = &self.window else { return };
            match event {
                // One video pixel per screen pixel on first frame; the user can resize after.
                UserEvent::VideoSize(w, h) => {
                    if !self.sized {
                        let _ = window.request_inner_size(PhysicalSize::new(w, h));
                        self.sized = true;
                    }
                    self.video = Some((w, h));
                }
                UserEvent::Title(t) => window.set_title(&t),
                UserEvent::Fatal(e) => {
                    self.error = Some(e);
                    el.exit();
                }
            }
        }
    }

    pub fn run(cli: Cli) -> Result<(), String> {
        let server = transport::resolve(&cli.server, 9000).map_err(|e| e.to_string())?;
        let sock = transport::udp_socket(transport::any_local(server)).map_err(|e| e.to_string())?;
        sock.connect(server).map_err(|e| e.to_string())?;
        let input = Arc::new(input::Sender::new(sock.try_clone().map_err(|e| e.to_string())?));
        let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
        let mut app = App {
            server,
            sock: Some(sock),
            input,
            proxy: event_loop.create_proxy(),
            stop: Arc::new(AtomicBool::new(false)),
            window: None,
            receiver: None,
            sized: false,
            video: None,
            modifiers: ModifiersState::empty(),
            locked: false,
            error: None,
        };
        event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
        app.stop.store(true, Relaxed);
        if let Some(r) = app.receiver.take() {
            let _ = r.join();
        }
        app.error.map_or(Ok(()), Err)
    }
}
