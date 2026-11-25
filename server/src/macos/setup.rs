//! "Set Up DisplayWire Server": every permission the Mac server needs in one window, each
//! with its state (checked every second) and a button that asks for it or opens the exact
//! page of System Settings. It opens by itself while something required is missing, and from
//! the menu bar. It runs as a process of its own (`--permissions`), which macOS treats as the
//! same app, so what it sees allowed is what the server has.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering::Relaxed};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, vec2};

use super::ffi;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Allowed,
    Missing,
    /// Not known until asked (Finder, before the first request).
    NotAsked,
}

/// One row of the window.
struct Item {
    title: &'static str,
    why: &'static str,
    required: bool,
    state: State,
}

fn pane(name: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{name}");
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
}

// Apple events: whether this app may tell Finder things (where a dropped file goes).
#[repr(C)]
struct AEDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AECreateDesc(type_code: u32, data: *const c_void, size: isize, result: *mut AEDesc) -> i16;
    fn AEDisposeDesc(desc: *mut AEDesc) -> i16;
    fn AEDeterminePermissionToAutomateTarget(target: *const AEDesc, class: u32, id: u32, ask: u8) -> i32;
}

const TYPE_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
const WILDCARD: u32 = u32::from_be_bytes(*b"****");
const WOULD_ASK: i32 = -1744;

/// 0 allowed, -1743 not allowed, `WOULD_ASK` (never asked), or another error.
fn finder_permission(ask: bool) -> i32 {
    let id = b"com.apple.finder";
    unsafe {
        let mut desc = AEDesc { descriptor_type: 0, data_handle: std::ptr::null_mut() };
        if AECreateDesc(TYPE_BUNDLE_ID, id.as_ptr().cast(), id.len() as isize, &mut desc) != 0 {
            return -1;
        }
        let r = AEDeterminePermissionToAutomateTarget(&desc, WILDCARD, WILDCARD, ask as u8);
        AEDisposeDesc(&mut desc);
        r
    }
}

fn blackhole_installed() -> bool {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal::default_host().output_devices().is_ok_and(|mut d| d.any(|d| d.description().is_ok_and(|n| n.to_string().contains("BlackHole"))))
}

struct Setup {
    items: Vec<Item>,
    checked: Instant,
    /// The Finder question, being asked on another thread: its answer when it comes.
    finder: Arc<AtomicI32>,
    restarted: bool,
}

const FINDER_PENDING: i32 = i32::MIN;

impl Setup {
    fn new() -> Setup {
        let mut s = Setup {
            items: vec![
                Item {
                    title: "Screen & System Audio Recording",
                    why: "To show this Mac's screen, and play its sound, on the other computer.",
                    required: true,
                    state: State::Missing,
                },
                Item {
                    title: "Accessibility",
                    why: "So the other computer's mouse and keyboard can control this Mac.",
                    required: true,
                    state: State::Missing,
                },
                Item {
                    title: "Finder (Automation)",
                    why: "So files dragged in from the other computer land in the folder you drop them on.",
                    required: false,
                    state: State::NotAsked,
                },
                Item {
                    title: "BlackHole audio driver",
                    why: "Optional: lets apps on this Mac hear the other computer's microphone.",
                    required: false,
                    state: State::Missing,
                },
            ],
            checked: Instant::now() - Duration::from_secs(10),
            finder: Arc::new(AtomicI32::new(0)),
            restarted: false,
        };
        s.check();
        s
    }

    fn check(&mut self) {
        self.checked = Instant::now();
        let yes = |b: bool| if b { State::Allowed } else { State::Missing };
        self.items[0].state = yes(unsafe { ffi::CGPreflightScreenCaptureAccess() });
        self.items[1].state = yes(super::accessibility_allowed(false));
        if self.finder.load(Relaxed) != FINDER_PENDING {
            self.items[2].state = match finder_permission(false) {
                0 => State::Allowed,
                WOULD_ASK => State::NotAsked,
                _ => State::Missing,
            };
        }
        self.items[3].state = yes(blackhole_installed());
    }

    fn allow(&mut self, i: usize) {
        match i {
            0 => {
                // The first time, this adds the app to the list (and macOS shows its own prompt).
                unsafe { ffi::CGRequestScreenCaptureAccess() };
                pane("Privacy_ScreenCapture");
            }
            1 => {
                super::accessibility_allowed(true);
                pane("Privacy_Accessibility");
            }
            2 => {
                if self.items[2].state == State::NotAsked {
                    // macOS asks; the answer comes back on that thread.
                    let f = self.finder.clone();
                    f.store(FINDER_PENDING, Relaxed);
                    std::thread::spawn(move || f.store(finder_permission(true), Relaxed));
                } else {
                    pane("Privacy_Automation");
                }
            }
            _ => {
                let _ = std::process::Command::new("/usr/bin/open").arg("https://existential.audio/blackhole/").spawn();
            }
        }
    }

