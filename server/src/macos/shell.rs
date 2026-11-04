//! The clipboard through AppKit's general pasteboard. Text a password manager marks as concealed
//! (`org.nspasteboard.ConcealedType`) is never read.

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Changes whenever anything (or anyone) writes to the clipboard.
pub fn clipboard_sequence() -> u32 {
    NSPasteboard::generalPasteboard().changeCount() as u32
}

pub fn clipboard_text() -> Option<String> {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let concealed = NSString::from_str("org.nspasteboard.ConcealedType");
        if pb.types().is_some_and(|t| t.iter().any(|ty| ty.isEqualToString(&concealed))) {
            return None;
        }
        pb.stringForType(NSPasteboardTypeString).map(|s| s.to_string())
    }
}

pub fn set_clipboard_text(text: &str) -> bool {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString)
    }
}
