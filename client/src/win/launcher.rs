//! The list of PCs: add one, pair with it (the PIN), connect, rename, forget, and the stream
//! settings. An egui window. A connection is a process of its own (`displaywire-client <address>`,
//! see `app`), so the stream never shares an event loop with this window; while it runs this
//! window waits minimized, and comes back when the stream ends, with the reason if it failed.

use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke, vec2};
use pairing::Progress;

use super::app::{EXIT_NO_REPLY, device_name};
use crate::settings::Prefs;
use crate::{discovery, servers};

const BG: Color32 = Color32::from_rgb(0x15, 0x17, 0x1c);
const CARD: Color32 = Color32::from_rgb(0x1f, 0x22, 0x2a);
const LINE: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3d);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xee);
const MUTED: Color32 = Color32::from_rgb(0x8a, 0x90, 0x9c);
const ACCENT: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
const GREEN: Color32 = Color32::from_rgb(0x34, 0xc7, 0x59);
const ORANGE: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);

const FPS_CAPS: [u16; 7] = [0, 30, 60, 90, 120, 144, 240];
const FEC_LEVELS: [(u8, &str); 5] = [(0, "Off"), (5, "Light (5%)"), (10, "Normal (10%)"), (20, "Strong (20%)"), (30, "Maximum (30%)")];
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One tile.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    address: String,
    name: String,
    paired: bool,
    /// `None` until the first check has finished.
    online: Option<bool>,
    saved: bool,
}

/// The tiles: every saved PC (online if it answered), then any found on the network that aren't saved.
fn rows_from(saved: &[servers::Server], answered: &[(String, String)], found: &[discovery::Found], probed: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = saved
        .iter()
        .map(|s| Row {
            address: s.address.clone(),
            name: s.name.clone(),
            paired: s.key.is_some(),
            online: probed.then(|| answered.iter().any(|(a, _)| a == &s.address)),
            saved: true,
        })
        .collect();
    for f in found {
        // The same PC can be saved by one address (Tailscale) and found by another (its LAN address): one tile.
        let known =
            |s: &servers::Server| (servers::same_host(&s.address, &f.address) && servers::port(&s.address) == servers::port(&f.address)) || s.name == f.name;
        if !saved.iter().any(known) {
            rows.push(Row { address: f.address.clone(), name: f.name.clone(), paired: false, online: Some(true), saved: false });
        }
    }
    rows
}

/// Checks every few seconds which saved PCs answer, and what else is on the network, while the list shows.
fn spawn_probe(ctx: egui::Context, probing: Arc<AtomicBool>) -> Receiver<Vec<Row>> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new().name("probe".into()).spawn(move || {
        loop {
            if !probing.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
            let saved = servers::load();
            let addresses: Vec<String> = saved.iter().map(|s| s.address.clone()).collect();
            let answered = discovery::probe(&addresses, Duration::from_millis(600));
            let found = discovery::browse(Duration::from_millis(400));
            // A PC that has said its name gets it, unless it was named by hand.
            for s in &saved {
                if let Some((_, name)) = answered.iter().find(|(a, _)| a == &s.address)
                    && s.name == s.address
                {
                    let _ = servers::upsert(&s.address, Some(name), None);
                }
            }
            if probing.load(Relaxed) {
                if tx.send(rows_from(&servers::load(), &answered, &found, true)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
    });
    rx
}

/// A stream that is running: its process, and the last error it printed.
struct Stream {
    address: String,
    name: String,
    child: Arc<Mutex<Child>>,
    error: Arc<Mutex<Option<String>>>,
}

enum View {
    List,
    Add { address: String },
    Rename { address: String, name: String },
    Forget { address: String, name: String },
    Pairing { address: String, name: String, pairing: pairing::Pairing, pin: String, wrong: Option<u8> },
    Settings(SettingsForm),
    /// Something to read: a title, the text, and (when the PC stopped answering) the PC to pair again.
    Message { title: String, text: String, pair_again: Option<String> },
}

struct Launcher {
    rows: Vec<Row>,
    probe: Receiver<Vec<Row>>,
    probing: Arc<AtomicBool>,
    view: View,
    stream: Option<Stream>,
    /// Debug builds only: `DISPLAYWIRE_TEST_SCRIPT` (`pair:ADDR` or `connect:ADDR`) hasn't been run yet.
    #[cfg(debug_assertions)]
    script_pending: bool,
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(20.0).strong().color(TEXT));
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(13.5).color(MUTED));
}

fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized([120.0, 34.0], egui::Button::new(RichText::new(text).size(15.0).color(Color32::WHITE)).fill(ACCENT))
}

fn secondary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized([100.0, 34.0], egui::Button::new(RichText::new(text).size(15.0)))
}

fn style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT;
    ctx.set_visuals(visuals);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(10.0, 10.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        for (_, font) in s.text_styles.iter_mut() {
            font.size = font.size.max(14.5);
        }
    });
}

impl Launcher {
    fn new(cc: &eframe::CreationContext<'_>) -> Launcher {
        style(&cc.egui_ctx);
        let probing = Arc::new(AtomicBool::new(true));
        Launcher {
            rows: rows_from(&servers::load(), &[], &[], false),
            probe: spawn_probe(cc.egui_ctx.clone(), probing.clone()),
            probing,
            view: View::List,
            stream: None,
            #[cfg(debug_assertions)]
            script_pending: true,
        }
    }

    fn refresh(&mut self) {
        // Keep what is known about who is online.
        let old = std::mem::take(&mut self.rows);
        self.rows = rows_from(&servers::load(), &[], &[], false);
        for r in &mut self.rows {
            r.online = old.iter().find(|o| o.address == r.address).and_then(|o| o.online);
        }
        let found: Vec<Row> = old.into_iter().filter(|o| !o.saved && !self.rows.iter().any(|r| r.address == o.address)).collect();
        self.rows.extend(found);
    }

