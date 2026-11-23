//! Keyboard and mouse injection with `CGEventPost` at the HID level, so events look like they
//! came from real devices. Keys arrive as Windows scancodes (the protocol's) and are mapped to
//! Mac virtual key codes by position: the Windows key is ⌘, Alt is ⌥. Modifier state is
//! tracked here and set on every event, since a posted key event doesn't update it.
//! Needs the Accessibility permission (`permissions`).

use proto::stream::{BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, Input, KEY_BITS};

use super::ffi::{self, *};

const SEC: u64 = 1_000_000_000;
/// Everything is released if the client goes quiet this long (it sends its state at least every 50 ms).
const IDLE_RELEASE_NS: u64 = SEC / 2;
/// The client's scroll unit (120 per notch) in pixels, as the Mac client converts the other way.
const PIXELS_PER_NOTCH: f64 = 50.0;
/// Two clicks this close in time and place are a double click.
const DOUBLE_CLICK_NS: u64 = 500_000_000;
const DOUBLE_CLICK_DISTANCE: f64 = 5.0;

pub struct Injector {
    source: CGEventSourceRef,
    held: Input,
    last_seq: Option<u32>,
    last_input_ns: u64,
    /// Caps Lock's toggled state, which a held key doesn't give.
    caps_lock: bool,
    /// Scroll left over from fractions of a pixel.
    scroll_rest: (f64, f64),
    /// The last button pressed: which, when, where, and its click count.
    last_click: Option<(u8, u64, CGPoint, i64)>,
    trusted_checked: bool,
}

