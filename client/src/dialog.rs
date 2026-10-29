//! Native prompts for when the client runs as Streamy.app, with no terminal.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSColor, NSComboBox, NSFont, NSSecureTextField,
    NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, ns_string};

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

/// The keyboard shortcuts, for the Help menu.
pub fn show_shortcuts() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Keyboard shortcuts"));
    alert.setInformativeText(ns_string!(
        "⌃⌥M    Capture or release the pointer (for games)\n\
         ⌃⌥S    Show or hide the stats graph\n\
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
