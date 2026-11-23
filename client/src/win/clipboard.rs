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

const CF_HDROP: u32 = 15;

/// Files copied in Explorer.
pub fn pasteboard_files() -> Option<Vec<std::path::PathBuf>> {
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() || !open() {
            return None;
        }
        let files = (|| {
            let drop = HDROP(GetClipboardData(CF_HDROP).ok()?.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            let mut v = Vec::new();
            for i in 0..n {
                let len = DragQueryFileW(drop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                v.push(std::path::PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
            (!v.is_empty()).then_some(v)
        })();
        let _ = CloseClipboard();
        files
    }
}

/// Puts files on the clipboard as Explorer's Copy does: pasting them copies them. Returns the
/// change count that results.
pub fn set_pasteboard_files(paths: &[std::path::PathBuf]) -> isize {
    // DROPFILES (20 bytes: offset of the list, a point, two flags) then the paths, each
    // nul-terminated, and one more nul.
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().to_string_lossy().encode_utf16());
        list.push(0);
    }
    list.push(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(20 + list.len() * 2);
    bytes.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    bytes.extend_from_slice(&[0; 8]); // pt
    bytes.extend_from_slice(&0u32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for u in list {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    unsafe {
        if open() {
            let _ = EmptyClipboard();
            if let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) {
                let at = GlobalLock(global) as *mut u8;
                if !at.is_null() {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
                    let _ = GlobalUnlock(global);
                    let _ = SetClipboardData(CF_HDROP, Some(HANDLE(global.0)));
                }
            }
            let _ = CloseClipboard();
        }
    }
    pasteboard_count()
}

/// Where files copied on the other computer wait to be pasted.
pub fn clipboard_files_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("TEMP"))?;
    Some(std::path::PathBuf::from(base).join("DisplayWire").join("Clipboard"))
}
