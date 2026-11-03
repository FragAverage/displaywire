//! The client's gamepads as virtual controllers through ViGEmBus: an Xbox 360 pad, or a
//! DualShock 4 for PlayStation pads, per slot. Games see ordinary controllers. Rumble a game
//! sets on an Xbox pad goes back to the client (DualShock 4 rumble and light bar need ViGEm's
//! extended interface, which `vigem-client` doesn't cover yet).

use std::sync::Arc;
use std::sync::mpsc;

use proto::stream::{MAX_PADS, Pad, PadKind, Pads, pad};
use vigem_client::{Client, DS4Report, DualShock4Wired, TargetId, XButtons, XGamepad, Xbox360Wired};

const SEC: u64 = 1_000_000_000;
/// Pads are unplugged if the client goes quiet this long (it sends their state every 50 ms).
const IDLE_UNPLUG_NS: u64 = SEC;
const ERROR_NO_MORE_ITEMS: u32 = 259;

enum Target {
    Xbox(Xbox360Wired<Arc<Client>>),
    PlayStation(DualShock4Wired<Arc<Client>>),
}

impl Target {
    fn kind(&self) -> PadKind {
        match self {
            Target::Xbox(_) => PadKind::Xbox,
            Target::PlayStation(_) => PadKind::PlayStation,
        }
    }
}

/// What a game asked of a pad, to pass back to the client.
pub struct Feedback {
    pub slot: u8,
    pub large: u8,
    pub small: u8,
}

pub struct Gamepads {
    /// None until a client first sends pads; `Err` once connecting failed (said once).
    bus: Option<Result<Arc<Client>, ()>>,
    targets: [Option<Target>; MAX_PADS],
    last_seq: Option<u32>,
    last_heard_ns: u64,
    feedback_tx: mpsc::Sender<Feedback>,
    pub feedback: mpsc::Receiver<Feedback>,
}

impl Gamepads {
    pub fn new() -> Gamepads {
        let (feedback_tx, feedback) = mpsc::channel();
        Gamepads { bus: None, targets: Default::default(), last_seq: None, last_heard_ns: 0, feedback_tx, feedback }
    }

    fn bus(&mut self) -> Option<Arc<Client>> {
        let bus = self.bus.get_or_insert_with(|| {
            Client::connect().map(Arc::new).map_err(|e| {
                println!("gamepads: ViGEmBus isn't available ({e}); install it from https://github.com/nefarius/ViGEmBus/releases");
            })
        });
        bus.as_ref().ok().cloned()
    }

    /// A new client session: unplug everything.
    pub fn reset(&mut self) {
        self.targets = Default::default();
        self.last_seq = None;
    }

    /// Unplugs the pads if the client has gone quiet.
    pub fn tick(&mut self, now_ns: u64) {
        if self.targets.iter().any(Option::is_some) && now_ns.saturating_sub(self.last_heard_ns) > IDLE_UNPLUG_NS {
            println!("gamepads: client went quiet, unplugging");
            self.reset();
        }
    }

    pub fn apply(&mut self, pads: &Pads, now_ns: u64) {
        // Older than what we have (reordered): ignore. A big jump back is a new client sequence.
        if let Some(last) = self.last_seq
            && pads.seq.wrapping_sub(last) as i32 <= 0
            && last.wrapping_sub(pads.seq) < 1000
        {
            return;
        }
        self.last_seq = Some(pads.seq);
        self.last_heard_ns = now_ns;
        let mut present = [false; MAX_PADS];
        for p in pads.iter() {
            present[p.slot as usize] = true;
            match self.update(p) {
                Ok(()) => {}
                // ViGEmBus had no read from Windows waiting for this report (nothing has the pad open
                // yet, or not at this instant): harmless, the next report goes through.
                Err(vigem_client::Error::WinError(ERROR_NO_MORE_ITEMS)) => {}
                Err(e) => {
                    println!("gamepads: slot {}: {e}", p.slot);
                    self.targets[p.slot as usize] = None;
                }
            }
        }
        for (slot, target) in self.targets.iter_mut().enumerate() {
            if !present[slot] && target.take().is_some() {
                println!("gamepads: slot {slot} unplugged");
            }
        }
    }

