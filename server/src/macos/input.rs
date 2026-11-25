//! Keyboard and mouse injection with `CGEventPost` at the HID level, so events look like they
//! came from real devices. Keys arrive as Windows scancodes (the protocol's) and are mapped to
//! Mac virtual key codes by position: the Windows key is ⌘, Alt is ⌥. Modifier state is
//! tracked here and set on every event, since a posted key event doesn't update it.
//! Needs the Accessibility permission (`permissions`).

use proto::stream::{BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, Input, KEY_BITS};

use super::ffi::{self, *};

const SEC: u64 = 1_000_000_000;

/// PC shortcuts: Ctrl works as ⌘ (and the Windows key as Control), and Windows' text keys do
/// what they do there: Ctrl+Backspace/Delete/←/→ by word, Home/End to the line's ends,
/// Ctrl+Home/End to the document's. On unless switched off in the menu bar, which a Mac
/// client (whose ⌘ arrives as the Windows key) wants.
const CMD: u64 = kCGEventFlagMaskCommand | NX_DEVICELCMDKEYMASK;
const OPT: u64 = kCGEventFlagMaskAlternate | NX_DEVICELALTKEYMASK;
const SHIFT: u64 = kCGEventFlagMaskShift | NX_DEVICELSHIFTKEYMASK;
const CTRL: u64 = kCGEventFlagMaskControl | NX_DEVICELCTLKEYMASK;

static PC_SHORTCUTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn pc_shortcuts_off_path() -> std::path::PathBuf {
    crate::auth::default_dir().join("pc-shortcuts-off")
}

pub fn load_pc_shortcuts() {
    PC_SHORTCUTS.store(!pc_shortcuts_off_path().exists(), std::sync::atomic::Ordering::Relaxed);
}