    /// Connects to a PC, pairing first if this computer has no key for it.
    fn connect(&mut self, ctx: &egui::Context, address: &str) {
        if servers::key_for(address).is_none() {
            return self.pair(address);
        }
        let name = servers::find(address).map_or_else(|| address.to_string(), |s| s.name);
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return self.say("Can't connect", &e.to_string()),
        };
        let mut command = Command::new(exe);
        command.arg(address).arg("--from-launcher").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
        // Debug builds only: more arguments for the stream, for tests.
        #[cfg(debug_assertions)]
        if let Ok(more) = std::env::var("DISPLAYWIRE_TEST_ARGS") {
            command.args(more.split_whitespace());
        }
        let spawned = command.spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return self.say("Can't connect", &e.to_string()),
        };
        let error = Arc::new(Mutex::new(None));
        if let Some(stderr) = child.stderr.take() {
            let (error, ctx) = (error.clone(), ctx.clone());
            let _ = std::thread::Builder::new().name("stream-errors".into()).spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(e) = line.strip_prefix("error: ") {
                        *error.lock().unwrap() = Some(e.to_string());
                    }
                }
                // The stream has ended: back to the front, so the list (or the reason) is seen.
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            });
        }
        self.stream = Some(Stream { address: address.to_string(), name, child: Arc::new(Mutex::new(child)), error });
        self.probing.store(false, Relaxed);
        self.view = View::List;
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }

    fn pair(&mut self, address: &str) {
        let name = servers::find(address).map_or_else(|| address.to_string(), |s| s.name);
        let started = transport::resolve(address, 9000).and_then(|addr| pairing::Pairing::start(addr, &device_name()));
        match started {
            Ok(pairing) => self.view = View::Pairing { address: address.to_string(), name, pairing, pin: String::new(), wrong: None },
            Err(e) => self.say("Pairing failed", &format!("Can't find {address}: {e}")),
        }
    }

    fn say(&mut self, title: &str, text: &str) {
        self.view = View::Message { title: title.to_string(), text: text.to_string(), pair_again: None };
    }

    /// Notices that the stream ended, and says why if it wasn't asked to.
    fn watch_stream(&mut self, ctx: &egui::Context) {
        let Some(s) = &self.stream else { return };
        let status = match s.child.lock().unwrap().try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => return,
            Err(_) => return,
        };
        // The error line may still be on its way through the pipe.
        std::thread::sleep(Duration::from_millis(50));
        let (address, error) = (s.address.clone(), s.error.lock().unwrap().take());
        self.stream = None;
        self.probing.store(true, Relaxed);
        self.refresh();
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        match status.code() {
            Some(0) => {}
            Some(EXIT_NO_REPLY) => {
                self.view = View::Message {
                    title: "Can't reach the PC".into(),
                    text: format!("{}\n\nIf this computer was removed on the PC, pair it again.", error.unwrap_or_else(|| "The PC didn't answer.".into())),
                    pair_again: Some(address),
                }
            }
            _ => self.say("Disconnected", &error.unwrap_or_else(|| "The stream stopped unexpectedly.".into())),
        }
    }

    fn list(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Your PCs").size(24.0).strong().color(TEXT));
                note(ui, "Click Connect to see and control a PC.");
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Add PC…").size(15.0).color(Color32::WHITE)).fill(ACCENT)).clicked() {
                    self.view = View::Add { address: String::new() };
                }
                if ui.button(RichText::new("Settings").size(15.0)).clicked() {
                    self.view = View::Settings(SettingsForm::new(Prefs::load()));
                }
            });
        });
        ui.add_space(6.0);
        if self.rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No PCs yet").size(17.0).strong().color(TEXT));
                note(ui, "Click “Add PC…”. On the PC you want to control, install DisplayWire Server, then choose “Pair new device…” from its icon in the notification area.");
            });
            return;
        }
        let mut act: Option<(usize, &str)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, row) in self.rows.iter().enumerate() {
                egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(16, 12)).show(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                ui.label(RichText::new(&row.name).size(17.0).strong().color(TEXT));
                                if row.name != row.address {
                                    ui.label(RichText::new(&row.address).size(12.5).color(MUTED));
                                }
                                let (text, color) = match (row.paired, row.online) {
                                    (false, _) => ("Not paired yet", ORANGE),
                                    (true, Some(true)) => ("● Online", GREEN),
                                    (true, Some(false)) => ("○ Offline", MUTED),
                                    (true, None) => ("Checking…", MUTED),
                                };
                                ui.label(RichText::new(text).size(12.5).color(color));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if row.saved {
                                    ui.menu_button(RichText::new("More").size(14.0), |ui| {
                                        if ui.button("Rename…").clicked() {
                                            act = Some((i, "rename"));
                                            ui.close();
                                        }
                                        if ui.button("Forget this PC").clicked() {
                                            act = Some((i, "forget"));
                                            ui.close();
                                        }
                                    });
                                }
                                if row.paired {
                                    if primary(ui, "Connect").clicked() {
                                        act = Some((i, "connect"));
                                    }
                                } else if primary(ui, "Pair").clicked() {
                                    act = Some((i, "pair"));
                                }
                            });
                        });
                    },
                );
            }
        });
        if let Some((i, what)) = act {
            let row = self.rows[i].clone();
            match what {
                "connect" => self.connect(ctx, &row.address),
                "pair" => {
                    let _ = servers::upsert(&row.address, Some(&row.name), None);
                    self.refresh();
                    self.pair(&row.address);
                }
                "rename" => self.view = View::Rename { address: row.address, name: row.name },
                _ => self.view = View::Forget { address: row.address, name: row.name },
            }
        }
    }

    /// The views other than the list. Returns what to show next, if that changes.
    fn dialog(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut next: Option<View> = None;
        let mut connect: Option<String> = None;
        let mut pair: Option<String> = None;
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        match &mut self.view {
            View::List => {}
            View::Add { address } => {
                heading(ui, "Add a PC");
                note(ui, "The address of the PC running DisplayWire Server: its IP address or computer name. On that PC, choose “Pair new device…” from the DisplayWire icon first.");
                let field = ui.add(egui::TextEdit::singleline(address).hint_text("e.g. 192.168.1.50 or gaming-pc").desired_width(f32::INFINITY));
                field.request_focus();
                ui.horizontal(|ui| {
                    let ok = primary(ui, "Add").clicked() || enter;
                    if ok && !address.trim().is_empty() {
                        let address = address.trim().to_string();
                        let _ = servers::upsert(&address, None, None);
                        pair = Some(address);
                    }
                    if secondary(ui, "Cancel").clicked() || escape {
                        next = Some(View::List);
                    }
                });
            }
            View::Rename { address, name } => {
                heading(ui, "Rename PC");
                note(ui, &format!("A name for {address}."));
                ui.add(egui::TextEdit::singleline(name).hint_text("Name").desired_width(f32::INFINITY)).request_focus();
                ui.horizontal(|ui| {
                    if (primary(ui, "Rename").clicked() || enter) && !name.trim().is_empty() {
                        let _ = servers::upsert(address, Some(name.trim()), None);
                        next = Some(View::List);
                    }
                    if secondary(ui, "Cancel").clicked() || escape {
                        next = Some(View::List);
                    }
                });
            }
            View::Forget { address, name } => {
                heading(ui, &format!("Forget {name}?"));
                note(ui, "This computer will forget the PC and its key. To use it again, pair again from the PC. The PC still lists this computer until you remove it there.");
                ui.horizontal(|ui| {
                    if ui.add_sized([120.0, 34.0], egui::Button::new(RichText::new("Forget").size(15.0).color(Color32::WHITE)).fill(RED)).clicked() {
                        let _ = servers::forget(address);
                        next = Some(View::List);
                    }
                    if secondary(ui, "Cancel").clicked() || escape {
                        next = Some(View::List);
                    }
                });
            }
            View::Pairing { address, name, pairing, pin, wrong } => {
                heading(ui, &format!("Pair with {name}"));
                let progress = pairing.progress();
                if let Progress::WrongPin { tries_left } = progress {
                    *wrong = Some(tries_left);
                    pin.clear();
                    pairing.retry();
                }
                let (text, ask, close) = match &progress {
                    Progress::Contacting => ("Contacting the PC…".to_string(), false, "Cancel"),
                    Progress::NoReply => (
                        "No reply yet. On the PC, click the DisplayWire icon in the notification area (bottom right), choose “Pair new device…”, and check the address is right."
                            .to_string(),
                        false,
                        "Cancel",
                    ),
                    Progress::WaitingForAllow => ("Waiting for you to allow this computer on the PC. A DisplayWire window there asks Allow or Deny.".to_string(), false, "Cancel"),
                    Progress::NeedPin | Progress::WrongPin { .. } => (
                        match wrong {
                            Some(n) => format!("That PIN was wrong. {n} {} left.", if *n == 1 { "try" } else { "tries" }),
                            None => "Type the 6-digit PIN now shown on the PC.".to_string(),
                        },
                        true,
                        "Cancel",
                    ),
                    Progress::Checking => ("Checking the PIN…".to_string(), false, "Cancel"),
                    Progress::Denied => ("The PC denied the request.".to_string(), false, "Close"),
                    Progress::Busy => ("Another device is pairing with this PC right now. Try again in a minute.".to_string(), false, "Close"),
                    Progress::Locked => {
                        ("Pairing was stopped: too many wrong PINs, or the PIN expired. Start again from the PC (Pair new device…).".to_string(), false, "Close")
                    }
                    Progress::Cancelled => (String::new(), false, "Close"),
                    Progress::Failed(e) => (e.clone(), false, "Close"),
                    Progress::Done { .. } => ("Paired.".to_string(), false, "Close"),
                };
                note(ui, &text);
                if ask {
                    let field = ui.add(
                        egui::TextEdit::singleline(pin).hint_text(RichText::new("000000").color(LINE)).char_limit(6).font(egui::FontId::monospace(30.0)).desired_width(150.0).horizontal_align(Align::Center),
                    );
                    field.request_focus();
                    // Debug builds only: the PIN from a file, so the whole exchange can be tested without typing.
                    #[cfg(debug_assertions)]
                    if let Ok(path) = std::env::var("DISPLAYWIRE_TEST_PIN_FILE")
                        && let Ok(text) = std::fs::read_to_string(path)
                    {
                        *pin = text.trim().to_string();
                    }
                    pin.retain(|c| c.is_ascii_digit());
                    if pin.len() == 6 {
                        pairing.submit_pin(pin);
                        pin.clear();
                        *wrong = None;
                    }
                }
                if let Progress::Done { key, server_name } = progress {
                    match servers::upsert(address, Some(&server_name), Some(key)) {
                        Ok(()) => connect = Some(address.clone()),
                        Err(e) => next = Some(View::Message { title: "Couldn't save the key".into(), text: e, pair_again: None }),
                    }
                } else if secondary(ui, close).clicked() || escape {
                    pairing.cancel();
                    next = Some(View::List);
                }
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            View::Settings(form) => {
                heading(ui, "Stream settings");
                note(ui, "Applied to the running stream and remembered.");
                form.show(ui);
                ui.horizontal(|ui| {
                    if primary(ui, "Save").clicked() {
                        form.prefs().save();
                        next = Some(View::List);
                    }
                    if secondary(ui, "Cancel").clicked() || escape {
                        next = Some(View::List);
                    }
                });
            }
            View::Message { title, text, pair_again } => {
                heading(ui, title);
                note(ui, text);
                ui.horizontal(|ui| {
                    if let Some(address) = pair_again {
                        if primary(ui, "Pair again").clicked() {
                            let _ = servers::unpair(address);
                            pair = Some(address.clone());
                        }
                        if secondary(ui, "Cancel").clicked() || escape {
                            next = Some(View::List);
                        }
                    } else if primary(ui, "OK").clicked() || enter || escape {
                        next = Some(View::List);
                    }
                });
            }
        }
        if let Some(view) = next {
            self.view = view;
            self.refresh();
        }
        if let Some(address) = pair {
            self.refresh();
            self.pair(&address);
        }
        if let Some(address) = connect {
            self.refresh();
            self.connect(ctx, &address);
        }
    }
}

