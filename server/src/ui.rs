//! The device-management window: paired devices (with Remove) and the pairing popup
//! (Allow / Deny, then the PIN). A separate process from the streaming server, so a
//! problem here can never interrupt a stream; it talks to the server through
//! `uiserver`. Built with egui.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke, vec2};

use crate::uiserver::Client;

const BG: Color32 = Color32::from_rgb(0x15, 0x17, 0x1c);
const CARD: Color32 = Color32::from_rgb(0x1f, 0x22, 0x2a);
const LINE: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3d);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xee);
const MUTED: Color32 = Color32::from_rgb(0x8a, 0x90, 0x9c);
const ACCENT: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
const GREEN: Color32 = Color32::from_rgb(0x34, 0xc7, 0x59);
const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Device {
    id: String,
    name: String,
    created: u64,
    last_seen: u64,
    last_addr: String,
    here: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Pairing {
    #[default]
    Off,
    Listening { secs: u64 },
    Asking { secs: u64, name: String, addr: String },
    Pin { secs: u64, name: String, addr: String, pin: String, tries: u8 },
}

#[derive(Clone, Debug, Default)]
struct Model {
    pairing: Pairing,
    /// How the last pairing ended: kind and device name.
    result: Option<(String, String)>,
    devices: Vec<Device>,
    now: u64,
    /// The server asked this window to come to the front.
    front: bool,
    /// Why the server can't be reached, if it can't.
    error: Option<String>,
}

fn parse(lines: &[String]) -> Model {
    let mut m = Model::default();
    for line in lines {
        let (key, rest) = line.split_once([' ', '\t']).unwrap_or((line.as_str(), ""));
        let f: Vec<&str> = rest.split('\t').collect();
        match key {
            "pairing" => {
                // "pairing off", "pairing listening\t90", "pairing asking\t60\tname\taddr", ...
                let mut parts = line.split('\t');
                let kind = parts.next().and_then(|h| h.split(' ').nth(1)).unwrap_or("");
                let f: Vec<&str> = parts.collect();
                let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let s = |i: usize| f.get(i).copied().unwrap_or("").to_string();
                m.pairing = match kind {
                    "listening" => Pairing::Listening { secs: n(0) },
                    "asking" => Pairing::Asking { secs: n(0), name: s(1), addr: s(2) },
                    "pin" => Pairing::Pin { secs: n(0), name: s(1), addr: s(2), pin: s(3), tries: n(4) as u8 },
                    _ => Pairing::Off,
                };
            }
            "result" => m.result = Some((f.first().unwrap_or(&"").to_string(), f.get(1).unwrap_or(&"").to_string())),
            "device" if f.len() >= 6 => {
                m.devices.push(Device {
                    id: f[0].into(),
                    name: f[1].into(),
                    created: f[2].parse().unwrap_or(0),
                    last_seen: f[3].parse().unwrap_or(0),
                    last_addr: f[4].into(),
                    here: f[5] == "1",
                });
            }
            "now" => m.now = rest.trim().parse().unwrap_or(0),
            "front" => m.front = true,
            _ => {}
        }
    }
    m
}

enum Cmd {
    Send(String),
    /// Window closing: stop listening for new devices.
    Quit,
}

/// Talks to the server: asks for its state a few times a second and does what the window asks.
fn worker(port: u16, token: String, model: Arc<Mutex<Model>>, rx: Receiver<Cmd>, ctx: egui::Context) {
    let mut conn: Option<Client> = None;
    loop {
        if conn.is_none() {
            match Client::connect(port, &token) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    model.lock().unwrap().error = Some(e);
                    ctx.request_repaint();
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("connected above");
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Cmd::Send(line)) => {
                let _ = c.request(&line);
            }
            Ok(Cmd::Quit) => {
                let _ = c.request("pair-stop");
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        match c.request("state") {
            Ok(lines) => {
                let mut m = parse(&lines);
                m.error = None;
                let mut shared = model.lock().unwrap();
                m.front |= shared.front;
                *shared = m;
            }
            Err(e) => {
                conn = None;
                model.lock().unwrap().error = Some(e);
            }
        }
        ctx.request_repaint();
    }
}

fn ago(now: u64, then: u64) -> String {
    if then == 0 {
        return "never connected".into();
    }
    let d = now.saturating_sub(then);
    match d {
        0..=59 => "connected just now".into(),
        60..=3599 => format!("last connected {} min ago", d / 60),
        3600..=86_399 => format!("last connected {} h ago", d / 3600),
        _ => format!("last connected {} days ago", d / 86_400),
    }
}

fn mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

struct App {
    model: Arc<Mutex<Model>>,
    tx: Sender<Cmd>,
    /// The device whose Remove button was clicked, awaiting confirmation.
    confirming: Option<String>,
    /// What was on top last frame, to notice a new request.
    was: Pairing,
    closing: bool,
}

impl App {
    fn send(&self, line: &str) {
        let _ = self.tx.send(Cmd::Send(line.to_string()));
    }

    fn device_card(&mut self, ui: &mut egui::Ui, d: &Device, now: u64) {
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, LINE))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center() + vec2(0.0, 2.0), 4.5, if d.here { GREEN } else { LINE });
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&d.name).size(16.0).strong().color(TEXT));
                        let seen = if d.here {
                            format!("Connected now from {}", d.last_addr)
                        } else if d.last_seen == 0 {
                            "Never connected".to_string()
                        } else {
                            format!("{} from {}", ago(now, d.last_seen), d.last_addr)
                        };
                        ui.label(RichText::new(seen).size(12.5).color(MUTED));
                        ui.label(RichText::new(format!("Paired {}", &crate::clients::format_date(d.created)[..10])).size(12.5).color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.confirming.as_deref() == Some(d.id.as_str()) {
                            if ui.add(egui::Button::new(RichText::new("Remove").color(Color32::WHITE)).fill(RED)).clicked() {
                                self.send(&format!("remove {}", d.id));
                                self.confirming = None;
                            }
                            if ui.button("Cancel").clicked() {
                                self.confirming = None;
                            }
                        } else if ui.button("Remove").clicked() {
                            self.confirming = Some(d.id.clone());
                        }
                    });
                });
                if self.confirming.as_deref() == Some(d.id.as_str()) {
                    ui.add_space(6.0);
                    ui.label(RichText::new("This destroys the device's key. It will have to pair again with a new PIN before it can connect.").size(12.5).color(RED));
                }
            });
    }

    fn pairing_panel(&mut self, ui: &mut egui::Ui, p: &Pairing) {
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.5_f32, ACCENT))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::same(18))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match p {
                    Pairing::Listening { secs } => {
                        ui.label(RichText::new("Waiting for a device").size(18.0).strong().color(TEXT));
                        ui.add_space(4.0);
                        ui.label(RichText::new("On the other computer, open DisplayWire and choose this PC.").color(MUTED));
                        ui.add_space(8.0);
                        ui.label(RichText::new(format!("Listening for {}", mmss(*secs))).size(13.0).color(MUTED));
                        ui.add_space(10.0);
                        if ui.button("Cancel").clicked() {
                            self.send("pair-stop");
                        }
                    }
                    Pairing::Asking { name, addr, secs } => {
                        ui.label(RichText::new("A device wants to pair").size(18.0).strong().color(TEXT));
                        ui.add_space(6.0);
                        ui.label(RichText::new(name).size(22.0).strong().color(ACCENT));
                        ui.label(RichText::new(format!("from {addr}")).color(MUTED));
                        ui.add_space(6.0);
                        ui.label(RichText::new("Only allow it if this is your device and you are pairing it right now. It will get full control of this PC.").size(13.0).color(MUTED));
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui.add_sized([110.0, 34.0], egui::Button::new(RichText::new("Allow").size(15.0).color(Color32::WHITE)).fill(ACCENT)).clicked() {
                                self.send("pair-allow");
                            }
                            if ui.add_sized([110.0, 34.0], egui::Button::new(RichText::new("Deny").size(15.0))).clicked() {
                                self.send("pair-deny");
                            }
                            ui.label(RichText::new(format!("{}s", secs)).color(MUTED));
                        });
                    }
                    Pairing::Pin { name, pin, tries, secs, .. } => {
                        ui.label(RichText::new(format!("Enter this PIN on {name}")).size(18.0).strong().color(TEXT));
                        ui.add_space(8.0);
                        let spaced = format!("{} {}", &pin[..pin.len().min(3)], pin.get(3..).unwrap_or(""));
                        ui.label(RichText::new(spaced).size(54.0).monospace().strong().color(Color32::WHITE));
                        ui.add_space(8.0);
                        ui.label(RichText::new(format!("{tries} tries left · expires in {}", mmss(*secs))).size(13.0).color(MUTED));
                        ui.add_space(10.0);
                        if ui.button("Cancel").clicked() {
                            self.send("pair-stop");
                        }
                    }
                    Pairing::Off => {}
                }
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let model = self.model.lock().unwrap().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            self.closing = true;
            let _ = self.tx.send(Cmd::Quit);
            std::thread::sleep(Duration::from_millis(150));
        }
        // A new request or PIN comes to the front and stays above other windows until it is dealt with.
        let urgent = matches!(model.pairing, Pairing::Asking { .. } | Pairing::Pin { .. });
        let was_urgent = matches!(self.was, Pairing::Asking { .. } | Pairing::Pin { .. });
        if urgent != was_urgent || model.front {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if urgent { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal }));
        }
        if (urgent && !was_urgent) || model.front {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            if model.front {
                self.model.lock().unwrap().front = false;
            }
        }
        self.was = model.pairing.clone();

        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 16))).show(ctx, |ui| {
            ui.label(RichText::new("DisplayWire").size(13.0).color(ACCENT).strong());
            ui.label(RichText::new("Devices that can connect to this PC").size(22.0).strong().color(TEXT));
        });
        egui::TopBottomPanel::bottom("bottom").frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 14))).show(ctx, |ui| {
            if let Some((kind, name)) = &model.result {
                let (text, color) = match kind.as_str() {
                    "paired" => (format!("{name} is paired"), GREEN),
                    "denied" => (format!("Denied {name}"), MUTED),
                    "locked" => (format!("{name} entered the wrong PIN three times: pairing cancelled"), RED),
                    "timed-out" => (format!("Pairing {name} timed out"), MUTED),
                    _ => (format!("Pairing {name} cancelled"), MUTED),
                };
                ui.label(RichText::new(text).size(13.5).color(color));
                ui.add_space(6.0);
            }
            let idle = model.pairing == Pairing::Off;
            let button = egui::Button::new(RichText::new("Pair new device").size(15.0).color(Color32::WHITE)).fill(if idle { ACCENT } else { LINE });
            if ui.add_enabled(idle, button.min_size(vec2(ui.available_width(), 38.0))).clicked() {
                self.send("pair-start");
            }
        });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 4))).show(ctx, |ui| {
            if let Some(e) = &model.error {
                ui.add_space(20.0);
                ui.label(RichText::new("Can't reach the DisplayWire service").size(16.0).strong().color(RED));
                ui.label(RichText::new(e).color(MUTED));
                return;
            }
            if model.pairing != Pairing::Off {
                self.pairing_panel(ui, &model.pairing);
                ui.add_space(12.0);
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if model.devices.is_empty() {
                    ui.add_space(24.0);
                    ui.label(RichText::new("No devices yet").size(16.0).strong().color(TEXT));
                    ui.label(RichText::new("Click Pair new device, then open DisplayWire on the computer you want to stream to.").color(MUTED));
                }
                for d in &model.devices {
                    self.device_card(ui, d, model.now);
                    ui.add_space(8.0);
                }
            });
        });
        // Counts down and notices changes even if nothing else repaints.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

