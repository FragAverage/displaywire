//! Where the client keeps its files on Windows, and the few native dialogs the stream window needs.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OPENFILENAMEW};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// A small settings file in `%APPDATA%\DisplayWire`.
pub fn state_file(name: &str) -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("DisplayWire").join(name))
}

/// Only the Mac client ever kept such a list.
pub fn recent_servers() -> Vec<String> {
    Vec::new()
}

pub fn show_message(title: &str, text: &str) {
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND) };
}

/// Asks for files to send to the PC.
pub fn choose_files(owner: isize) -> Vec<PathBuf> {
    let mut buf = vec![0u16; 32 * 1024];
    let title = HSTRING::from("Choose files to send to the PC");
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: HWND(owner as *mut _),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_EXPLORER | OFN_ALLOWMULTISELECT | OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        return Vec::new();
    }
    // One file: its path. Several: the folder, then each name, each ending in a zero and the list in two.
    let parts: Vec<String> = buf.split(|&c| c == 0).take_while(|p| !p.is_empty()).map(String::from_utf16_lossy).collect();
    match parts.as_slice() {
        [] => Vec::new(),
        [one] => vec![PathBuf::from(one)],
        [dir, names @ ..] => names.iter().map(|n| PathBuf::from(dir).join(n)).collect(),
    }
}
