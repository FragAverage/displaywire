//! Clipboard and folders of the logged-in user, for clipboard sharing and
//! file upload. The server runs as SYSTEM in the user's session, so it shares
//! their clipboard, and does file work as the user by impersonating them.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_CREATE, SHGetKnownFolderPath};

/// Changes every time the clipboard does.
pub fn clipboard_sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Another program may hold the clipboard open for a moment.
fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

pub fn clipboard_text() -> Option<String> {
    if !open_clipboard() {
        return None;
    }
    unsafe {
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal).cast::<u16>();
            if ptr.is_null() {
                return None;
            }
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(hglobal);
            Some(s)
        })();
        let _ = CloseClipboard();
        result
    }
}

pub fn set_clipboard_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    if !open_clipboard() {
        return false;
    }
    unsafe {
        let result = (|| {
            EmptyClipboard().ok()?;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let ptr = GlobalLock(hmem).cast::<u16>();
            if ptr.is_null() {
                let _ = GlobalFree(Some(hmem));
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hmem);
            // On success the system owns the memory.
            if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0))).is_err() {
                let _ = GlobalFree(Some(hmem));
                return None;
            }
            Some(())
        })();
        let _ = CloseClipboard();
        result.is_some()
    }
}

/// The logged-on user's token for the session we run in, if someone is logged in.
fn user_token() -> io::Result<HANDLE> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.map_err(io::Error::other)?;
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }.map_err(|_| io::Error::other("no user is logged in on the PC"))?;
    Ok(token)
}

/// `Downloads\Streamy` of the logged-in user.
pub fn uploads_dir() -> io::Result<PathBuf> {
    let token = user_token()?;
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_CREATE, Some(token)) };
    let _ = unsafe { CloseHandle(token) };
    let path = path.map_err(io::Error::other)?;
    let dir = PathBuf::from(unsafe { path.to_string() }.map_err(io::Error::other)?);
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as *const _)) };
    Ok(dir.join("Streamy"))
}

/// Runs `f` as the logged-in user, so what it creates is theirs.
pub fn as_user<T>(f: impl FnOnce() -> T) -> T {
    let Ok(token) = user_token() else { return f() };
    let impersonating = unsafe { ImpersonateLoggedOnUser(token) }.is_ok();
    let result = f();
    if impersonating {
        let _ = unsafe { RevertToSelf() };
    }
    let _ = unsafe { CloseHandle(token) };
    result
}