pub fn pc_shortcuts() -> bool {
    PC_SHORTCUTS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_pc_shortcuts(on: bool) {
    PC_SHORTCUTS.store(on, std::sync::atomic::Ordering::Relaxed);
    let path = pc_shortcuts_off_path();
    let _ = if on {
        std::fs::remove_file(&path)
    } else {
        std::fs::create_dir_all(path.parent().unwrap()).and_then(|()| std::fs::write(&path, b"off\n"))
    };
}
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
    /// Keys sent as something else (PC shortcuts): scancode → the Mac key and modifiers it went
    /// down as, so it repeats and comes up the same way.
    translated: std::collections::HashMap<u16, (u16, u64)>,
    /// Alt+Tab is showing the app switcher: ⌘ is held for it until Alt comes up.
    alt_tab: bool,
    /// The Windows key is down and nothing else has been pressed with it.
    win_tap: bool,
    /// Keys that went down as media keys: scancode → `NX_KEYTYPE_*`.
    media_down: std::collections::HashMap<u16, u32>,
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
            translated: std::collections::HashMap::new(),
            alt_tab: false,
            win_tap: false,
            media_down: std::collections::HashMap::new(),
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
        let (ctrl, win) = if pc_shortcuts() { ((0x15B, 0x15C), (0x1D, 0x11D)) } else { ((0x1D, 0x11D), (0x15B, 0x15C)) };
        for (code, generic, side) in [
            (0x2A, kCGEventFlagMaskShift, NX_DEVICELSHIFTKEYMASK),
            (0x36, kCGEventFlagMaskShift, NX_DEVICERSHIFTKEYMASK),
            (ctrl.0, kCGEventFlagMaskControl, NX_DEVICELCTLKEYMASK),
            (ctrl.1, kCGEventFlagMaskControl, NX_DEVICERCTLKEYMASK),
            (0x38, kCGEventFlagMaskAlternate, NX_DEVICELALTKEYMASK),
            (0x138, kCGEventFlagMaskAlternate, NX_DEVICERALTKEYMASK),
            (win.0, kCGEventFlagMaskCommand, NX_DEVICELCMDKEYMASK),
            (win.1, kCGEventFlagMaskCommand, NX_DEVICERCMDKEYMASK),
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
        let Some(mut vk) = mac_key(code) else { return };
        if code == 0x3A && down && !repeat {
            self.caps_lock = !self.caps_lock;
        }
        let modifier = is_modifier(code);
        if modifier && repeat {
            return;
        }
        let pc = pc_shortcuts();
        // Exactly these flags, for a key sent as another (PC shortcuts).
        let mut exact: Option<u64> = None;
        if pc {
            vk = match code {
                0x1D => 0x37,  // Left Control → Command
                0x11D => 0x36, // Right Control → Right Command
                0x15B => 0x3B, // Left Windows → Control
                0x15C => 0x3E, // Right Windows → Right Control
                _ => vk,
            };
            if down && !matches!(code, 0x15B | 0x15C) {
                self.win_tap = false;
            }
            let alt = self.held.key(0x38) || self.held.key(0x138);
            let shift = if self.held.key(0x2A) || self.held.key(0x36) { SHIFT } else { 0 };
            // Alt+Tab: ⌘Tab, with ⌘ held until Alt comes up so the switcher stays open.
            if code == 0x0F && (self.alt_tab || (down && alt && !self.ctrl_held() && !self.win_held())) {
                if down && !self.alt_tab {
                    self.alt_tab = true;
                    self.post_key(0x37, true, self.base_flags() | CMD, true);
                }
                self.post_key(0x30, down, self.base_flags() | CMD | shift, false);
                return;
            }
            if !modifier && let Some(nx) = self.media_key(code, down, alt || shift != 0) {
                if !repeat {
                    post_media(nx, down);
                }
                return;
            }
            if !modifier {
                if !down || repeat {
                    if let Some(&(v, f)) = self.translated.get(&code) {
                        (vk, exact) = (v, Some(self.base_flags() | f));
                    }
                    if !down {
                        self.translated.remove(&code);
                    }
                } else if let Some((v, f)) = self.pc_combo(code, vk) {
                    self.translated.insert(code, (v, f));
                    (vk, exact) = (v, Some(self.base_flags() | f));
                }
            }
        }
        let mut flags = exact.unwrap_or_else(|| self.flags());
        if is_keypad(vk) {
            flags |= kCGEventFlagMaskNumericPad;
        }
        self.post_key_repeat(vk, down, flags, modifier, repeat);
        if pc && modifier {
            match (code, down) {
                (0x38 | 0x138, false) if self.alt_tab => {
                    // Alt up: let go of ⌘, which picks the app.
                    self.alt_tab = false;
                    self.post_key(0x37, false, self.flags(), true);
                }
                (0x15B | 0x15C, true) => self.win_tap = true,
                (0x15B | 0x15C, false) if std::mem::take(&mut self.win_tap) => {
                    // The Windows key on its own opens Start: Spotlight here.
                    let f = self.base_flags() | CMD;
                    self.post_key(0x31, true, f, false);
                    self.post_key(0x31, false, f, false);
                }
                _ => {}
            }
        }
    }

    /// A key that is a media key on the Mac: the PC's own media keys, and (with PC shortcuts,
    /// pressed on their own) the F-keys that are media keys on a Mac's top row. The
    /// `NX_KEYTYPE_*`; a key comes up as what it went down as.
    fn media_key(&mut self, code: u16, down: bool, alt_or_shift: bool) -> Option<u32> {
        if !down {
            return self.media_down.remove(&code);
        }
        if let Some(&nx) = self.media_down.get(&code) {
            return Some(nx);
        }
        let plain = !alt_or_shift && !self.ctrl_held() && !self.win_held();
        let nx = match code {
            0x120 => 7,         // Mute
            0x12E => 1,         // Volume down
            0x130 => 0,         // Volume up
            0x122 => 16,        // Play/pause
            0x110 => 18,        // Previous track
            0x119 => 17,        // Next track
            0x3B if plain => 3, // F1: brightness down
            0x3C if plain && !finder_files_focused() => 2, // F2: brightness up (renames in Finder)
            0x41 if plain => 18, // F7: previous
            0x42 if plain => 16, // F8: play/pause
            0x43 if plain => 17, // F9: next
            0x44 if plain => 7,  // F10: mute
            0x57 if plain => 1,  // F11: volume down
            0x58 if plain => 0,  // F12: volume up
            _ => return None,
        };
        self.media_down.insert(code, nx);
        Some(nx)
    }

    fn ctrl_held(&self) -> bool {
        self.held.key(0x1D) || self.held.key(0x11D)
    }

    fn win_held(&self) -> bool {
        self.held.key(0x15B) || self.held.key(0x15C)
    }

    /// Flags no modifier contributes to.
    fn base_flags(&self) -> u64 {
        NX_NONCOALSESCEDMASK | if self.caps_lock { kCGEventFlagMaskAlphaShift } else { 0 }
    }

    /// A Windows shortcut as the Mac one that does the same: the Mac key and the modifiers to
    /// send with it (and nothing else held), or `None` to send the key as it is.
    fn pc_combo(&self, code: u16, vk: u16) -> Option<(u16, u64)> {
        let (c, a, w) = (self.ctrl_held(), self.held.key(0x38) || self.held.key(0x138), self.win_held());
        let s = self.held.key(0x2A) || self.held.key(0x36);
        let sh = if s { SHIFT } else { 0 };
        // Explorer's keys in Finder's file list (not while a name is being typed).
        let finder = matches!(code, 0x153 | 0x1C | 0x3C | 0x0E | 0x148) && finder_files_focused();
        Some(match code {
            0x153 if finder && c && s => (0x33, CMD | SHIFT), // Ctrl+Shift+Delete: empty the Bin
            0x153 if finder && s => (0x33, CMD | OPT),       // Shift+Delete: delete at once
            0x153 if finder => (0x33, CMD),                  // Delete: to the Bin
            0x1C if finder && a => (0x22, CMD),              // Alt+Enter: Get Info
            0x1C if finder && !c => (0x7D, CMD),             // Enter: open
            0x3C if finder && !c && !a => (0x24, 0),         // F2: rename
            0x0E if finder && !c && !a => (0x21, CMD),       // Backspace: back
            0x148 if finder && a => (0x7E, CMD),             // Alt+↑: up a folder
            // The Windows key with a letter.
            0x20 if w => (0x63, CMD),               // Win+D: show the desktop (⌘F3)
            0x12 if w => (0x31, CMD | OPT),         // Win+E: a Finder window (⌥⌘Space)
            0x26 if w => (0x0C, CMD | CTRL),        // Win+L: lock (⌃⌘Q)
            0x13 if w => (0x31, CMD),               // Win+R: Spotlight
            0x1F if w && s => (0x15, CMD | SHIFT),  // Win+Shift+S: screenshot of an area (⇧⌘4)
            0x0F if w => (0x7E, CTRL),              // Win+Tab: Mission Control (⌃↑)
            0x137 => (0x14, CMD | SHIFT),           // Print Screen: the whole screen (⇧⌘3)
            0x3E if a && !c => (0x0C, CMD),         // Alt+F4: quit (⌘Q)
            // F3 to F5 on their own: what a Mac's top row (and Windows' F5) does.
            0x3D if !c && !a && !s && !w => (0x7E, CTRL), // F3: Mission Control (⌃↑)
            0x3E if !c && !a && !s && !w => (0x31, CMD),  // F4: Spotlight
            0x3F if !c && !a && !s && !w => (0x0F, CMD),  // F5: refresh (⌘R)
            0x01 if c && s => (0x35, CMD | OPT),    // Ctrl+Shift+Esc: Force Quit (⌥⌘Esc)
            0x15 if c && !s && !a => (0x06, CMD | SHIFT), // Ctrl+Y: redo (⇧⌘Z)
            0x14B if a && !c => (0x21, CMD),        // Alt+←: back (⌘[)
            0x14D if a && !c => (0x1E, CMD),        // Alt+→: forward (⌘])
            // By word: ⌥ where Windows uses Ctrl.
            0x0E | 0x153 | 0x14B | 0x14D if c && !a => (vk, OPT | sh),
            0x147 if c => (0x7E, CMD | sh), // Ctrl+Home: the top
            0x147 => (0x7B, CMD | sh),      // Home: the line's start
            0x14F if c => (0x7D, CMD | sh), // Ctrl+End: the end
            0x14F => (0x7C, CMD | sh),      // End: the line's end
            _ => return None,
        })
    }

    fn post_key(&self, vk: u16, down: bool, flags: u64, flags_changed: bool) {
        self.post_key_repeat(vk, down, flags, flags_changed, false);
    }

    fn post_key_repeat(&self, vk: u16, down: bool, mut flags: u64, flags_changed: bool, repeat: bool) {
        unsafe {
            let e = CGEventCreateKeyboardEvent(self.source, vk, down);
            if e.is_null() {
                return;
            }
            if is_fn_key(vk) {
                flags |= kCGEventFlagMaskSecondaryFn;
            }
            if flags_changed {
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

/// A media key (volume, brightness, play): the system-defined event a Mac keyboard's top row sends.
fn post_media(nx: u32, down: bool) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    objc2::rc::autoreleasepool(|_| {
        let state: usize = if down { 0xA } else { 0xB };
        let e = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::SystemDefined,
            objc2_foundation::NSPoint::new(0.0, 0.0),
            NSEventModifierFlags(state << 8),
            0.0,
            0,
            None,
            8,
            ((nx as isize) << 16) | ((state as isize) << 8),
            -1,
        );
        if let Some(cg) = e.and_then(|e| e.CGEvent()) {
            // The NSEvent owns it: posted, not released.
            unsafe { CGEventPost(kCGHIDEventTap, &*cg as *const _ as CGEventRef) };
        }
    });
}

/// Whether Finder is in front with its files (not a name being typed or the search field) focused.
fn finder_files_focused() -> bool {
    use objc2_app_kit::NSWorkspace;
    let finder = objc2::rc::autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|a| a.bundleIdentifier())
            .is_some_and(|id| id.to_string() == "com.apple.finder")
    });
    if !finder {
        return false;
    }
    unsafe {
        let Some(system) = ffi::Cf::owned(AXUIElementCreateSystemWide()) else { return true };
        let focused_attr = ffi::Cf::string("AXFocusedUIElement");
        let mut focused: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(system.0, focused_attr.0, &mut focused) != 0 || focused.is_null() {
            return true;
        }
        let focused = ffi::Cf(focused);
        let role_attr = ffi::Cf::string("AXRole");
        let mut role: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(focused.0, role_attr.0, &mut role) != 0 || role.is_null() {
            return true;
        }
        let role = ffi::Cf(role);
        let name = (*(role.0 as *const objc2_foundation::NSString)).to_string();
        !matches!(name.as_str(), "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
    }
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

    /// PC shortcuts: the PC's Ctrl is ⌘ on the Mac. Holds it for 50 ms (nothing else pressed).
    #[test]
    #[ignore]
    fn pc_ctrl_is_command() {
        super::set_pc_shortcuts(true);
        let mut inj = Injector::new();
        let mut input = Input { seq: 1, ..Input::default() };
        input.set_key(0x1D, true);
        inj.apply(&input, timing::now_ns());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (key, flags) = unsafe { (CGEventSourceKeyState(kCGEventSourceStateHIDSystemState, 0x37), CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState)) };
        input.seq = 2;
        input.set_key(0x1D, false);
        inj.apply(&input, timing::now_ns());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(key && flags & kCGEventFlagMaskCommand != 0 && flags & kCGEventFlagMaskControl == 0, "Ctrl didn't arrive as ⌘");
        let flags = unsafe { CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState) };
        assert!(flags & kCGEventFlagMaskCommand == 0, "⌘ is stuck");
    }

    /// F12 (PC shortcuts) turns the volume up, as on a Mac keyboard; F11 back down.
    #[test]
    #[ignore]
    fn f12_is_volume_up() {
        let vol = || {
            let o = std::process::Command::new("osascript").args(["-e", "output volume of (get volume settings)"]).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim().parse::<i32>().unwrap()
        };
        super::set_pc_shortcuts(true);
        let before = vol();
        let mut inj = Injector::new();
        let mut input = Input { seq: 1, ..Input::default() };
        let tap = |inj: &mut Injector, input: &mut Input, code: u16| {
            for down in [true, false] {
                input.seq += 1;
                input.set_key(code, down);
                inj.apply(input, timing::now_ns());
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        };
        let code = if before >= 90 { 0x57 } else { 0x58 };
        tap(&mut inj, &mut input, code);
        let after = vol();
        tap(&mut inj, &mut input, if code == 0x58 { 0x57 } else { 0x58 });
        assert_ne!(before, after, "the volume didn't change");
    }
}
