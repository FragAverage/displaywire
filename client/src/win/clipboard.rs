//! Clipboard text on Windows, in the shape `transfer` wants: a change counter, the text, and a setter.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::w;

const CF_UNICODETEXT: u32 = 13;

pub fn pasteboard_count() -> isize {
    unsafe { GetClipboardSequenceNumber() as isize }
}

/// Another program may have the clipboard open for a moment: try a few times.
fn open() -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

pub fn pasteboard_text() -> Option<String> {
    unsafe {
        // Password managers mark what they copy so clipboard tools leave it alone.
        let secret = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
        if secret != 0 && IsClipboardFormatAvailable(secret).is_ok() {
            return None;
        }
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() || !open() {
            return None;
        }
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let global = HGLOBAL(handle.0);
            let at = GlobalLock(global) as *const u16;
            if at.is_null() {
                return None;
            }
            let units = std::slice::from_raw_parts(at, GlobalSize(global) / 2);
            let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            let text = String::from_utf16_lossy(&units[..end]);
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// Puts `text` on the clipboard; returns the change count that results.
pub fn set_pasteboard_text(text: &str) -> isize {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if open() {
            let _ = EmptyClipboard();
            if let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) {
                let at = GlobalLock(global) as *mut u16;
                if !at.is_null() {
                    std::ptr::copy_nonoverlapping(units.as_ptr(), at, units.len());
                    let _ = GlobalUnlock(global);
                    // The clipboard owns the memory from here.
                    let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0)));
                }
            }
            let _ = CloseClipboard();
        }
    }
    pasteboard_count()
}