impl eframe::App for Launcher {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(rows) = self.probe.try_recv() {
            if self.stream.is_none() {
                self.rows = rows;
            }
        }
        self.watch_stream(ctx);
        #[cfg(debug_assertions)]
        if std::mem::take(&mut self.script_pending)
            && let Ok(script) = std::env::var("DISPLAYWIRE_TEST_SCRIPT")
        {
            // What a click would do, so the flows can be tested without a mouse.
            match script.split_once(':') {
                Some(("pair", address)) => {
                    let _ = servers::upsert(address, None, None);
                    self.pair(address);
                }
                Some(("connect", address)) => self.connect(ctx, address),
                _ => {}
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(22, 18))).show(ctx, |ui| {
            if let Some(s) = &self.stream {
                ui.add_space(40.0);
                let mut stop = false;
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(format!("Connected to {}", s.name)).size(20.0).strong().color(TEXT));
                    note(ui, "The stream is in its own window. Closing that window disconnects.");
                    stop = secondary(ui, "Disconnect").clicked();
                });
                if stop {
                    let _ = s.child.lock().unwrap().kill();
                }
                ctx.request_repaint_after(Duration::from_millis(250));
                return;
            }
            match self.view {
                View::List => self.list(ctx, ui),
                _ => {
                    ui.add_space(8.0);
                    egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(20)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.dialog(ctx, ui);
                    });
                }
            }
        });
    }
}