// The event source is only used from the control thread.
unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Injector {
        let source = unsafe { CGEventSourceCreate(kCGEventSourceStateHIDSystemState) };
        Injector {
            source,
            held: Input::default(),
            last_seq: None,
            last_input_ns: 0,
            caps_lock: false,
            scroll_rest: (0.0, 0.0),
            last_click: None,
            trusted_checked: false,
        }
    }

    pub fn reset(&mut self) {
        self.release_all();
        self.last_seq = None;
    }

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
        if !self.trusted_checked {
            self.trusted_checked = true;
            if !unsafe { AXIsProcessTrusted() } {
                println!(
                    "input: not allowed to control this Mac; allow DisplayWire Server (or the terminal running it) in System Settings › Privacy & Security › Accessibility, then restart it"
                );
            }
        }
        // Move first, so a click lands where the pointer now is.
        if let Some((x, y)) = input.absolute {
            let at = point_on(super::stream_display(input.stream), x, y);
            self.move_to(at, 0, 0);
        }
        if input.dx != 0 || input.dy != 0 {
            let at = self.location();
            let to = clamp_to_displays(CGPoint { x: at.x + input.dx as f64, y: at.y + input.dy as f64 });
            self.move_to(to, input.dx as i64, input.dy as i64);
        }
        self.buttons(input.buttons, now_ns);
        if input.wheel != 0 || input.hwheel != 0 {
            self.scroll(input.wheel, input.hwheel);
        }
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

    fn location(&self) -> CGPoint {
        unsafe {
            let e = CGEventCreate(self.source);
            if e.is_null() {
                return CGPoint::default();
            }
            let p = CGEventGetLocation(e);
            CFRelease(e);
            p
        }
    }

    fn post(&self, e: CGEventRef) {
        if !e.is_null() {
            unsafe {
                CGEventPost(kCGHIDEventTap, e);
                CFRelease(e);
            }
        }
    }

    /// Modifier flags for what is held now.
    fn flags(&self) -> u64 {
        let k = |c: u16| self.held.key(c);
        let mut f = NX_NONCOALSESCEDMASK;
        for (code, generic, side) in [
            (0x2A, kCGEventFlagMaskShift, NX_DEVICELSHIFTKEYMASK),
            (0x36, kCGEventFlagMaskShift, NX_DEVICERSHIFTKEYMASK),
            (0x1D, kCGEventFlagMaskControl, NX_DEVICELCTLKEYMASK),
            (0x11D, kCGEventFlagMaskControl, NX_DEVICERCTLKEYMASK),
            (0x38, kCGEventFlagMaskAlternate, NX_DEVICELALTKEYMASK),
            (0x138, kCGEventFlagMaskAlternate, NX_DEVICERALTKEYMASK),
            (0x15B, kCGEventFlagMaskCommand, NX_DEVICELCMDKEYMASK),
            (0x15C, kCGEventFlagMaskCommand, NX_DEVICERCMDKEYMASK),
        ] {
            if k(code) {
                f |= generic | side;
            }
        }
        if self.caps_lock {
            f |= kCGEventFlagMaskAlphaShift;
        }
        f
    }

    fn key(&mut self, code: u16, down: bool, repeat: bool) {
        let Some(vk) = mac_key(code) else { return };
        if code == 0x3A && down && !repeat {
            self.caps_lock = !self.caps_lock;
        }
        let modifier = is_modifier(code);
        if modifier && repeat {
            return;
        }
        unsafe {
            let e = CGEventCreateKeyboardEvent(self.source, vk, down);
            if e.is_null() {
                return;
            }
            let mut flags = self.flags();
            if is_keypad(vk) {
                flags |= kCGEventFlagMaskNumericPad;
            }
            if is_fn_key(vk) {
                flags |= kCGEventFlagMaskSecondaryFn;
            }
            if modifier {
                // What a real keyboard sends for a modifier: a flags change, with the new state.
                CGEventSetType(e, kCGEventFlagsChanged);
            }
            CGEventSetFlags(e, flags);
            if repeat {
                CGEventSetIntegerValueField(e, kCGKeyboardEventAutorepeat, 1);
            }
            self.post(e);
        }
    }

    fn move_to(&self, at: CGPoint, dx: i64, dy: i64) {
        let b = self.held.buttons;
        let (ty, button) = if b & BUTTON_LEFT != 0 {
            (kCGEventLeftMouseDragged, 0)
        } else if b & BUTTON_RIGHT != 0 {
            (kCGEventRightMouseDragged, 1)
        } else if b & (BUTTON_MIDDLE | BUTTON_BACK | BUTTON_FORWARD) != 0 {
            (kCGEventOtherMouseDragged, 2)
        } else {
            (kCGEventMouseMoved, 0)
        };
        unsafe {
            let e = CGEventCreateMouseEvent(self.source, ty, at, button);
            if e.is_null() {
                return;
            }
            // Games read the motion, not the position, which stops at the edge of the screen.
            CGEventSetIntegerValueField(e, kCGMouseEventDeltaX, dx);
            CGEventSetIntegerValueField(e, kCGMouseEventDeltaY, dy);
            CGEventSetFlags(e, self.flags());
            self.post(e);
        }
    }

    fn buttons(&mut self, buttons: u8, now_ns: u64) {
        const MAP: [(u8, u32, u32, u32); 5] = [
            (BUTTON_LEFT, kCGEventLeftMouseDown, kCGEventLeftMouseUp, 0),
            (BUTTON_RIGHT, kCGEventRightMouseDown, kCGEventRightMouseUp, 1),
            (BUTTON_MIDDLE, kCGEventOtherMouseDown, kCGEventOtherMouseUp, 2),
            (BUTTON_BACK, kCGEventOtherMouseDown, kCGEventOtherMouseUp, 3),
            (BUTTON_FORWARD, kCGEventOtherMouseDown, kCGEventOtherMouseUp, 4),
        ];
        let changed = buttons ^ self.held.buttons;
        if changed == 0 {
            return;
        }
        let at = self.location();
        for (bit, down_ty, up_ty, number) in MAP {
            if changed & bit == 0 {
                continue;
            }
            let down = buttons & bit != 0;
            // Double and triple clicks: Finder and text fields read the click count, not the timing.
            let clicks = if down {
                let n = match self.last_click {
                    Some((b, t, p, n))
                        if b == bit && now_ns - t < DOUBLE_CLICK_NS && (p.x - at.x).hypot(p.y - at.y) < DOUBLE_CLICK_DISTANCE =>
                    {
                        n + 1
                    }
                    _ => 1,
                };
                self.last_click = Some((bit, now_ns, at, n));
                n
            } else {
                self.last_click.filter(|c| c.0 == bit).map_or(1, |c| c.3)
            };
            unsafe {
                let e = CGEventCreateMouseEvent(self.source, if down { down_ty } else { up_ty }, at, number);
                if e.is_null() {
                    continue;
                }
                CGEventSetIntegerValueField(e, kCGMouseEventButtonNumber, number as i64);
                CGEventSetIntegerValueField(e, kCGMouseEventClickState, clicks);
                CGEventSetFlags(e, self.flags());
                self.post(e);
            }
        }
        self.held.buttons = buttons;
    }

    fn scroll(&mut self, wheel: i16, hwheel: i16) {
        let k = PIXELS_PER_NOTCH / 120.0;
        let (y, x) = (self.scroll_rest.0 + wheel as f64 * k, self.scroll_rest.1 - hwheel as f64 * k);
        let (wy, wx) = (y.trunc(), x.trunc());
        self.scroll_rest = (y - wy, x - wx);
        if wy == 0.0 && wx == 0.0 {
            return;
        }
        unsafe {
            let e = CGEventCreateScrollWheelEvent2(self.source, kCGScrollEventUnitPixel, 2, wy as i32, wx as i32, 0);
            if !e.is_null() {
                CGEventSetFlags(e, self.flags());
                self.post(e);
            }
        }
    }

    fn release_all(&mut self) {
        let now = timing::now_ns();
        self.buttons(0, now);
        for code in 0..KEY_BITS as u16 {
            if self.held.key(code) {
                self.held.set_key(code, false);
                self.key(code, false, false);
            }
        }
        self.held = Input::default();
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        if !self.source.is_null() {
            unsafe { CFRelease(self.source) };
        }
    }
}