    fn update(&mut self, p: &Pad) -> Result<(), vigem_client::Error> {
        let slot = p.slot as usize;
        if self.targets[slot].as_ref().is_some_and(|t| t.kind() != p.kind) {
            self.targets[slot] = None;
        }
        if self.targets[slot].is_none() {
            let Some(bus) = self.bus() else { return Ok(()) };
            self.targets[slot] = Some(self.plug(bus, p.slot, p.kind)?);
        }
        match self.targets[slot].as_mut() {
            Some(Target::Xbox(t)) => t.update(&xbox_report(p)),
            Some(Target::PlayStation(t)) => t.update(&ds4_report(p)),
            None => Ok(()),
        }
    }

    fn plug(&self, bus: Arc<Client>, slot: u8, kind: PadKind) -> Result<Target, vigem_client::Error> {
        let target = match kind {
            PadKind::Xbox => {
                let mut t = Xbox360Wired::new(bus, TargetId::XBOX360_WIRED);
                t.plugin()?;
                t.wait_ready()?;
                let tx = self.feedback_tx.clone();
                // Ends by itself when the target is unplugged.
                t.request_notification()?.spawn_thread(move |_, n| {
                    let _ = tx.send(Feedback { slot, large: n.large_motor, small: n.small_motor });
                });
                Target::Xbox(t)
            }
            PadKind::PlayStation => {
                let mut t = DualShock4Wired::new(bus, TargetId::DUALSHOCK4_WIRED);
                t.plugin()?;
                t.wait_ready()?;
                Target::PlayStation(t)
            }
        };
        println!("gamepads: slot {slot} plugged in as {kind:?}");
        Ok(target)
    }
}

impl Drop for Gamepads {
    fn drop(&mut self) {
        self.reset();
    }
}

fn xbox_report(p: &Pad) -> XGamepad {
    XGamepad {
        // Same layout as XInput; the touchpad bit is ours and means nothing to an Xbox pad.
        buttons: XButtons { raw: p.buttons & !pad::TOUCHPAD },
        left_trigger: p.left_trigger,
        right_trigger: p.right_trigger,
        thumb_lx: p.lx,
        thumb_ly: p.ly,
        thumb_rx: p.rx,
        thumb_ry: p.ry,
    }
}

fn ds4_report(p: &Pad) -> DS4Report {
    let b = |bit: u16| p.buttons & bit != 0;
    // The d-pad is a hat: 0 = up, clockwise in eighths, 8 = released.
    let hat = match (b(pad::DPAD_UP), b(pad::DPAD_RIGHT), b(pad::DPAD_DOWN), b(pad::DPAD_LEFT)) {
        (true, false, _, false) => 0,
        (true, true, _, _) => 1,
        (false, true, false, _) => 2,
        (_, true, true, _) => 3,
        (_, false, true, false) => 4,
        (_, _, true, true) => 5,
        (false, _, false, true) => 6,
        (true, _, _, true) => 7,
        _ => 8,
    };
    let mut buttons = hat;
    for (bit, ds4) in [
        (pad::X, 0x0010),     // square
        (pad::A, 0x0020),     // cross
        (pad::B, 0x0040),     // circle
        (pad::Y, 0x0080),     // triangle
        (pad::LEFT_SHOULDER, 0x0100),
        (pad::RIGHT_SHOULDER, 0x0200),
        (pad::BACK, 0x1000),  // share
        (pad::START, 0x2000), // options
        (pad::LEFT_THUMB, 0x4000),
        (pad::RIGHT_THUMB, 0x8000),
    ] {
        if b(bit) {
            buttons |= ds4;
        }
    }
    // The triggers count as buttons too once pressed at all.
    if p.left_trigger > 0 {
        buttons |= 0x0400;
    }
    if p.right_trigger > 0 {
        buttons |= 0x0800;
    }
    // 0..=255 with 128 centred; DS4 y grows downwards.
    let axis = |v: i16| ((v as i32 + 32768) >> 8) as u8;
    DS4Report {
        thumb_lx: axis(p.lx),
        thumb_ly: 255 - axis(p.ly),
        thumb_rx: axis(p.rx),
        thumb_ry: 255 - axis(p.ry),
        buttons,
        special: b(pad::GUIDE) as u8 | (b(pad::TOUCHPAD) as u8) << 1,
        trigger_l: p.left_trigger,
        trigger_r: p.right_trigger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ds4_hat_and_face_buttons() {
        let r = ds4_report(&Pad { buttons: pad::DPAD_UP | pad::DPAD_RIGHT | pad::A, ..Default::default() });
        assert_eq!(r.buttons, 1 | 0x0020);
        let r = ds4_report(&Pad { lx: -32768, ly: 32767, ..Default::default() });
        assert_eq!((r.thumb_lx, r.thumb_ly, r.buttons), (0, 0, 8));
    }
}