/// The stream settings as they are edited.
struct SettingsForm {
    prefs: Prefs,
    bitrate: String,
}

impl SettingsForm {
    fn new(prefs: Prefs) -> SettingsForm {
        SettingsForm { prefs, bitrate: prefs.bitrate_mbps.to_string() }
    }

    fn prefs(&self) -> Prefs {
        Prefs { bitrate_mbps: self.bitrate.trim().parse().unwrap_or(self.prefs.bitrate_mbps), ..self.prefs }.clamped()
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.prefs;
        egui::Grid::new("settings").num_columns(2).spacing(vec2(18.0, 12.0)).show(ui, |ui| {
            ui.label("Video bitrate (Mbps)");
            ui.add(egui::TextEdit::singleline(&mut self.bitrate).desired_width(70.0));
            ui.end_row();
            ui.label("Frame rate limit");
            let title = |c: u16| if c == 0 { "No limit".to_string() } else { format!("{c} fps") };
            egui::ComboBox::from_id_salt("fps").selected_text(title(p.fps_cap)).show_ui(ui, |ui| {
                for c in FPS_CAPS {
                    ui.selectable_value(&mut p.fps_cap, c, title(c));
                }
            });
            ui.end_row();
            ui.label("Loss protection");
            let current = FEC_LEVELS.iter().find(|l| l.0 == p.fec_percent).map_or_else(|| format!("{}%", p.fec_percent), |l| l.1.to_string());
            egui::ComboBox::from_id_salt("fec").selected_text(current).show_ui(ui, |ui| {
                for (value, name) in FEC_LEVELS {
                    ui.selectable_value(&mut p.fec_percent, value, name);
                }
            });
            ui.end_row();
            ui.label("Volume");
            ui.horizontal(|ui| {
                ui.add(egui::Slider::new(&mut p.volume, 0..=100).show_value(false));
                ui.checkbox(&mut p.muted, "Mute");
            });
            ui.end_row();
        });
        ui.checkbox(&mut p.adaptive, "Lower the bitrate when the network struggles");
        ui.checkbox(&mut p.clipboard, "Share clipboard text with the PC");
        ui.checkbox(&mut p.auto_capture, "Capture the pointer when the window has focus");
        ui.checkbox(&mut p.quiet_wifi, "Pause Wi-Fi background scans while streaming");
    }
}

/// The settings alone, as the stream window's Settings opens them.
struct SettingsWindow(SettingsForm);

impl eframe::App for SettingsWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(22, 18))).show(ctx, |ui| {
            heading(ui, "Stream settings");
            note(ui, "Applied to the running stream and remembered.");
            self.0.show(ui);
            ui.horizontal(|ui| {
                if primary(ui, "Save").clicked() {
                    self.0.prefs().save();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if secondary(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

fn options(size: [f32; 2], min: [f32; 2]) -> eframe::NativeOptions {
    eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size(size).with_min_inner_size(min), ..Default::default() }
}

pub fn run() -> Result<(), String> {
    eframe::run_native("DisplayWire", options([560.0, 600.0], [460.0, 420.0]), Box::new(|cc| Ok(Box::new(Launcher::new(cc))))).map_err(|e| e.to_string())
}

pub fn run_settings() -> Result<(), String> {
    eframe::run_native(
        "DisplayWire Settings",
        options([470.0, 470.0], [440.0, 440.0]),
        Box::new(|cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(SettingsWindow(SettingsForm::new(Prefs::load()))))
        }),
    )
    .map_err(|e| e.to_string())
}
