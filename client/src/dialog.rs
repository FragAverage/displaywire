//! Native prompts for when the client runs as Streamy.app, with no terminal.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSButton, NSColor, NSComboBox, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSModalResponseOK, NSOpenPanel, NSPopUpButton, NSSecureTextField, NSSlider,
    NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, ns_string};

use crate::settings::Prefs;
use crate::{discovery, keys};

/// What the user entered.
pub struct Entered {
    pub server: String,
    /// A pairing key typed in, if any (otherwise the saved one is used).
    pub key: Option<String>,
}

const MAX_RECENT: usize = 8;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    l.setTextColor(Some(&NSColor::secondaryLabelColor()));
    l
}

/// Asks for the server (a combo box of recent and discovered ones) and, if one
/// isn't saved yet, its pairing key. `None` means quit.
pub fn ask_server(problem: Option<&str>) -> Option<Entered> {
    let mtm = MainThreadMarker::new().expect("main thread");
    let recent = recent_servers();
    let found = discovery::browse(std::time::Duration::from_millis(700));

    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Connect to Streamy"));
    let mut info = String::new();
    if let Some(p) = problem {
        info += &format!("{p}\n\n");
        alert.setAlertStyle(NSAlertStyle::Warning);
    }
    info += "Address of the PC running streamy-server.";
    if !found.is_empty() {
        let list: Vec<String> = found.iter().map(|f| format!("{} ({})", f.name, f.address)).collect();
        info += &format!("\nFound on this network: {}", list.join(", "));
    }
    alert.setInformativeText(&NSString::from_str(&info));
    alert.addButtonWithTitle(ns_string!("Connect"));
    alert.addButtonWithTitle(ns_string!("Quit"));

    let (w, h) = (320.0, 108.0);
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, w, h));
    view.addSubview(&label(mtm, "Server", rect(0.0, h - 16.0, w, 14.0)));
    let combo = NSComboBox::initWithFrame(NSComboBox::alloc(mtm), rect(0.0, h - 42.0, w, 24.0));
    combo.setCompletes(true);
    let mut items: Vec<String> = recent.clone();
    for f in &found {
        if !items.contains(&f.address) {
            items.push(f.address.clone());
        }
    }
    for item in &items {
        let s = NSString::from_str(item);
        unsafe { combo.addItemWithObjectValue(&*(&*s as *const NSString as *const AnyObject)) };
    }
    let first = problem.is_none().then(|| recent.first().or(items.first()).cloned()).flatten().unwrap_or_default();
    combo.setStringValue(&NSString::from_str(&first));
    combo.setPlaceholderString(Some(ns_string!("e.g. 192.168.1.50 or gaming-pc")));
    view.addSubview(&combo);

    let saved = keys::load(&first).is_some();
    view.addSubview(&label(mtm, "Pairing key", rect(0.0, 36.0, w, 14.0)));
    let key = NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), rect(0.0, 10.0, w, 24.0));
    key.setPlaceholderString(Some(if saved {
        ns_string!("Saved: leave empty to use it")
    } else {
        ns_string!("sy1-… from streamy-server --show-key")
    }));
    view.addSubview(&key);
    alert.setAccessoryView(Some(&view));
    alert.window().setInitialFirstResponder(Some(&combo));

    NSApplication::sharedApplication(mtm).activate();
    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let server = combo.stringValue().to_string().trim().to_string();
    let typed = key.stringValue().to_string().trim().to_string();
    Some(Entered { server, key: (!typed.is_empty()).then_some(typed) })
}

const FPS_CAPS: [u16; 7] = [0, 30, 60, 90, 120, 144, 240];
const FEC_LEVELS: [(u8, &str); 5] = [(0, "Off"), (5, "Light (5%)"), (10, "Normal (10%)"), (20, "Strong (20%)"), (30, "Maximum (30%)")];

fn row_label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l
}

fn popup(mtm: MainThreadMarker, frame: NSRect, titles: &[String], selected: usize) -> Retained<NSPopUpButton> {
    let p = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), frame, false);
    for t in titles {
        p.addItemWithTitle(&NSString::from_str(t));
    }
    p.selectItemAtIndex(selected as isize);
    p
}