/// 0..=65535 across a display → global screen coordinates (points).
fn point_on(display: CGDirectDisplayID, x: u16, y: u16) -> CGPoint {
    let b = unsafe { CGDisplayBounds(display) };
    CGPoint {
        x: b.origin.x + x as f64 * (b.size.width - 1.0).max(1.0) / 65535.0,
        y: b.origin.y + y as f64 * (b.size.height - 1.0).max(1.0) / 65535.0,
    }
}

/// Keeps relative motion on some screen.
fn clamp_to_displays(p: CGPoint) -> CGPoint {
    let rects: Vec<CGRect> = ffi::displays().into_iter().map(|d| unsafe { CGDisplayBounds(d) }).collect();
    let inside = |r: &CGRect| p.x >= r.origin.x && p.y >= r.origin.y && p.x < r.origin.x + r.size.width && p.y < r.origin.y + r.size.height;
    if rects.iter().any(inside) {
        return p;
    }
    // Off every screen: the nearest point of the nearest one.
    rects
        .iter()
        .map(|r| CGPoint {
            x: p.x.clamp(r.origin.x, r.origin.x + r.size.width - 1.0),
            y: p.y.clamp(r.origin.y, r.origin.y + r.size.height - 1.0),
        })
        .min_by(|a, b| (a.x - p.x).hypot(a.y - p.y).total_cmp(&(b.x - p.x).hypot(b.y - p.y)))
        .unwrap_or(p)
}

fn is_modifier(code: u16) -> bool {
    matches!(code, 0x2A | 0x36 | 0x1D | 0x11D | 0x38 | 0x138 | 0x15B | 0x15C | 0x3A)
}

fn is_keypad(vk: u16) -> bool {
    matches!(vk, 0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51..=0x59 | 0x5B | 0x5C | 0x5F | 0x7B..=0x7E)
}

/// Keys a Mac keyboard sends with the Fn flag: arrows, the navigation block and F-keys.
fn is_fn_key(vk: u16) -> bool {
    matches!(vk, 0x7B..=0x7E | 0x72..=0x77 | 0x79 | 0x60..=0x65 | 0x67 | 0x69..=0x6B | 0x6D | 0x6F | 0x71 | 0x7A | 0x78 | 0x40 | 0x4F | 0x50 | 0x5A)
}

