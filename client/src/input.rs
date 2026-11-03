//! Keyboard and mouse → server. Every datagram carries the full key and
//! button state, sent on each change and re-sent every `HEARTBEAT_NS`, so a
//! lost packet can't leave a key held down on the server.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use proto::stream::{BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT, Input, Msg};
use timing::now_ns;
use winit::event::MouseButton;
use winit::keyboard::KeyCode;

use crate::link::Link;

const HEARTBEAT_NS: u64 = 50_000_000;
/// Trackpad scrolling: pixels per 120-unit wheel notch.
const PIXELS_PER_NOTCH: f64 = 50.0;

pub struct Sender {
    link: Arc<Link>,
    state: Mutex<State>,
    iso_keyboard: bool,
    /// Whether the stream window has focus (gamepads read it too).
    pub focused: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    msg: Input,
    /// Sub-unit remainders of motion and wheel deltas.
    rest: [f64; 4],
    last_sent_ns: u64,
}

impl Sender {
    pub fn new(link: Arc<Link>) -> Sender {
        Sender { link, state: Mutex::default(), iso_keyboard: iso_keyboard(), focused: Arc::new(AtomicBool::new(true)) }
    }

    /// Returns false for keys Windows has no scancode for.
    pub fn key(&self, code: KeyCode, down: bool, repeat: bool) -> bool {
        let Some(sc) = scancode(code, self.iso_keyboard) else { return false };
        self.update(|m| {
            if repeat {
                m.repeat = sc;
            }
            m.set_key(sc, down);
        });
        true
    }

    pub fn button(&self, button: MouseButton, down: bool) {
        let bit = match button {
            MouseButton::Left => BUTTON_LEFT,
            MouseButton::Right => BUTTON_RIGHT,
            MouseButton::Middle => BUTTON_MIDDLE,
            MouseButton::Back => BUTTON_BACK,
            MouseButton::Forward => BUTTON_FORWARD,
            MouseButton::Other(_) => return,
        };
        self.update(|m| if down { m.buttons |= bit } else { m.buttons &= !bit });
    }

    /// Pointer position as a fraction (0..=1) of the streamed screen.
    pub fn absolute(&self, x: f64, y: f64) {
        let norm = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
        self.update(|m| m.absolute = Some((norm(x), norm(y))));
    }

    /// Relative motion in mouse counts.
    pub fn motion(&self, dx: f64, dy: f64) {
        self.accumulate([dx, dy, 0.0, 0.0]);
    }

    /// Scroll in wheel lines (vertical positive = away from the user).
    pub fn wheel_lines(&self, x: f64, y: f64) {
        self.accumulate([0.0, 0.0, y * 120.0, x * 120.0]);
    }

    /// Scroll in trackpad pixels.
    pub fn wheel_pixels(&self, x: f64, y: f64) {
        let k = 120.0 / PIXELS_PER_NOTCH;
        self.accumulate([0.0, 0.0, y * k, x * k]);
    }

    pub fn release_all(&self) {
        self.update(|m| {
            m.keys = Input::default().keys;
            m.buttons = 0;
        });
    }

    /// Re-sends the current state if nothing went out recently.
    pub fn heartbeat(&self) {
        let mut s = self.state.lock().unwrap();
        if now_ns() - s.last_sent_ns >= HEARTBEAT_NS {
            self.send(&mut s);
        }
    }

    fn accumulate(&self, delta: [f64; 4]) {
        let mut s = self.state.lock().unwrap();
        let mut whole = [0i16; 4];
        for i in 0..4 {
            let v = s.rest[i] + delta[i];
            let w = v.trunc().clamp(i16::MIN as f64, i16::MAX as f64);
            whole[i] = w as i16;
            s.rest[i] = v - w;
        }
        if whole == [0; 4] {
            return;
        }
        let m = &mut s.msg;
        [m.dx, m.dy, m.wheel, m.hwheel] = whole;
        self.send(&mut s);
    }

    fn update(&self, f: impl FnOnce(&mut Input)) {
        let mut s = self.state.lock().unwrap();
        f(&mut s.msg);
        self.send(&mut s);
    }

    fn send(&self, s: &mut State) {
        s.msg.seq = s.msg.seq.wrapping_add(1);
        let mut buf = [0u8; proto::MAX_DATAGRAM];
        let n = Msg::Input(s.msg).encode(&mut buf);
        self.link.send(&buf[..n]);
        s.last_sent_ns = now_ns();
        // Deltas and one-shot fields go out once; the key/button state persists.
        let m = &mut s.msg;
        (m.dx, m.dy, m.wheel, m.hwheel, m.absolute, m.repeat) = (0, 0, 0, 0, None, 0);
    }
}

