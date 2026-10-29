//! Keyboard and mouse injection with `SendInput`, the same mechanism Sunshine,
//! Parsec and Steam Remote Play use: scancodes for keys, relative motion for
//! games, absolute positions for the desktop.

use proto::stream::{
    BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, Input, KEY_BITS,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, OpenInputDesktop, SetThreadDesktop,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const SEC: u64 = 1_000_000_000;
/// Everything is released if the client goes quiet this long (it sends its
/// state at least every 50 ms while connected).
const IDLE_RELEASE_NS: u64 = SEC / 2;
const GENERIC_ALL: u32 = 0x1000_0000;
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

pub struct Injector {
    /// What we have pressed on Windows' side.
    held: Input,
    last_seq: Option<u32>,
    last_input_ns: u64,
    /// Streamed monitor in virtual-desktop pixels.
    screen: RECT,
    desktop: Option<HDESK>,
    last_error_ns: u64,
}

// HDESK is a plain handle used only from the control thread.
unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Injector {
        let screen = super::capture::primary_output()
            .and_then(|(_, output, _)| Ok(unsafe { output.GetDesc()? }.DesktopCoordinates))
            .unwrap_or_else(|e| {
                println!("input: can't find the streamed monitor ({e}); using the primary one");
                RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
            });
        Injector { held: Input::default(), last_seq: None, last_input_ns: 0, screen, desktop: None, last_error_ns: 0 }
    }

    /// A new client session: forget sequence numbers and let go of everything.
    pub fn reset(&mut self) {
        self.release_all();
        self.last_seq = None;
    }

    /// Releases everything if the client stopped sending.
    pub fn tick(&mut self, now_ns: u64) {
        if now_ns.saturating_sub(self.last_input_ns) > IDLE_RELEASE_NS && self.held != Input::default() {
            self.release_all();
        }
    }

    pub fn apply(&mut self, input: &Input, now_ns: u64) {
        if self.last_seq.is_some_and(|last| (input.seq.wrapping_sub(last) as i32) <= 0) {
            return;
        }
        self.last_seq = Some(input.seq);
        self.last_input_ns = now_ns;

        let mut events = Vec::new();
        // Move first, so a click lands where the pointer now is.
        if let Some((x, y)) = input.absolute {
            let (x, y) = self.to_virtual_desk(x, y);
            events.push(mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK));
        }
        if input.dx != 0 || input.dy != 0 {
            events.push(mouse(input.dx as i32, input.dy as i32, 0, MOUSEEVENTF_MOVE));
        }
        self.buttons(input.buttons, &mut events);
        if input.wheel != 0 {
            events.push(mouse(0, 0, input.wheel as i32 as u32, MOUSEEVENTF_WHEEL));
        }
        if input.hwheel != 0 {
            events.push(mouse(0, 0, input.hwheel as i32 as u32, MOUSEEVENTF_HWHEEL));
        }
        // Releases before presses, so a fast A→B roll never shows both down.
        for down in [false, true] {
            for code in 0..KEY_BITS as u16 {
                if input.key(code) == down && self.held.key(code) != down {
                    events.push(key(code, down));
                    self.held.set_key(code, down);
                }
            }
        }
        if input.repeat != 0 && self.held.key(input.repeat) {
            events.push(key(input.repeat, true));
        }
        self.send(&events);
    }

    fn buttons(&mut self, buttons: u8, events: &mut Vec<INPUT>) {
        const MAP: [(u8, MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, u32); 5] = [
            (BUTTON_LEFT, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
            (BUTTON_RIGHT, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
            (BUTTON_MIDDLE, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
            (BUTTON_BACK, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
            (BUTTON_FORWARD, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2),
        ];
        let changed = buttons ^ self.held.buttons;
        for (bit, down, up, data) in MAP {
            if changed & bit != 0 {
                events.push(mouse(0, 0, data, if buttons & bit != 0 { down } else { up }));
            }
        }
        self.held.buttons = buttons;
    }

    fn release_all(&mut self) {
        let mut events = Vec::new();
        self.buttons(0, &mut events);
        for code in 0..KEY_BITS as u16 {
            if self.held.key(code) {
                events.push(key(code, false));
            }
        }
        self.held = Input::default();
        self.send(&events);
    }

    /// 0..=65535 across the streamed monitor → 0..=65535 across the virtual desktop.
    fn to_virtual_desk(&self, x: u16, y: u16) -> (i32, i32) {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let s = self.screen;
        let px = s.left as i64 + x as i64 * (s.right - s.left - 1) as i64 / 65535;
        let py = s.top as i64 + y as i64 * (s.bottom - s.top - 1) as i64 / 65535;
        (((px - vx as i64) * 65535 / (vw - 1) as i64) as i32, ((py - vy as i64) * 65535 / (vh - 1) as i64) as i32)
    }

    fn send(&mut self, events: &[INPUT]) {
        if events.is_empty() {
            return;
        }
        let sent = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
        if sent as usize == events.len() {
            return;
        }
        // Blocked: usually the lock screen or a UAC prompt is now the input
        // desktop. Follow it (works when running as SYSTEM) and retry once.
        let e = windows::core::Error::from_thread();
        if self.switch_desktop() {
            let rest = &events[sent as usize..];
            if unsafe { SendInput(rest, size_of::<INPUT>() as i32) } as usize == rest.len() {
                return;
            }
        }
        let now = timing::now_ns();
        if now - self.last_error_ns > 5 * SEC {
            self.last_error_ns = now;
            println!("input: SendInput blocked ({}); is a higher-privilege window focused?", e.message().trim());
        }
    }

    fn switch_desktop(&mut self) -> bool {
        let Ok(desk) = (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) })
        else {
            return false;
        };
        if unsafe { SetThreadDesktop(desk) }.is_err() {
            let _ = unsafe { CloseDesktop(desk) };
            return false;
        }
        if let Some(old) = self.desktop.replace(desk) {
            let _ = unsafe { CloseDesktop(old) };
        }
        true
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key(code: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if code & 0x100 != 0 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code & 0xFF,
                dwFlags: KEYBD_EVENT_FLAGS(flags.0),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