#[cfg(windows)]
fn install_system_font(ctx: &egui::Context) {
    let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("segoe".into(), Arc::new(egui::FontData::from_owned(bytes)));
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "segoe".into());
    ctx.set_fonts(fonts);
}

#[cfg(not(windows))]
fn install_system_font(_: &egui::Context) {}

/// Runs the window until it is closed. `port` and `token` are the server's (see `uiserver`).
pub fn run(port: u16, token: String, start_pairing: bool) -> Result<(), String> {
    let model = Arc::new(Mutex::new(Model::default()));
    let (tx, rx) = channel();
    if start_pairing {
        let _ = tx.send(Cmd::Send("pair-start".into()));
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DisplayWire: paired devices")
            .with_inner_size([540.0, 620.0])
            .with_min_inner_size([460.0, 420.0]),
        ..Default::default()
    };
    let (m, t) = (model.clone(), tx.clone());
    eframe::run_native(
        "DisplayWire",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = BG;
            visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
            visuals.override_text_color = Some(TEXT);
            ctx.set_visuals(visuals);
            ctx.style_mut(|s| {
                s.spacing.item_spacing = vec2(8.0, 6.0);
                s.spacing.button_padding = vec2(12.0, 6.0);
            });
            install_system_font(&ctx);
            std::thread::spawn(move || worker(port, token, m, rx, ctx));
            Ok(Box::new(App { model, tx: t, confirming: None, was: Pairing::Off, closing: false }))
        }),
    )
    .map_err(|e| format!("window: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_state_is_understood() {
        let lines: Vec<String> = [
            "pairing pin\t95\tJamie's MacBook\t100.79.94.10\t482913\t2",
            "result paired\tOld Laptop",
            "device a1b2c3d4\tJamie's MacBook\t1793616540\t1793700000\t100.79.94.10\t1",
            "device 0f0f0f0f\tOld Laptop\t1700000000\t0\t\t0",
            "front",
            "now 1793700100",
        ]
        .map(String::from)
        .to_vec();
        let m = parse(&lines);
        assert_eq!(m.pairing, Pairing::Pin { secs: 95, name: "Jamie's MacBook".into(), addr: "100.79.94.10".into(), pin: "482913".into(), tries: 2 });
        assert_eq!(m.result, Some(("paired".into(), "Old Laptop".into())));
        assert_eq!(m.devices.len(), 2);
        assert!(m.devices[0].here && !m.devices[1].here);
        assert_eq!(m.devices[1].last_addr, "");
        assert!(m.front);
        assert_eq!(m.now, 1_793_700_100);
        assert_eq!(parse(&["pairing off".to_string()]).pairing, Pairing::Off);
        assert_eq!(parse(&["pairing listening\t42".to_string()]).pairing, Pairing::Listening { secs: 42 });
        assert_eq!(parse(&["pairing asking\t30\tX\t1.2.3.4".to_string()]).pairing, Pairing::Asking { secs: 30, name: "X".into(), addr: "1.2.3.4".into() });
    }

    #[test]
    fn times_read_well() {
        assert_eq!(ago(1000, 0), "never connected");
        assert_eq!(ago(1000, 990), "connected just now");
        assert_eq!(ago(10_000, 10_000 - 5 * 60), "last connected 5 min ago");
        assert_eq!(ago(100_000, 100_000 - 2 * 3600), "last connected 2 h ago");
        assert_eq!(mmss(95), "1:35");
    }
}