/// ISO Mac keyboards report the key left of `1` and the key left of `Z`
/// swapped relative to their physical position.
fn iso_keyboard() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(keyboard_type: i16) -> u32;
    }
    const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");
    unsafe { KBGetLayoutType(LMGetKbdType() as i16) == KEYBOARD_ISO }
}

/// Windows set-1 scancode for a physical key; `0x100` marks E0-extended keys.
fn scancode(code: KeyCode, iso: bool) -> Option<u16> {
    use KeyCode::*;
    let code = match code {
        Backquote if iso => IntlBackslash,
        IntlBackslash if iso => Backquote,
        c => c,
    };
    Some(match code {
        Escape => 0x01,
        Digit1 => 0x02,
        Digit2 => 0x03,
        Digit3 => 0x04,
        Digit4 => 0x05,
        Digit5 => 0x06,
        Digit6 => 0x07,
        Digit7 => 0x08,
        Digit8 => 0x09,
        Digit9 => 0x0A,
        Digit0 => 0x0B,
        Minus => 0x0C,
        Equal => 0x0D,
        Backspace => 0x0E,
        Tab => 0x0F,
        KeyQ => 0x10,
        KeyW => 0x11,
        KeyE => 0x12,
        KeyR => 0x13,
        KeyT => 0x14,
        KeyY => 0x15,
        KeyU => 0x16,
        KeyI => 0x17,
        KeyO => 0x18,
        KeyP => 0x19,
        BracketLeft => 0x1A,
        BracketRight => 0x1B,
        Enter => 0x1C,
        ControlLeft => 0x1D,
        KeyA => 0x1E,
        KeyS => 0x1F,
        KeyD => 0x20,
        KeyF => 0x21,
        KeyG => 0x22,
        KeyH => 0x23,
        KeyJ => 0x24,
        KeyK => 0x25,
        KeyL => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backquote => 0x29,
        ShiftLeft => 0x2A,
        Backslash => 0x2B,
        KeyZ => 0x2C,
        KeyX => 0x2D,
        KeyC => 0x2E,
        KeyV => 0x2F,
        KeyB => 0x30,
        KeyN => 0x31,
        KeyM => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        ShiftRight => 0x36,
        NumpadMultiply => 0x37,
        AltLeft => 0x38,
        Space => 0x39,
        CapsLock => 0x3A,
        F1 => 0x3B,
        F2 => 0x3C,
        F3 => 0x3D,
        F4 => 0x3E,
        F5 => 0x3F,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        ScrollLock => 0x46,
        Numpad7 => 0x47,
        Numpad8 => 0x48,
        Numpad9 => 0x49,
        NumpadSubtract => 0x4A,
        Numpad4 => 0x4B,
        Numpad5 => 0x4C,
        Numpad6 => 0x4D,
        NumpadAdd => 0x4E,
        Numpad1 => 0x4F,
        Numpad2 => 0x50,
        Numpad3 => 0x51,
        Numpad0 => 0x52,
        NumpadDecimal => 0x53,
        IntlBackslash => 0x56,
        F11 => 0x57,
        F12 => 0x58,
        NumpadEqual => 0x59,
        F13 => 0x64,
        F14 => 0x65,
        F15 => 0x66,
        F16 => 0x67,
        F17 => 0x68,
        F18 => 0x69,
        F19 => 0x6A,
        F20 => 0x6B,
        KanaMode => 0x70,
        Lang2 => 0x71,
        Lang1 => 0x72,
        IntlRo => 0x73,
        IntlYen => 0x7D,
        NumpadComma => 0x7E,
        MediaTrackPrevious => 0x110,
        MediaTrackNext => 0x119,
        NumpadEnter => 0x11C,
        ControlRight => 0x11D,
        AudioVolumeMute => 0x120,
        MediaPlayPause => 0x122,
        MediaStop => 0x124,
        AudioVolumeDown => 0x12E,
        AudioVolumeUp => 0x130,
        NumpadDivide => 0x135,
        PrintScreen => 0x137,
        AltRight => 0x138,
        NumLock => 0x145,
        Home => 0x147,
        ArrowUp => 0x148,
        PageUp => 0x149,
        ArrowLeft => 0x14B,
        ArrowRight => 0x14D,
        End => 0x14F,
        ArrowDown => 0x150,
        PageDown => 0x151,
        Insert => 0x152,
        Delete => 0x153,
        SuperLeft => 0x15B,
        SuperRight => 0x15C,
        ContextMenu => 0x15D,
        _ => return None,
    })
}
