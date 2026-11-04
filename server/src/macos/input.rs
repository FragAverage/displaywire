//! Keyboard and mouse into the Mac through CoreGraphics events, as if typed and clicked here.
//! The client sends Windows set-1 scancodes (its ⌘ arrives as the Windows key) and the whole
//! key/button state each time, like the Windows injector takes it.
//!
//! Needs the Accessibility permission (System Settings › Privacy & Security), asked for at start;
//! without it macOS drops the events.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use proto::stream::{BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, Input, KEY_BITS};

use super::cg;

const SEC: u64 = 1_000_000_000;
/// Everything is released if the client goes quiet this long (it sends its state every 50 ms).
const IDLE_RELEASE_NS: u64 = SEC / 2;
/// Wheel units per notch from the client (Windows' 120), and pixels a notch scrolls here.
const WHEEL_NOTCH: f64 = 120.0;
const PIXELS_PER_NOTCH: f64 = 12.0;

// CGEventType
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
// CGEventField
const MOUSE_CLICK_STATE: u32 = 1;
const MOUSE_BUTTON_NUMBER: u32 = 3;
const MOUSE_DELTA_X: u32 = 4;
const MOUSE_DELTA_Y: u32 = 5;
const KEY_AUTOREPEAT: u32 = 8;
// CGEventFlags
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_CAPS: u64 = 0x0001_0000;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> *const c_void;
    fn CGEventCreateMouseEvent(source: *const c_void, kind: u32, at: cg::CGPoint, button: u32) -> *const c_void;
    fn CGEventCreateScrollWheelEvent(source: *const c_void, units: u32, wheels: u32, wheel1: i32, ...) -> *const c_void;
    fn CGEventCreate(source: *const c_void) -> *const c_void;
    fn CGEventGetLocation(event: *const c_void) -> cg::CGPoint;
    fn CGEventSetFlags(event: *const c_void, flags: u64);
    fn CGEventSetIntegerValueField(event: *const c_void, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: *const c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Which display each stream shows (`CGDirectDisplayID`), so a click lands on the right one.
static STREAM_DISPLAYS: Mutex<Option<HashMap<u8, u32>>> = Mutex::new(None);

pub fn set_stream_display(stream: u8, display: Option<u32>) {
    let mut map = STREAM_DISPLAYS.lock().unwrap();
    let map = map.get_or_insert_with(Default::default);
    match display {
        Some(d) => map.insert(stream, d),
        None => map.remove(&stream),
    };
}

fn stream_display(stream: u8) -> u32 {
    STREAM_DISPLAYS.lock().unwrap().as_ref().and_then(|m| m.get(&stream).copied()).unwrap_or_else(cg::main_display)
}

/// Windows set-1 scancode (`+ 0x100` for E0 keys) → macOS virtual key code.
pub fn mac_key(scancode: u16) -> Option<u16> {
    Some(match scancode {
        0x01 => 0x35, // Escape
        0x02 => 0x12,
        0x03 => 0x13,
        0x04 => 0x14,
        0x05 => 0x15,
        0x06 => 0x17,
        0x07 => 0x16,
        0x08 => 0x1A,
        0x09 => 0x1C,
        0x0A => 0x19,
        0x0B => 0x1D,
        0x0C => 0x1B, // -
        0x0D => 0x18, // =
        0x0E => 0x33, // Backspace
        0x0F => 0x30, // Tab
        0x10 => 0x0C, // Q
        0x11 => 0x0D,
        0x12 => 0x0E,
        0x13 => 0x0F,
        0x14 => 0x11,
        0x15 => 0x10,
        0x16 => 0x20,
        0x17 => 0x22,
        0x18 => 0x1F,
        0x19 => 0x23, // P
        0x1A => 0x21, // [
        0x1B => 0x1E, // ]
        0x1C => 0x24, // Return
        0x1D => 0x3B, // Left control
        0x1E => 0x00, // A
        0x1F => 0x01,
        0x20 => 0x02,
        0x21 => 0x03,
        0x22 => 0x05,
        0x23 => 0x04,
        0x24 => 0x26,
        0x25 => 0x28,
        0x26 => 0x25, // L
        0x27 => 0x29, // ;
        0x28 => 0x27, // '
        0x29 => 0x32, // `
        0x2A => 0x38, // Left shift
        0x2B => 0x2A, // \
        0x2C => 0x06, // Z
        0x2D => 0x07,
        0x2E => 0x08,
        0x2F => 0x09,
        0x30 => 0x0B,
        0x31 => 0x2D,
        0x32 => 0x2E, // M
        0x33 => 0x2B, // ,
        0x34 => 0x2F, // .
        0x35 => 0x2C, // /
        0x36 => 0x3C, // Right shift
        0x37 => 0x43, // Keypad *
        0x38 => 0x3A, // Left alt → left option
        0x39 => 0x31, // Space
        0x3A => 0x39, // Caps lock
        0x3B => 0x7A, // F1
        0x3C => 0x78,
        0x3D => 0x63,
        0x3E => 0x76,
        0x3F => 0x60,
        0x40 => 0x61,
        0x41 => 0x62,
        0x42 => 0x64,
        0x43 => 0x65,
        0x44 => 0x6D, // F10
        0x47 => 0x59, // Keypad 7
        0x48 => 0x5B,
        0x49 => 0x5C,
        0x4A => 0x4E, // Keypad -
        0x4B => 0x56,
        0x4C => 0x57,
        0x4D => 0x58,
        0x4E => 0x45, // Keypad +
        0x4F => 0x53,
        0x50 => 0x54,
        0x51 => 0x55,
        0x52 => 0x52, // Keypad 0
        0x53 => 0x41, // Keypad .
        0x56 => 0x0A, // ISO § / extra key
        0x57 => 0x67, // F11
        0x58 => 0x6F, // F12
        0x59 => 0x51, // Keypad =
        0x64 => 0x69, // F13
        0x65 => 0x6B,
        0x66 => 0x71,
        0x67 => 0x6A,
        0x68 => 0x40,
        0x69 => 0x4F,
        0x6A => 0x50,
        0x6B => 0x5A, // F20
        0x70 => 0x68, // Kana
        0x71 => 0x66, // Eisu
        0x72 => 0x68,
        0x73 => 0x5E, // Ro
        0x7D => 0x5D, // Yen
        0x7E => 0x5F, // Keypad ,
        0x11C => 0x4C, // Keypad enter
        0x11D => 0x3E, // Right control
        0x120 => 0x4A, // Mute
        0x12E => 0x49, // Volume down
        0x130 => 0x48, // Volume up
        0x135 => 0x4B, // Keypad /
        0x138 => 0x3D, // Right alt → right option
        0x145 => 0x47, // Num lock → clear
        0x147 => 0x73, // Home
        0x148 => 0x7E, // Up
        0x149 => 0x74, // Page up
        0x14B => 0x7B, // Left
        0x14D => 0x7C, // Right
        0x14F => 0x77, // End
        0x150 => 0x7D, // Down
        0x151 => 0x79, // Page down
        0x152 => 0x72, // Insert → help
        0x153 => 0x75, // Forward delete
        0x15B => 0x37, // Left Windows → left command (the client's ⌘)
        0x15C => 0x36, // Right Windows → right command
        0x15D => 0x6E, // Menu
        _ => return None,
    })
}

/// The modifier flag a key holds down, if it's a modifier.
fn modifier_flag(scancode: u16) -> u64 {
    match scancode {
        0x2A | 0x36 => FLAG_SHIFT,
        0x1D | 0x11D => FLAG_CONTROL,
        0x38 | 0x138 => FLAG_OPTION,
        0x15B | 0x15C => FLAG_COMMAND,
        _ => 0,
    }
}

pub struct Injector {
    held: Input,
    last_seq: Option<u32>,
    last_input_ns: u64,
    /// Where the pointer was put, in global points (relative motion builds on it).
    at: Option<cg::CGPoint>,
    /// For double-clicks: the last press (button, time, place) and how many in a row.
    last_click: Option<(u8, u64, cg::CGPoint, i64)>,
    wheel_rest: (f64, f64),
    caps: bool,
    warned: bool,
}

impl Injector {
    pub fn new() -> Injector {
        Injector { held: Input::default(), last_seq: None, last_input_ns: 0, at: None, last_click: None, wheel_rest: (0.0, 0.0), caps: false, warned: false }
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
        if !self.warned && !unsafe { AXIsProcessTrusted() } {
            self.warned = true;
            println!("input: needs the Accessibility permission (System Settings › Privacy & Security › Accessibility)");
        }

        // Move first, so a click lands where the pointer now is.
        if let Some((x, y)) = input.absolute {
            let b = cg::bounds(stream_display(input.stream));
            let at = cg::CGPoint {
                x: b.origin.x + x as f64 / 65535.0 * (b.size.width - 1.0),
                y: b.origin.y + y as f64 / 65535.0 * (b.size.height - 1.0),
            };
            self.move_to(at, 0, 0);
        }
        if input.dx != 0 || input.dy != 0 {
            let from = self.at.unwrap_or_else(current_location);
            let at = cg::CGPoint { x: from.x + input.dx as f64, y: from.y + input.dy as f64 };
            self.move_to(at, input.dx as i64, input.dy as i64);
        }
        self.buttons(input.buttons, now_ns);
        if input.wheel != 0 || input.hwheel != 0 {
            self.scroll(input.wheel, input.hwheel);
        }
        // Releases before presses, so a fast A→B roll never shows both down.
        for down in [false, true] {
            for code in 0..KEY_BITS as u16 {
                if input.key(code) == down && self.held.key(code) != down {
                    self.held.set_key(code, down);
                    self.key(code, down, false);
                }
            }
        }
        if input.repeat != 0 && self.held.key(input.repeat) {
            self.key(input.repeat, true, true);
        }
    }

    fn flags(&self) -> u64 {
        let mut flags = if self.caps { FLAG_CAPS } else { 0 };
        for code in [0x2A, 0x36, 0x1D, 0x11D, 0x38, 0x138, 0x15B, 0x15C] {
            if self.held.key(code) {
                flags |= modifier_flag(code);
            }
        }
        flags
    }

    fn key(&mut self, scancode: u16, down: bool, repeat: bool) {
        let Some(key) = mac_key(scancode) else { return };
        if scancode == 0x3A && down && !repeat {
            self.caps = !self.caps;
        }
        let flags = self.flags();
        post(unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), key, down) }, |e| unsafe {
            CGEventSetFlags(e, flags);
            if repeat {
                CGEventSetIntegerValueField(e, KEY_AUTOREPEAT, 1);
            }
        });
    }

    fn move_to(&mut self, at: cg::CGPoint, dx: i64, dy: i64) {
        self.at = Some(at);
        // A move with a button down is a drag, or apps don't see it.
        let b = self.held.buttons;
        let (kind, button) = if b & BUTTON_LEFT != 0 {
            (LEFT_DRAGGED, 0)
        } else if b & BUTTON_RIGHT != 0 {
            (RIGHT_DRAGGED, 1)
        } else if b & (BUTTON_MIDDLE | BUTTON_BACK | BUTTON_FORWARD) != 0 {
            (OTHER_DRAGGED, 2)
        } else {
            (MOVED, 0)
        };
        let flags = self.flags();
        post(unsafe { CGEventCreateMouseEvent(std::ptr::null(), kind, at, button) }, |e| unsafe {
            CGEventSetFlags(e, flags);
            // Games read the deltas, not the position.
            CGEventSetIntegerValueField(e, MOUSE_DELTA_X, dx);
            CGEventSetIntegerValueField(e, MOUSE_DELTA_Y, dy);
        });
    }

    fn buttons(&mut self, buttons: u8, now_ns: u64) {
        const MAP: [(u8, u32, u32, u32); 5] = [
            (BUTTON_LEFT, LEFT_DOWN, LEFT_UP, 0),
            (BUTTON_RIGHT, RIGHT_DOWN, RIGHT_UP, 1),
            (BUTTON_MIDDLE, OTHER_DOWN, OTHER_UP, 2),
            (BUTTON_BACK, OTHER_DOWN, OTHER_UP, 3),
            (BUTTON_FORWARD, OTHER_DOWN, OTHER_UP, 4),
        ];
        let changed = buttons ^ self.held.buttons;
        self.held.buttons = buttons;
        let at = self.at.unwrap_or_else(current_location);
        for (bit, down_kind, up_kind, number) in MAP {
            if changed & bit == 0 {
                continue;
            }
            let down = buttons & bit != 0;
            // A press soon after one of the same button, nearby, counts up (double-, triple-click).
            if down {
                let clicks = match self.last_click {
                    Some((b, t, p, n)) if b == bit && now_ns - t < SEC / 2 && (p.x - at.x).abs() < 5.0 && (p.y - at.y).abs() < 5.0 => n + 1,
                    _ => 1,
                };
                self.last_click = Some((bit, now_ns, at, clicks));
            }
            let clicks = self.last_click.filter(|c| c.0 == bit).map_or(1, |c| c.3);
            let flags = self.flags();
            post(unsafe { CGEventCreateMouseEvent(std::ptr::null(), if down { down_kind } else { up_kind }, at, number) }, |e| unsafe {
                CGEventSetFlags(e, flags);
                CGEventSetIntegerValueField(e, MOUSE_CLICK_STATE, clicks);
                CGEventSetIntegerValueField(e, MOUSE_BUTTON_NUMBER, number as i64);
            });
        }
    }

    fn scroll(&mut self, wheel: i16, hwheel: i16) {
        let (v, h) = (
            self.wheel_rest.0 + wheel as f64 / WHEEL_NOTCH * PIXELS_PER_NOTCH,
            self.wheel_rest.1 - hwheel as f64 / WHEEL_NOTCH * PIXELS_PER_NOTCH,
        );
        let (vi, hi) = (v.trunc(), h.trunc());
        self.wheel_rest = (v - vi, h - hi);
        if vi == 0.0 && hi == 0.0 {
            return;
        }
        // Pixel units (0), two wheels: vertical, then horizontal (positive is up / left).
        post(unsafe { CGEventCreateScrollWheelEvent(std::ptr::null(), 0, 2, vi as i32, hi as i32) }, |_| {});
    }

    fn release_all(&mut self) {
        self.buttons(0, 0);
        for code in 0..KEY_BITS as u16 {
            if self.held.key(code) {
                self.held.set_key(code, false);
                self.key(code, false, false);
            }
        }
    }
}

