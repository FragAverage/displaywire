//! The clipboard, the received-files folder and notifications. The Mac server runs as the
//! logged-in user (a LaunchAgent), so there is nobody else to act as.

use std::io;
use std::path::PathBuf;

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Password managers mark what they copy with these; it isn't sent to the client.
const PRIVATE_TYPES: [&str; 2] = ["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType"];

/// Changes every time the clipboard does.
pub fn clipboard_sequence() -> u32 {
    NSPasteboard::generalPasteboard().changeCount() as u32
}

pub fn clipboard_text() -> Option<String> {
    autoreleasepool(|_| {
        let pb = NSPasteboard::generalPasteboard();
        let types = pb.types()?;
        if types.iter().any(|t| PRIVATE_TYPES.iter().any(|p| t.to_string() == *p)) {
            return None;
        }
        pb.stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
    })
}

pub fn set_clipboard_text(text: &str) -> bool {
    autoreleasepool(|_| {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
    })
}

/// `~/Downloads/DisplayWire`.
pub fn uploads_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("no home directory"))?;
    Ok(PathBuf::from(home).join("Downloads/DisplayWire"))
}

pub fn as_user<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    f()
}

/// A notification in the corner of the screen.
pub fn notify(text: &str) {
    let quoted = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"DisplayWire\"", quoted(text));
    let _ = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).spawn();
}

/// Opens a folder in the Finder.
pub fn open(path: &std::path::Path) {
    let _ = std::process::Command::new("/usr/bin/open").arg(path).spawn();
}

#[cfg(test)]
mod tests {
    /// Uses the real clipboard (and puts back what was there): `cargo test -p displaywire-server -- --ignored clipboard`.
    #[test]
    #[ignore]
    fn clipboard_round_trip() {
        let saved = super::clipboard_text();
        let before = super::clipboard_sequence();
        assert!(super::set_clipboard_text("displaywire ✓ test"));
        assert_ne!(super::clipboard_sequence(), before);
        assert_eq!(super::clipboard_text().as_deref(), Some("displaywire ✓ test"));
        if let Some(s) = saved {
            super::set_clipboard_text(&s);
        }
    }
}
