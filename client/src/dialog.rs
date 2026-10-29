//! Native prompts for when the client runs as Streamy.app, with no terminal.

use std::path::PathBuf;

use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, ns_string};

/// Asks for the server address, prefilled with the last one used. `None` means quit.
pub fn ask_server(problem: Option<&str>) -> Option<String> {
    let mtm = MainThreadMarker::new().expect("main thread");
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Connect to Streamy"));
    let info = match problem {
        Some(p) => format!("{p}\n\nHost name or IP address of the PC running streamy-server:"),
        None => "Host name or IP address of the PC running streamy-server:".into(),
    };
    alert.setInformativeText(&NSString::from_str(&info));
    if problem.is_some() {
        alert.setAlertStyle(NSAlertStyle::Warning);
    }
    alert.addButtonWithTitle(ns_string!("Connect"));
    alert.addButtonWithTitle(ns_string!("Quit"));

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 24.0));
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    field.setStringValue(&NSString::from_str(&last_server().unwrap_or_default()));
    field.setPlaceholderString(Some(ns_string!("e.g. 100.64.0.1 or gaming-pc")));
    alert.setAccessoryView(Some(&field));
    alert.window().setInitialFirstResponder(Some(&field));

    NSApplication::sharedApplication(mtm).activate();
    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    let server = field.stringValue().to_string().trim().to_string();
    if !server.is_empty() {
        save_server(&server);
    }
    Some(server)
}

/// A small settings file in `~/Library/Application Support/Streamy`.
pub fn state_file(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Streamy").join(name))
}

fn last_server() -> Option<String> {
    Some(std::fs::read_to_string(state_file("server")?).ok()?.trim().to_string())
}

fn save_server(server: &str) {
    if let Some(path) = state_file("server") {
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, server);
    }
}
