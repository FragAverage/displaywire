//! Keys Windows keeps for itself (the Windows key, Alt+Tab, Alt+Esc, Ctrl+Esc, Alt+F4) go to the
//! remote PC instead while the stream is "immersive": full screen, or with the pointer captured.
//! A low-level keyboard hook catches them before the shell does. In an ordinary window they keep
//! working here, so there is always a way out.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_F4, VK_LWIN, VK_RWIN, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_EXTENDED, LLKHF_INJECTED, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::input::Sender;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static TARGET: Mutex<Option<Arc<Sender>>> = Mutex::new(None);
/// Keys whose press was taken, so their release is too (whatever is held by then).
static TAKEN: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// While it lives, the keys above go to `sender` whenever `set_active(true)`.
pub struct Hook(HHOOK);

impl Hook {
    /// Call on the thread that runs the window's messages.
    pub fn install(sender: Arc<Sender>) -> Option<Hook> {
        *TARGET.lock().unwrap() = Some(sender);
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), None, 0) }.ok().map(Hook)
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        ACTIVE.store(false, Relaxed);
        let _ = unsafe { UnhookWindowsHookEx(self.0) };
        *TARGET.lock().unwrap() = None;
        TAKEN.lock().unwrap().clear();
    }
}

pub fn set_active(on: bool) {
    ACTIVE.store(on, Relaxed);
}

unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let sc = k.scanCode as u16 & 0xFF | if k.flags.contains(LLKHF_EXTENDED) { 0x100 } else { 0 };
        let mut taken = TAKEN.lock().unwrap();
        let held = taken.contains(&sc);
        let wanted = ACTIVE.load(Relaxed) && !k.flags.contains(LLKHF_INJECTED) && {
            let vk = k.vkCode as u16;
            let alt = k.flags.contains(LLKHF_ALTDOWN);
            let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
            vk == VK_LWIN.0 || vk == VK_RWIN.0 || (alt && (vk == VK_TAB.0 || vk == VK_ESCAPE.0 || vk == VK_F4.0)) || (ctrl && vk == VK_ESCAPE.0)
        };
        if (down && wanted) || (!down && held) {
            if down && !held {
                taken.push(sc);
            } else if !down {
                taken.retain(|&t| t != sc);
            }
            drop(taken);
            if let Some(sender) = TARGET.lock().unwrap().as_ref() {
                sender.scancode(sc, down);
            }
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
