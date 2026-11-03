//! Gamepads through Apple's GameController framework: every connected pad's state, sent to the
//! server as `Msg::Pads` on each change and every `HEARTBEAT_NS`, like the keyboard. Each pad
//! keeps its slot (player number) while it stays connected. PlayStation pads become a DualShock 4
//! on the PC, anything else an Xbox 360 pad. Rumble and light bar colours games set come back and
//! are played on the pad (rumble through CoreHaptics).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, Message, msg_send};
use objc2_core_haptics::{
    CHHapticAdvancedPatternPlayer, CHHapticDynamicParameter, CHHapticDynamicParameterIDHapticIntensityControl, CHHapticEngine,
    CHHapticEvent, CHHapticEventParameter, CHHapticEventParameterIDHapticIntensity, CHHapticEventTypeHapticContinuous,
    CHHapticPattern, CHHapticPatternPlayer,
};
use objc2_foundation::NSArray;
use objc2_game_controller::{
    GCColor, GCController, GCControllerButtonInput, GCDevice, GCDualSenseGamepad, GCDualShockGamepad, GCExtendedGamepad,
    GCHapticsLocalityDefault, GCSystemGestureState,
};
use proto::stream::{MAX_PADS, Msg, Pad, PadKind, Pads, pad};
use timing::now_ns;

use crate::link::Link;

const HEARTBEAT_NS: u64 = 50_000_000;
/// After the last pad goes, keep saying so for this long, then stop sending.
const EMPTY_FOR_NS: u64 = 1_000_000_000;

pub struct Poller {
    link: Arc<Link>,
    /// Whether the stream window has focus: macOS stops delivering pad input otherwise, so the pads are
    /// reported released, as keys are.
    focused: Arc<AtomicBool>,
    slots: [Option<Retained<GCController>>; MAX_PADS],
    /// Made the first time a game rumbles that slot's pad; None if the pad can't.
    rumble: [Option<Option<Rumble>>; MAX_PADS],
    last: Pads,
    seq: u32,
    last_sent_ns: u64,
    empty_since_ns: Option<u64>,
}

impl Poller {
    pub fn new(link: Arc<Link>, focused: Arc<AtomicBool>) -> Poller {
        Poller {
            link,
            focused,
            slots: Default::default(),
            rumble: Default::default(),
            last: Pads::default(),
            seq: 0,
            last_sent_ns: 0,
            empty_since_ns: None,
        }
    }

    /// A game set slot `slot`'s motors (0..=255 each).
    pub fn rumble(&mut self, slot: u8, large: u8, small: u8) {
        let Some(c) = self.slots.get(slot as usize).and_then(Option::as_ref) else { return };
        let rumble = self.rumble[slot as usize].get_or_insert_with(|| Rumble::new(c));
        if let Some(r) = rumble {
            // One actuator's worth of intensity: the stronger motor wins.
            r.set(large.max(small) as f32 / 255.0);
        }
    }

    /// A game set slot `slot`'s light bar.
    pub fn light(&mut self, slot: u8, r: u8, g: u8, b: u8) {
        let Some(c) = self.slots.get(slot as usize).and_then(Option::as_ref) else { return };
        if let Some(light) = unsafe { c.light() } {
            let color = unsafe { GCColor::initWithRed_green_blue(GCColor::alloc(), r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0) };
            unsafe { light.setColor(&color) };
        }
    }