    /// Screen recording only takes effect when the server starts again.
    fn restart_server(&mut self) {
        self.restarted = true;
        if super::agent::installed() {
            let uid = unsafe { libc_getuid() };
            let _ = std::process::Command::new("/bin/launchctl").args(["kickstart", "-k", &format!("gui/{uid}/{}", super::agent::LABEL)]).spawn();
        } else if let Ok(exe) = std::env::current_exe() {
            // Not started at login: quit the running one and open the app again.
            let _ = std::process::Command::new("/usr/bin/pkill").args(["-f", "-x", &exe.to_string_lossy()]).status();
            if let Some(app) = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) {
                let _ = std::process::Command::new("/usr/bin/open").arg(app).spawn();
            }
        }
    }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

const GREEN: Color32 = Color32::from_rgb(52, 199, 89);
const ORANGE: Color32 = Color32::from_rgb(255, 159, 10);

impl eframe::App for Setup {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.checked.elapsed() > Duration::from_secs(1) {
            self.check();
        }
        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(20.0)).show(ctx, |ui| {
            ui.heading("Set up DisplayWire Server");
            ui.add_space(4.0);
            ui.label("Allow each of these, then restart the server. This window updates as you go.");
            ui.add_space(14.0);
            let mut clicked = None;
            for (i, item) in self.items.iter().enumerate() {
                egui::Frame::group(ui.style()).inner_margin(12.0).corner_radius(8.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (mark, colour) = match item.state {
                            State::Allowed => ("✔", GREEN),
                            _ if item.required => ("●", ORANGE),
                            _ => ("○", ui.visuals().weak_text_color()),
                        };
                        ui.label(RichText::new(mark).size(18.0).color(colour));
                        ui.vertical(|ui| {
                            let title = if item.required { item.title.to_string() } else { format!("{} (optional)", item.title) };
                            ui.label(RichText::new(title).strong().size(15.0));
                            ui.label(RichText::new(item.why).weak());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match item.state {
                                State::Allowed => {
                                    ui.label(RichText::new(if i == 3 { "Installed" } else { "Allowed" }).color(GREEN));
                                }
                                _ if i == 2 && self.finder.load(Relaxed) == FINDER_PENDING => {
                                    ui.label("Answer macOS's question…");
                                }
                                _ => {
                                    let label = match i {
                                        3 => "Download…",
                                        2 if item.state == State::NotAsked => "Allow…",
                                        _ => "Open Settings…",
                                    };
                                    if ui.button(label).clicked() {
                                        clicked = Some(i);
                                    }
                                }
                            }
                        });
                    });
                });
                ui.add_space(6.0);
            }
            if let Some(i) = clicked {
                self.allow(i);
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "In Settings, switch DisplayWire Server on. If it isn't listed, click +, choose it from Applications, and switch it on. \
                     If macOS asks whether DisplayWire Server may accept incoming network connections, click Allow.",
                )
                .weak(),
            );
            ui.add_space(12.0);
            let ready = self.items.iter().filter(|i| i.required).all(|i| i.state == State::Allowed);
            ui.horizontal(|ui| {
                if ready {
                    ui.label(RichText::new("✔ Everything needed is allowed.").color(GREEN).strong());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Done").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    let restart = ui.add_enabled(!self.restarted, egui::Button::new(if self.restarted { "Restarted" } else { "Restart Server" }));
                    if restart.clicked() {
                        self.restart_server();
                    }
                });
            });
        });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// Runs the window until it is closed.
pub fn run() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Set Up DisplayWire Server").with_inner_size([600.0, 560.0]).with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DisplayWire Server Setup",
        options,
        Box::new(|cc| {
            cc.egui_ctx.style_mut(|s| {
                s.spacing.item_spacing = vec2(8.0, 6.0);
                s.spacing.button_padding = vec2(12.0, 6.0);
            });
            Ok(Box::new(Setup::new()))
        }),
    )
    .map_err(|e| format!("window: {e}"))
}

/// Opens the window (another process) unless one is open already.
pub fn open() {
    let open_already = std::process::Command::new("/usr/bin/pgrep").args(["-f", "displaywire-server --permissions"]).output().is_ok_and(|o| !o.stdout.is_empty());
    if open_already {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--permissions").spawn();
    }
}

/// Whether something required is missing (the window opens by itself then).
pub fn missing() -> bool {
    !(unsafe { ffi::CGPreflightScreenCaptureAccess() }) || !super::accessibility_allowed(false)
}