fn current_location() -> cg::CGPoint {
    unsafe {
        let e = CGEventCreate(std::ptr::null());
        if e.is_null() {
            return cg::CGPoint::default();
        }
        let at = CGEventGetLocation(e);
        CFRelease(e);
        at
    }
}

/// Sets up and posts `event` into the system's event stream (the HID tap), then releases it.
fn post(event: *const c_void, setup: impl FnOnce(*const c_void)) {
    if event.is_null() {
        return;
    }
    setup(event);
    unsafe {
        CGEventPost(0, event);
        CFRelease(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_command_map_to_mac_keys() {
        assert_eq!(mac_key(0x1E), Some(0x00)); // A
        assert_eq!(mac_key(0x2C), Some(0x06)); // Z
        assert_eq!(mac_key(0x0B), Some(0x1D)); // 0
        assert_eq!(mac_key(0x15B), Some(0x37)); // ⌘ back to ⌘
        assert_eq!(mac_key(0x148), Some(0x7E)); // Up
        assert_eq!(mac_key(0x1FF), None);
        // No two scancodes on one Mac key, apart from Kana's two names.
        let mut seen = HashMap::new();
        for code in 0..KEY_BITS as u16 {
            if let Some(k) = mac_key(code)
                && let Some(other) = seen.insert(k, code)
            {
                assert!(k == 0x68, "{code:#x} and {other:#x} both map to {k:#x}");
            }
        }
    }
}