    /// Reads the pads and sends their state if it changed or is due.
    pub fn poll(&mut self) {
        let now = now_ns();
        let mut pads = Pads::default();
        if self.focused.load(Relaxed) {
            self.assign_slots();
            for (slot, c) in self.slots.iter().enumerate() {
                if let Some(c) = c
                    && let Some(gp) = unsafe { c.extendedGamepad() }
                {
                    pads.pads[pads.count as usize] = read(slot as u8, &gp);
                    pads.count += 1;
                }
            }
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

    /// Keeps pads in the slots they had, frees the slots of pads that went, and gives new pads the
    /// lowest free slot.
    fn assign_slots(&mut self) {
        let connected = unsafe { GCController::controllers() };
        let connected: Vec<Retained<GCController>> = connected.iter().filter(|c| unsafe { c.extendedGamepad() }.is_some()).collect();
        for (slot, rumble) in self.slots.iter_mut().zip(&mut self.rumble) {
            if slot.as_ref().is_some_and(|s| !connected.iter().any(|c| std::ptr::eq(&**c, &**s))) {
                *slot = None;
                *rumble = None;
            }
        }
        for c in connected {
            if self.slots.iter().flatten().any(|s| std::ptr::eq(&*c, &**s)) {
                continue;
            }
            let Some(free) = self.slots.iter().position(Option::is_none) else { break };
            // The Home / PS button is for the PC while streaming, not macOS's game overlay.
            if let Some(gp) = unsafe { c.extendedGamepad() }
                && let Some(home) = unsafe { gp.buttonHome() }
            {
                unsafe { home.setPreferredSystemGestureState(GCSystemGestureState::Disabled) };
            }
            println!("gamepad: {} is player {}", unsafe { c.vendorName() }.map_or("a controller".into(), |n| n.to_string()), free + 1);
            self.slots[free] = Some(c.retain());
        }
    }
}

/// A pad's haptics playing one looping continuous event whose strength follows the game.
struct Rumble {
    _engine: Retained<CHHapticEngine>,
    player: Retained<ProtocolObject<dyn CHHapticAdvancedPatternPlayer>>,
    playing: bool,
}

impl Rumble {
    fn new(c: &GCController) -> Option<Rumble> {
        let made = (|| unsafe {
            let haptics = c.haptics()?;
            // `objc2-game-controller` only binds this on iOS; the method exists on macOS 11+ too.
            let engine: Option<Retained<CHHapticEngine>> = msg_send![&*haptics, createEngineWithLocality: GCHapticsLocalityDefault];
            let engine = engine?;
            engine.startAndReturnError().ok()?;
            let full = CHHapticEventParameter::initWithParameterID_value(CHHapticEventParameter::alloc(), CHHapticEventParameterIDHapticIntensity, 1.0);
            let event = CHHapticEvent::initWithEventType_parameters_relativeTime_duration(
                CHHapticEvent::alloc(),
                CHHapticEventTypeHapticContinuous,
                &NSArray::from_retained_slice(&[full]),
                0.0,
                // The longest a continuous event may be; the player loops it.
                30.0,
            );
            let pattern =
                CHHapticPattern::initWithEvents_parameters_error(CHHapticPattern::alloc(), &NSArray::from_retained_slice(&[event]), &NSArray::new()).ok()?;
            let player = engine.createAdvancedPlayerWithPattern_error(&pattern).ok()?;
            player.setLoopEnabled(true);
            Some(Rumble { _engine: engine, player, playing: false })
        })();
        if made.is_none() {
            println!("gamepad: this controller can't rumble");
        }
        made
    }

    /// 0.0 stops it.
    fn set(&mut self, level: f32) {
        unsafe {
            if level <= 0.0 {
                if self.playing {
                    let _ = self.player.stopAtTime_error(0.0);
                    self.playing = false;
                }
                return;
            }
            let p = CHHapticDynamicParameter::initWithParameterID_value_relativeTime(
                CHHapticDynamicParameter::alloc(),
                CHHapticDynamicParameterIDHapticIntensityControl,
                level,
                0.0,
            );
            let _ = self.player.sendParameters_atTime_error(&NSArray::from_retained_slice(&[p]), 0.0);
            if !self.playing {
                self.playing = self.player.startAtTime_error(0.0).is_ok();
            }
        }
    }
}

fn pressed(b: &GCControllerButtonInput) -> bool {
    unsafe { b.isPressed() }
}

/// -1.0..=1.0 → -32768..=32767.
fn axis(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn read(slot: u8, gp: &GCExtendedGamepad) -> Pad {
    let mut buttons = 0;
    let mut set = |bit: u16, on: bool| {
        if on {
            buttons |= bit;
        }
    };
    unsafe {
        let dpad = gp.dpad();
        set(pad::DPAD_UP, pressed(&dpad.up()));
        set(pad::DPAD_DOWN, pressed(&dpad.down()));
        set(pad::DPAD_LEFT, pressed(&dpad.left()));
        set(pad::DPAD_RIGHT, pressed(&dpad.right()));
        set(pad::A, pressed(&gp.buttonA()));
        set(pad::B, pressed(&gp.buttonB()));
        set(pad::X, pressed(&gp.buttonX()));
        set(pad::Y, pressed(&gp.buttonY()));
        set(pad::LEFT_SHOULDER, pressed(&gp.leftShoulder()));
        set(pad::RIGHT_SHOULDER, pressed(&gp.rightShoulder()));
        set(pad::START, pressed(&gp.buttonMenu()));
        set(pad::BACK, gp.buttonOptions().is_some_and(|b| pressed(&b)));
        set(pad::GUIDE, gp.buttonHome().is_some_and(|b| pressed(&b)));
        set(pad::LEFT_THUMB, gp.leftThumbstickButton().is_some_and(|b| pressed(&b)));
        set(pad::RIGHT_THUMB, gp.rightThumbstickButton().is_some_and(|b| pressed(&b)));
    }
    // DualShock 4 and DualSense pads are their own subclasses, with a clickable touchpad.
    let any: &AnyObject = gp.as_ref();
    let kind = if let Some(ds4) = any.downcast_ref::<GCDualShockGamepad>() {
        set(pad::TOUCHPAD, unsafe { ds4.touchpadButton() }.is_some_and(|b| pressed(&b)));
        PadKind::PlayStation
    } else if let Some(ds) = any.downcast_ref::<GCDualSenseGamepad>() {
        set(pad::TOUCHPAD, unsafe { pressed(&ds.touchpadButton()) });
        PadKind::PlayStation
    } else {
        PadKind::Xbox
    };
    let (ls, rs) = unsafe { (gp.leftThumbstick(), gp.rightThumbstick()) };
    let trigger = |b: Retained<GCControllerButtonInput>| (unsafe { b.value() }.clamp(0.0, 1.0) * 255.0).round() as u8;
    Pad {
        slot,
        kind,
        buttons,
        left_trigger: trigger(unsafe { gp.leftTrigger() }),
        right_trigger: trigger(unsafe { gp.rightTrigger() }),
        lx: axis(unsafe { ls.xAxis().value() }),
        ly: axis(unsafe { ls.yAxis().value() }),
        rx: axis(unsafe { rs.xAxis().value() }),
        ry: axis(unsafe { rs.yAxis().value() }),
        ..Default::default()
    }
}