/// Windows set-1 scancode (`0x100` = E0-extended) → Mac virtual key code, by physical position.
pub fn mac_key(code: u16) -> Option<u16> {
    Some(match code {
        0x01 => 0x35, // Escape
        0x02 => 0x12, // 1
        0x03 => 0x13,
        0x04 => 0x14,
        0x05 => 0x15,
        0x06 => 0x17, // 5
        0x07 => 0x16, // 6
        0x08 => 0x1A, // 7
        0x09 => 0x1C, // 8
        0x0A => 0x19, // 9
        0x0B => 0x1D, // 0
        0x0C => 0x1B, // -
        0x0D => 0x18, // =
        0x0E => 0x33, // Backspace
        0x0F => 0x30, // Tab
        0x10 => 0x0C, // Q
        0x11 => 0x0D, // W
        0x12 => 0x0E, // E
        0x13 => 0x0F, // R
        0x14 => 0x11, // T
        0x15 => 0x10, // Y
        0x16 => 0x20, // U
        0x17 => 0x22, // I
        0x18 => 0x1F, // O
        0x19 => 0x23, // P
        0x1A => 0x21, // [
        0x1B => 0x1E, // ]
        0x1C => 0x24, // Return
        0x1D => 0x3B, // Left Control
        0x1E => 0x00, // A
        0x1F => 0x01, // S
        0x20 => 0x02, // D
        0x21 => 0x03, // F
        0x22 => 0x05, // G
        0x23 => 0x04, // H
        0x24 => 0x26, // J
        0x25 => 0x28, // K
        0x26 => 0x25, // L
        0x27 => 0x29, // ;
        0x28 => 0x27, // '
        0x29 => 0x32, // `
        0x2A => 0x38, // Left Shift
        0x2B => 0x2A, // Backslash
        0x2C => 0x06, // Z
        0x2D => 0x07, // X
        0x2E => 0x08, // C
        0x2F => 0x09, // V
        0x30 => 0x0B, // B
        0x31 => 0x2D, // N
        0x32 => 0x2E, // M
        0x33 => 0x2B, // ,
        0x34 => 0x2F, // .
        0x35 => 0x2C, // /
        0x36 => 0x3C, // Right Shift
        0x37 => 0x43, // Keypad *
        0x38 => 0x3A, // Left Alt → Option
        0x39 => 0x31, // Space
        0x3A => 0x39, // Caps Lock
        0x3B => 0x7A, // F1
        0x3C => 0x78, // F2
        0x3D => 0x63, // F3
        0x3E => 0x76, // F4
        0x3F => 0x60, // F5
        0x40 => 0x61, // F6
        0x41 => 0x62, // F7
        0x42 => 0x64, // F8
        0x43 => 0x65, // F9
        0x44 => 0x6D, // F10
        0x46 => 0x6B, // Scroll Lock → F14
        0x47 => 0x59, // Keypad 7
        0x48 => 0x5B, // Keypad 8
        0x49 => 0x5C, // Keypad 9
        0x4A => 0x4E, // Keypad -
        0x4B => 0x56, // Keypad 4
        0x4C => 0x57, // Keypad 5
        0x4D => 0x58, // Keypad 6
        0x4E => 0x45, // Keypad +
        0x4F => 0x53, // Keypad 1
        0x50 => 0x54, // Keypad 2
        0x51 => 0x55, // Keypad 3
        0x52 => 0x52, // Keypad 0
        0x53 => 0x41, // Keypad .
        0x56 => 0x0A, // ISO key left of Z (§ on Mac ISO keyboards)
        0x57 => 0x67, // F11
        0x58 => 0x6F, // F12
        0x59 => 0x51, // Keypad =
        0x64 => 0x69, // F13
        0x65 => 0x6B, // F14
        0x66 => 0x71, // F15
        0x67 => 0x6A, // F16
        0x68 => 0x40, // F17
        0x69 => 0x4F, // F18
        0x6A => 0x50, // F19
        0x6B => 0x5A, // F20
        0x70 => 0x68, // Kana
        0x71 => 0x66, // Lang2 → Eisu
        0x72 => 0x68, // Lang1 → Kana
        0x73 => 0x5E, // Ro → JIS underscore
        0x7D => 0x5D, // Yen
        0x7E => 0x5F, // Keypad , (JIS)
        0x11C => 0x4C, // Keypad Enter
        0x11D => 0x3E, // Right Control
        0x120 => 0x4A, // Mute
        0x12E => 0x49, // Volume Down
        0x130 => 0x48, // Volume Up
        0x135 => 0x4B, // Keypad /
        0x137 => 0x69, // Print Screen → F13
        0x138 => 0x3D, // Right Alt → Right Option
        0x145 => 0x47, // Num Lock → Keypad Clear
        0x147 => 0x73, // Home
        0x148 => 0x7E, // Up
        0x149 => 0x74, // Page Up
        0x14B => 0x7B, // Left
        0x14D => 0x7C, // Right
        0x14F => 0x77, // End
        0x150 => 0x7D, // Down
        0x151 => 0x79, // Page Down
        0x152 => 0x72, // Insert → Help
        0x153 => 0x75, // Delete → Forward Delete
        0x15B => 0x37, // Left Windows → Command
        0x15C => 0x36, // Right Windows → Right Command
        0x15D => 0x6E, // Menu
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::mac_key;

    #[test]
    fn letters_and_modifiers_map_by_position() {
        assert_eq!(mac_key(0x1E), Some(0x00)); // A
        assert_eq!(mac_key(0x10), Some(0x0C)); // Q
        assert_eq!(mac_key(0x15B), Some(0x37)); // Windows → ⌘
        assert_eq!(mac_key(0x38), Some(0x3A)); // Alt → ⌥
        assert_eq!(mac_key(0x1D), Some(0x3B)); // Ctrl → ⌃
        assert_eq!(mac_key(0x148), Some(0x7E)); // Up
        assert_eq!(mac_key(0x00), None);
    }

    #[test]
    fn no_two_keys_share_a_code() {
        let mut seen = std::collections::HashMap::new();
        for code in 0..0x200u16 {
            if let Some(vk) = mac_key(code) {
                // Scroll Lock and F14, Print Screen and F13, Kana and Lang1 are the same key on a Mac.
                if let Some(prev) = seen.insert(vk, code) {
                    assert!(matches!((prev, code), (0x46, 0x65) | (0x64, 0x137) | (0x70, 0x72)), "{prev:#x} and {code:#x} both map to {vk:#x}");
                }
            }
        }
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Moves the real pointer: run by hand with `cargo test -p displaywire-server -- --ignored pointer_moves`
    /// (needs the Accessibility permission for the terminal).
    #[test]
    #[ignore]
    fn pointer_moves() {
        let mut inj = Injector::new();
        let before = inj.location();
        let mut input = Input { seq: 1, absolute: Some((32768, 32768)), ..Input::default() };
        inj.apply(&input, timing::now_ns());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let b = unsafe { CGDisplayBounds(super::super::stream_display(0)) };
        let at = inj.location();
        assert!((at.x - (b.origin.x + b.size.width / 2.0)).abs() < 2.0 && (at.y - (b.origin.y + b.size.height / 2.0)).abs() < 2.0, "{at:?}");
        // Relative motion.
        input = Input { seq: 2, dx: 40, dy: -30, ..Input::default() };
        inj.apply(&input, timing::now_ns());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let moved = inj.location();
        assert!((moved.x - at.x - 40.0).abs() < 2.0 && (moved.y - at.y + 30.0).abs() < 2.0, "{moved:?}");
        unsafe { CGWarpMouseCursorPosition(before) };
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceFlagsState(state: i32) -> u64;
    }

    /// Holds Shift (harmless anywhere) and checks the system sees it down, then up.
    #[test]
    #[ignore]
    fn keys_reach_the_system() {
        let mut inj = Injector::new();
        let mut input = Input { seq: 1, ..Input::default() };
        input.set_key(0x2A, true);
        inj.apply(&input, timing::now_ns());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (key, flags) = unsafe { (CGEventSourceKeyState(kCGEventSourceStateHIDSystemState, 0x38), CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState)) };
        assert!(key && flags & kCGEventFlagMaskShift != 0, "shift isn't down");
        input.seq = 2;
        input.set_key(0x2A, false);
        inj.apply(&input, timing::now_ns());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let flags = unsafe { CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState) | CGEventSourceFlagsState(0) };
        assert!(flags & kCGEventFlagMaskShift == 0, "shift is stuck");
    }
}