/// The stream settings sheet. `None` if cancelled.
pub fn ask_settings(current: Prefs) -> Option<Prefs> {
    let mtm = MainThreadMarker::new().expect("main thread");
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Stream settings"));
    alert.setInformativeText(ns_string!("Applied to the running stream and remembered."));
    alert.addButtonWithTitle(ns_string!("Save"));
    alert.addButtonWithTitle(ns_string!("Cancel"));

    let (w, h) = (360.0, 166.0);
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, w, h));
    let rows = [h - 28.0, h - 62.0, h - 96.0, h - 128.0];
    let (lx, cx) = (0.0, 150.0);
    view.addSubview(&row_label(mtm, "Video bitrate (Mbps)", rect(lx, rows[0] + 2.0, 148.0, 20.0)));
    let bitrate = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(cx, rows[0], 80.0, 24.0));
    bitrate.setStringValue(&NSString::from_str(&current.bitrate_mbps.to_string()));
    view.addSubview(&bitrate);

    view.addSubview(&row_label(mtm, "Frame rate limit", rect(lx, rows[1] + 2.0, 148.0, 20.0)));
    let fps_titles: Vec<String> = FPS_CAPS.iter().map(|&c| if c == 0 { "No limit".to_string() } else { format!("{c} fps") }).collect();
    let fps_at = FPS_CAPS.iter().position(|&c| c == current.fps_cap).unwrap_or(0);
    let fps = popup(mtm, rect(cx, rows[1], 150.0, 26.0), &fps_titles, fps_at);
    view.addSubview(&fps);

    view.addSubview(&row_label(mtm, "Loss protection", rect(lx, rows[2] + 2.0, 148.0, 20.0)));
    let fec_titles: Vec<String> = FEC_LEVELS.iter().map(|(_, t)| t.to_string()).collect();
    let fec_at = FEC_LEVELS.iter().position(|&(p, _)| p == current.fec_percent).unwrap_or(2);
    let fec = popup(mtm, rect(cx, rows[2], 150.0, 26.0), &fec_titles, fec_at);
    view.addSubview(&fec);

    view.addSubview(&row_label(mtm, "Volume", rect(lx, rows[3] + 2.0, 148.0, 20.0)));
    let volume = NSSlider::initWithFrame(NSSlider::alloc(mtm), rect(cx, rows[3], 150.0, 22.0));
    volume.setMinValue(0.0);
    volume.setMaxValue(100.0);
    volume.setDoubleValue(current.volume as f64);
    view.addSubview(&volume);
    let clipboard =
        unsafe { NSButton::checkboxWithTitle_target_action(ns_string!("Share clipboard text with the PC"), None, None, mtm) };
    clipboard.setFrame(rect(lx, 0.0, w, 20.0));
    clipboard.setState(if current.clipboard { NSControlStateValueOn } else { NSControlStateValueOff });
    view.addSubview(&clipboard);
    alert.setAccessoryView(Some(&view));

    NSApplication::sharedApplication(mtm).activate();
    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let typed = bitrate.stringValue().to_string();
    let bitrate_mbps = typed.trim().parse().unwrap_or(current.bitrate_mbps);
    Some(
        Prefs {
            bitrate_mbps,
            fps_cap: FPS_CAPS.get(fps.indexOfSelectedItem().max(0) as usize).copied().unwrap_or(0),
            fec_percent: FEC_LEVELS.get(fec.indexOfSelectedItem().max(0) as usize).map_or(10, |&(p, _)| p),
            volume: volume.doubleValue().round().clamp(0.0, 100.0) as u8,
            muted: current.muted,
            clipboard: clipboard.state() == NSControlStateValueOn,
        }
        .clamped(),
    )
}

/// Files chosen in an open panel, to send to the PC.
pub fn choose_files() -> Vec<PathBuf> {
    let mtm = MainThreadMarker::new().expect("main thread");
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setAllowsMultipleSelection(true);
    panel.setCanChooseDirectories(false);
    panel.setMessage(Some(ns_string!("Choose files to send to the PC")));
    NSApplication::sharedApplication(mtm).activate();
    if panel.runModal() != NSModalResponseOK {
        return Vec::new();
    }
    panel.URLs().iter().filter_map(|u| u.path().map(|p| PathBuf::from(p.to_string()))).collect()
}

/// The keyboard shortcuts, for the Help menu.
pub fn show_shortcuts() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Keyboard shortcuts"));
    alert.setInformativeText(ns_string!(
        "⌃⌥M    Capture or release the pointer (for games)\n\
         ⌃⌥S    Show or hide the stats graph\n\
         ⌃⌥V    Microphone on or off (off at every start)\n\
         ⌃⌘F    Full screen\n\
         ⌘Q     Quit Streamy\n\n\
         Everything else goes to the PC while this window has focus. The ⌘ key is the Windows key there."
    ));
    alert.addButtonWithTitle(ns_string!("OK"));
    NSApplication::sharedApplication(mtm).activate();
    alert.runModal();
}

/// A small settings file in `~/Library/Application Support/Streamy`.
pub fn state_file(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Streamy").join(name))
}

/// Servers connected to before, most recent first.
pub fn recent_servers() -> Vec<String> {
    let read = |name: &str| state_file(name).and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let mut list: Vec<String> = read("servers").lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    // Before there was a list, only the last one was kept.
    let old = read("server").trim().to_string();
    if !old.is_empty() && !list.contains(&old) {
        list.push(old);
    }
    list
}

/// Puts `server` first in the list of recent servers.
pub fn remember(server: &str) {
    let mut list = recent_servers();
    list.retain(|s| s != server);
    list.insert(0, server.to_string());
    list.truncate(MAX_RECENT);
    if let Some(path) = state_file("servers") {
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, list.join("\n"));
    }
}
