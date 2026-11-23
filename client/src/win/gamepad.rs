//! Gamepads through XInput: every connected pad's state, sent to the server as `Msg::Pads` on each
//! change and every `HEARTBEAT_NS`, like the keyboard. XInput's four slots are the players, and
//! its button layout is the protocol's. Rumble a game asks for comes back and is played on the pad.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use proto::stream::{MAX_PADS, Msg, Pad, PadKind, Pads};
use timing::now_ns;
use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState};

use crate::link::Link;

const HEARTBEAT_NS: u64 = 50_000_000;
/// After the last pad goes, keep saying so for this long, then stop sending.
const EMPTY_FOR_NS: u64 = 1_000_000_000;
/// Asking about a slot with no pad in it is slow, so those are only looked at this often.
const RESCAN_NS: u64 = 1_000_000_000;

pub struct Poller {
    link: Arc<Link>,
    /// Whether the stream window has focus: otherwise the pads read as released, as keys do, but
    /// stay plugged in (a game would see them disconnect).
    focused: Arc<AtomicBool>,
    connected: [bool; MAX_PADS],
    next_scan_ns: u64,
    last: Pads,
    seq: u32,
    last_sent_ns: u64,
    empty_since_ns: Option<u64>,
}

impl Poller {
    pub fn new(link: Arc<Link>, focused: Arc<AtomicBool>) -> Poller {
        Poller { link, focused, connected: [false; MAX_PADS], next_scan_ns: 0, last: Pads::default(), seq: 0, last_sent_ns: 0, empty_since_ns: None }
    }

    /// A game set slot `slot`'s motors (0..=255 each).
    pub fn rumble(&mut self, slot: u8, large: u8, small: u8) {
        if self.connected.get(slot as usize) == Some(&true) {
            let v = XINPUT_VIBRATION { wLeftMotorSpeed: large as u16 * 257, wRightMotorSpeed: small as u16 * 257 };
            unsafe { XInputSetState(slot as u32, &v) };
        }
    }

    /// Reads the pads and sends their state if it changed or is due.
    pub fn poll(&mut self) {
        let now = now_ns();
        let scan = now >= self.next_scan_ns;
        if scan {
            self.next_scan_ns = now + RESCAN_NS;
        }
        let focused = self.focused.load(Relaxed);
        let mut pads = Pads::default();
        for (slot, connected) in self.connected.iter_mut().enumerate().take(MAX_PADS) {
            if !*connected && !scan {
                continue;
            }
            let mut state = XINPUT_STATE::default();
            let here = unsafe { XInputGetState(slot as u32, &mut state) } == 0;
            if here != *connected {
                *connected = here;
                println!("gamepad: player {} {}", slot + 1, if here { "connected" } else { "disconnected" });
            }
            if !here {
                continue;
            }
            let g = state.Gamepad;
            let mut pad = Pad { slot: slot as u8, kind: PadKind::Xbox, ..Default::default() };
            if focused {
                pad = Pad {
                    buttons: g.wButtons.0,
                    left_trigger: g.bLeftTrigger,
                    right_trigger: g.bRightTrigger,
                    lx: g.sThumbLX,
                    ly: g.sThumbLY,
                    rx: g.sThumbRX,
                    ry: g.sThumbRY,
                    ..pad
                };
            }
            pads.pads[pads.count as usize] = pad;
            pads.count += 1;
        }
        if pads.count == 0 {
            let since = *self.empty_since_ns.get_or_insert(now);
            if self.last.count == 0 && now - since > EMPTY_FOR_NS {
                return;
            }
        } else {
            self.empty_since_ns = None;
        }
        let changed = (pads.count, pads.pads) != (self.last.count, self.last.pads);
        if changed || now - self.last_sent_ns >= HEARTBEAT_NS {
            self.seq = self.seq.wrapping_add(1);
            pads.seq = self.seq;
            let mut buf = [0u8; proto::MAX_PLAIN];
            let n = Msg::Pads(pads).encode(&mut buf);
            self.link.send(&buf[..n]);
            self.last = pads;
            self.last_sent_ns = now;
        }
    }
}
