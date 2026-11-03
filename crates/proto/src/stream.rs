//! Streaming protocol between `displaywire-server` and `displaywire-client`.

use crate::{Reader, Writer};

pub const VERSION: u16 = 5;

/// Bytes taken by a `Video` header.
pub const VIDEO_HEADER_LEN: usize = 2 + 1 + 1 + 4 + 2 + 2 + 1 + 2 + 8;
/// Bytes in every video fragment but the last data one, and in every parity
/// fragment. Even, as the Reed-Solomon code needs.
pub const VIDEO_PAYLOAD: usize = (crate::MAX_PLAIN - VIDEO_HEADER_LEN) & !1;

/// Bytes taken by an `Audio`/`Mic` header.
pub const AUDIO_HEADER_LEN: usize = 2 + 1 + 1 + 4 + 8;
/// Largest audio payload per datagram.
pub const AUDIO_PAYLOAD: usize = crate::MAX_PLAIN - AUDIO_HEADER_LEN;
/// Audio is always 48 kHz stereo.
pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;

/// `Hello::caps`: what the client wants from the session.
/// Server → client desktop audio.
pub const CAP_AUDIO: u8 = 1;
/// Client → server microphone.
pub const CAP_MIC: u8 = 2;
/// The client draws the pointer itself from `Msg::Cursor`/`Msg::CursorShape`, so it isn't in the video.
pub const CAP_CURSOR: u8 = 16;

const HELLO: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;
const KEYFRAME_REQUEST: u8 = 4;
const VIDEO: u8 = 5;
const BYE: u8 = 6;
const INPUT: u8 = 7;
const AUDIO: u8 = 8;
const MIC: u8 = 9;
const SETTINGS: u8 = 10;
const XFER: u8 = 11;
const XFER_ACK: u8 = 12;
const XFER_CANCEL: u8 = 13;
const PADS: u8 = 14;
const RUMBLE: u8 = 15;
const LIGHT: u8 = 16;
const CURSOR: u8 = 17;
const CURSOR_SHAPE: u8 = 18;
const FEEDBACK: u8 = 19;
const FRAME_LOST: u8 = 20;
const RECOVERED: u8 = 21;
const MONITORS: u8 = 22;
const CHOOSE_MONITOR: u8 = 23;

const FLAG_KEYFRAME: u8 = 1;
const FLAG_ABSOLUTE: u8 = 1;

/// Bits in `Input::keys`: one per Windows set-1 scancode, `+ 0x100` for E0-extended keys.
pub const KEY_BITS: usize = 512;
pub const BUTTON_LEFT: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;
pub const BUTTON_MIDDLE: u8 = 4;
pub const BUTTON_BACK: u8 = 8;
pub const BUTTON_FORWARD: u8 = 16;

/// Largest pointer image sent, in pixels each way (Windows' own go to 256 only at huge sizes).
pub const MAX_CURSOR_SIZE: u16 = 128;
const CURSOR_SHAPE_HEADER_LEN: usize = 3 + 4 + 2 * 4 + 4;
/// RGBA bytes per `CursorShape` datagram.
pub const CURSOR_PAYLOAD: usize = crate::MAX_PLAIN - CURSOR_SHAPE_HEADER_LEN;

/// Server → client: one piece of pointer image `id` (RGBA, straight alpha, rows top to bottom).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    /// The pixel that is the pointer's position.
    pub hot_x: u16,
    pub hot_y: u16,
    /// Where this datagram's bytes go in the `width × height × 4` image.
    pub offset: u32,
}

/// Client → server, every 50 ms while video flows: how the video got here, for the server's rate control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Feedback {
    /// Video datagrams that arrived, and how many the frames they belong to were sent as (data + repair).
    pub received: u16,
    pub expected: u16,
    /// Frames that never arrived at all, or couldn't be rebuilt.
    pub frames_lost: u16,
    /// Median capture → arrival time of the frames completed in this period, in µs on the server's
    /// clock (so it includes the clock-offset estimate's error; only its changes matter); -1 if none.
    pub delay_us: i32,
}

/// Monitors listed, and how long a name can be (bytes of UTF-8).
pub const MAX_MONITORS: usize = 8;
pub const MONITOR_NAME_LEN: usize = 32;

/// One of the server's monitors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Monitor {
    pub width: u16,
    pub height: u16,
    /// Hz.
    pub refresh: u16,
    pub primary: bool,
    name: [u8; MONITOR_NAME_LEN],
    name_len: u8,
}

impl Monitor {
    pub fn new(width: u16, height: u16, refresh: u16, primary: bool, name: &str) -> Monitor {
        let mut m = Monitor { width, height, refresh, primary, ..Default::default() };
        // Cut at a character boundary.
        let mut end = name.len().min(MONITOR_NAME_LEN);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        m.name[..end].copy_from_slice(&name.as_bytes()[..end]);
        m.name_len = end as u8;
        m
    }

    pub fn name(&self) -> &str {
        std::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("")
    }
}

/// Server → client: the monitors (in a fixed order: `ChooseMonitor` names one by its place) and
/// which is being streamed; on connecting and whenever they change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Monitors {
    pub count: u8,
    pub streaming: u8,
    pub list: [Monitor; MAX_MONITORS],
}

impl Monitors {
    pub fn iter(&self) -> impl Iterator<Item = &Monitor> {
        self.list[..(self.count as usize).min(MAX_MONITORS)].iter()
    }
}

/// Gamepads one client can have connected at once (XInput's limit).
pub const MAX_PADS: usize = 4;

/// `Pad::buttons`, in XInput's layout, plus the PlayStation touchpad click in a bit XInput leaves free.
pub mod pad {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    /// Menu / Options.
    pub const START: u16 = 0x0010;
    /// View / Share.
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    /// Xbox / PS button.
    pub const GUIDE: u16 = 0x0400;
    pub const TOUCHPAD: u16 = 0x0800;
    /// Cross on PlayStation pads; the face buttons go by position.
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// What a pad should look like on the PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PadKind {
    #[default]
    Xbox,
    /// A DualShock 4 (a DualSense on the client shows up as one).
    PlayStation,
}

/// One gamepad's complete state. Sticks are -32768..=32767 with up positive, as in XInput.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Pad {
    /// 0..MAX_PADS: which player; stable while the pad stays connected.
    pub slot: u8,
    pub kind: PadKind,
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    /// Gyro (deg/s × 16) and accelerometer (g × 8192), x/y/z, when the pad has them.
    pub motion: Option<([i16; 3], [i16; 3])>,
    /// Touchpad fingers, 0..=65535 across and down the pad.
    pub touches: [Option<(u16, u16)>; 2],
}

/// Client → server: every connected pad. Like `Input` it is the whole state, sent on every change and
/// regularly in between, so a lost datagram can't leave a button held; a pad missing from it is unplugged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Pads {
    pub seq: u32,
    pub count: u8,
    pub pads: [Pad; MAX_PADS],
}

impl Pads {
    pub fn iter(&self) -> impl Iterator<Item = &Pad> {
        self.pads[..(self.count as usize).min(MAX_PADS)].iter()
    }
}

/// Client → server keyboard and mouse. Carries the complete key and button
/// state, so a lost datagram can't leave anything stuck down; motion and
/// wheel are deltas since the previous message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input {
    /// Increases by one per message; older messages are ignored.
    pub seq: u32,
    pub keys: [u8; KEY_BITS / 8],
    pub buttons: u8,
    /// Pointer position, 0..=65535 across the streamed screen.
    pub absolute: Option<(u16, u16)>,
    /// Relative pointer motion in mouse counts.
    pub dx: i16,
    pub dy: i16,
    /// Wheel motion, 120 per notch.
    pub wheel: i16,
    pub hwheel: i16,
    /// Scancode of a held key the client's OS auto-repeated, or 0.
    pub repeat: u16,
}

impl Default for Input {
    fn default() -> Self {
        Input { seq: 0, keys: [0; KEY_BITS / 8], buttons: 0, absolute: None, dx: 0, dy: 0, wheel: 0, hwheel: 0, repeat: 0 }
    }
}

impl Input {
    pub fn key(&self, code: u16) -> bool {
        let i = code as usize;
        i < KEY_BITS && self.keys[i / 8] & (1 << (i % 8)) != 0
    }

    pub fn set_key(&mut self, code: u16, down: bool) {
        let i = code as usize;
        if i < KEY_BITS {
            if down {
                self.keys[i / 8] |= 1 << (i % 8);
            } else {
                self.keys[i / 8] &= !(1 << (i % 8));
            }
        }
    }
}

/// One fragment of an encoded video frame (HEVC Annex B). A frame is `frags`
/// data fragments (`frag` 0..frags) followed by `parity` repair fragments
/// (`frag` frags..frags+parity): any `frags` of them rebuild the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Video {
    pub frame: u32,
    pub frag: u16,
    /// Data fragments in the frame.
    pub frags: u16,
    /// Repair fragments in the frame.
    pub parity: u8,
    /// Bytes in the last data fragment (the others are `VIDEO_PAYLOAD`).
    pub last_len: u16,
    pub keyframe: bool,
    /// Server clock when the frame was captured.
    pub capture_ns: u64,
}

/// Stream settings the client asks the server to apply (sent after connecting
/// and whenever they change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Video bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Most frames per second to send; 0 for no limit.
    pub fps_cap: u16,
    /// Repair packets per frame, as a percentage of its data packets.
    pub fec_percent: u8,
    /// Let the server lower the bitrate (from `bitrate_kbps`, the ceiling) and raise repair packets (from
    /// `fec_percent`) when the network struggles. Absent from older clients, which means yes.
    pub adaptive: bool,
}

impl Default for Settings {
    /// The server's own defaults.
    fn default() -> Self {
        Settings { bitrate_kbps: 50_000, fps_cap: 0, fec_percent: 10, adaptive: true }
    }
}

impl Settings {
    pub const MIN_BITRATE_KBPS: u32 = 2_000;
    pub const MAX_BITRATE_KBPS: u32 = 500_000;
    pub const MAX_FEC_PERCENT: u8 = 50;

    /// The same settings with every value brought into a range the server accepts.
    pub fn clamped(self) -> Settings {
        Settings {
            bitrate_kbps: self.bitrate_kbps.clamp(Self::MIN_BITRATE_KBPS, Self::MAX_BITRATE_KBPS),
            fps_cap: if self.fps_cap == 0 { 0 } else { self.fps_cap.clamp(1, 1000) },
            fec_percent: self.fec_percent.min(Self::MAX_FEC_PERCENT),
            adaptive: self.adaptive,
        }
    }
}

/// How an `Audio`/`Mic` payload is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    /// Interleaved signed 16-bit little-endian samples, whole frames only
    /// (4 bytes per stereo frame).
    Pcm16 = 0,
}

/// One chunk of audio, server → client (`Msg::Audio`) or client → server (`Msg::Mic`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Audio {
    /// Increases by one per chunk; gaps are lost chunks.
    pub seq: u32,
    pub format: AudioFormat,
    /// Sender's clock when the chunk's first sample was captured.
    pub capture_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    /// Client → server: start streaming to me. `caps` is `CAP_*` flags.
    Hello { version: u16, caps: u8 },
    /// Client → server, every few ms; doubles as the session heartbeat.
    Ping { id: u32, client_ns: u64 },
    /// Server → client, for RTT and clock offset.
    Pong { id: u32, client_ns: u64, server_ns: u64 },
    /// Client → server: the decoder lost sync, send an IDR.
    KeyframeRequest,
    Video(Video, &'a [u8]),
    Bye,
    Input(Input),
    /// Server → client: desktop audio.
    Audio(Audio, &'a [u8]),
    /// Client → server: microphone.
    Mic(Audio, &'a [u8]),
    /// Client → server: apply these stream settings.
    Settings(Settings),
    /// Either way: one block of a file or clipboard transfer (see the `xfer` crate).
    Xfer { id: u32, index: u32, payload: &'a [u8] },
    /// Either way: what the receiver of transfer `id` has (first missing block, bitmap after it).
    XferAck { id: u32, next: u32, sack: u64 },
    /// Either way: stop sending transfer `id`.
    XferCancel { id: u32, reason: u8 },
    /// Client → server: the client's gamepads.
    Pads(Pads),
    /// Server → client: what a game asked pad `slot`'s motors to do (0 = off).
    Rumble { slot: u8, large: u8, small: u8 },
    /// Server → client: the light bar colour a game set on PlayStation pad `slot`.
    Light { slot: u8, r: u8, g: u8, b: u8 },
    /// Server → client (with `CAP_CURSOR`): the pointer, on every change and every second. `x`/`y` are its
    /// position, 0..=65535 across the streamed screen; `shape` names the image (`CursorShape::id`).
    Cursor { visible: bool, x: u16, y: u16, shape: u32 },
    CursorShape(CursorShape, &'a [u8]),
    Feedback(Feedback),
    /// Client → server: frames `first..=last` never arrived. The server stops the encoder referencing
    /// them (and answers `Recovered`), or sends a keyframe if it can't.
    FrameLost { first: u32, last: u32 },
    /// Server → client, just before frame `from`: from it on, no frame references any frame up to
    /// `lost_upto` that was reported lost, so decoding can carry on there instead of at a keyframe.
    Recovered { from: u32, lost_upto: u32 },
    Monitors(Monitors),
    /// Client → server: stream monitor `index` of the last `Monitors`.
    ChooseMonitor { index: u8 },
}

impl<'a> Msg<'a> {
    /// Encodes into `buf` (at least `MAX_PLAIN` bytes) and returns the length.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(buf);
        w.bytes(&crate::MAGIC);
        match *self {
            Msg::Hello { version, caps } => {
                w.u8(HELLO);
                w.u16(version);
                w.u8(caps);
            }
            Msg::Ping { id, client_ns } => {
                w.u8(PING);
                w.u32(id);
                w.u64(client_ns);
            }
            Msg::Pong { id, client_ns, server_ns } => {
                w.u8(PONG);
                w.u32(id);
                w.u64(client_ns);
                w.u64(server_ns);
            }
            Msg::KeyframeRequest => w.u8(KEYFRAME_REQUEST),
            Msg::Video(v, payload) => {
                w.u8(VIDEO);
                w.u8(if v.keyframe { FLAG_KEYFRAME } else { 0 });
                w.u32(v.frame);
                w.u16(v.frag);
                w.u16(v.frags);
                w.u8(v.parity);
                w.u16(v.last_len);
                w.u64(v.capture_ns);
                w.bytes(payload);
            }
            Msg::Bye => w.u8(BYE),
            Msg::Input(i) => {
                w.u8(INPUT);
                w.u8(if i.absolute.is_some() { FLAG_ABSOLUTE } else { 0 });
                w.u32(i.seq);
                w.bytes(&i.keys);
                w.u8(i.buttons);
                let (x, y) = i.absolute.unwrap_or_default();
                w.u16(x);
                w.u16(y);
                for v in [i.dx, i.dy, i.wheel, i.hwheel] {
                    w.u16(v as u16);
                }
                w.u16(i.repeat);
            }
            Msg::Pads(p) => {
                w.u8(PADS);
                w.u32(p.seq);
                let pads: Vec<&Pad> = p.iter().collect();
                w.u8(pads.len() as u8);
                for pad in pads {
                    let flags = pad.motion.is_some() as u8 | (pad.touches[0].is_some() as u8) << 1 | (pad.touches[1].is_some() as u8) << 2;
                    w.u8(pad.slot);
                    w.u8(pad.kind as u8);
                    w.u16(pad.buttons);
                    w.u8(pad.left_trigger);
                    w.u8(pad.right_trigger);
                    for v in [pad.lx, pad.ly, pad.rx, pad.ry] {
                        w.u16(v as u16);
                    }
                    w.u8(flags);
                    let (gyro, accel) = pad.motion.unwrap_or_default();
                    for v in gyro.into_iter().chain(accel) {
                        w.u16(v as u16);
                    }
                    for t in pad.touches {
                        let (x, y) = t.unwrap_or_default();
                        w.u16(x);
                        w.u16(y);
                    }
                }
            }
            Msg::Rumble { slot, large, small } => {
                w.u8(RUMBLE);
                w.u8(slot);
                w.u8(large);
                w.u8(small);
            }
            Msg::Cursor { visible, x, y, shape } => {
                w.u8(CURSOR);
                w.u8(visible as u8);
                w.u16(x);
                w.u16(y);
                w.u32(shape);
            }
            Msg::CursorShape(c, payload) => {
                w.u8(CURSOR_SHAPE);
                w.u32(c.id);
                for v in [c.width, c.height, c.hot_x, c.hot_y] {
                    w.u16(v);
                }
                w.u32(c.offset);
                w.bytes(payload);
            }
            Msg::Feedback(f) => {
                w.u8(FEEDBACK);
                w.u16(f.received);
                w.u16(f.expected);
                w.u16(f.frames_lost);
                w.u32(f.delay_us as u32);
            }
            Msg::Monitors(m) => {
                w.u8(MONITORS);
                w.u8(m.count.min(MAX_MONITORS as u8));
                w.u8(m.streaming);
                for mon in m.iter() {
                    w.u16(mon.width);
                    w.u16(mon.height);
                    w.u16(mon.refresh);
                    w.u8(mon.primary as u8);
                    w.u8(mon.name_len);
                    w.bytes(&mon.name[..mon.name_len as usize]);
                }
            }
            Msg::ChooseMonitor { index } => {
                w.u8(CHOOSE_MONITOR);
                w.u8(index);
            }
            Msg::FrameLost { first, last } => {
                w.u8(FRAME_LOST);
                w.u32(first);
                w.u32(last);
            }
            Msg::Recovered { from, lost_upto } => {
                w.u8(RECOVERED);
                w.u32(from);
                w.u32(lost_upto);
            }
            Msg::Light { slot, r, g, b } => {
                w.u8(LIGHT);
                w.u8(slot);
                w.u8(r);
                w.u8(g);
                w.u8(b);
            }
            Msg::Xfer { id, index, payload } => {
                w.u8(XFER);
                w.u32(id);
                w.u32(index);
                w.bytes(payload);
            }
            Msg::XferAck { id, next, sack } => {
                w.u8(XFER_ACK);
                w.u32(id);
                w.u32(next);
                w.u64(sack);
            }
            Msg::XferCancel { id, reason } => {
                w.u8(XFER_CANCEL);
                w.u32(id);
                w.u8(reason);
            }
            Msg::Settings(s) => {
                w.u8(SETTINGS);
                w.u32(s.bitrate_kbps);
                w.u16(s.fps_cap);
                w.u8(s.fec_percent);
                w.u8(s.adaptive as u8);
            }
            Msg::Audio(a, payload) | Msg::Mic(a, payload) => {
                w.u8(if matches!(self, Msg::Audio(..)) { AUDIO } else { MIC });
                w.u8(a.format as u8);
                w.u32(a.seq);
                w.u64(a.capture_ns);
                w.bytes(payload);
            }
        }
        w.len()
    }

    pub fn decode(buf: &'a [u8]) -> Option<Msg<'a>> {
        let mut r = Reader::new(buf);
        r.magic()?;
        Some(match r.u8()? {
            HELLO => Msg::Hello { version: r.u16()?, caps: r.u8()? },
            PING => Msg::Ping { id: r.u32()?, client_ns: r.u64()? },
            PONG => Msg::Pong { id: r.u32()?, client_ns: r.u64()?, server_ns: r.u64()? },
            KEYFRAME_REQUEST => Msg::KeyframeRequest,
            VIDEO => {
                let flags = r.u8()?;
                let v = Video {
                    keyframe: flags & FLAG_KEYFRAME != 0,
                    frame: r.u32()?,
                    frag: r.u16()?,
                    frags: r.u16()?,
                    parity: r.u8()?,
                    last_len: r.u16()?,
                    capture_ns: r.u64()?,
                };
                if v.frags == 0 || v.frag as usize >= v.frags as usize + v.parity as usize || v.last_len as usize > VIDEO_PAYLOAD {
                    return None;
                }
                Msg::Video(v, r.rest())
            }
            BYE => Msg::Bye,
            INPUT => {
                let flags = r.u8()?;
                let seq = r.u32()?;
                let keys = r.take()?;
                let buttons = r.u8()?;
                let (x, y) = (r.u16()?, r.u16()?);
                Msg::Input(Input {
                    seq,
                    keys,
                    buttons,
                    absolute: (flags & FLAG_ABSOLUTE != 0).then_some((x, y)),
                    dx: r.u16()? as i16,
                    dy: r.u16()? as i16,
                    wheel: r.u16()? as i16,
                    hwheel: r.u16()? as i16,
                    repeat: r.u16()?,
                })
            }
            PADS => {
                let mut p = Pads { seq: r.u32()?, count: r.u8()?, ..Default::default() };
                if p.count as usize > MAX_PADS {
                    return None;
                }
                for pad in &mut p.pads[..p.count as usize] {
                    let slot = r.u8()?;
                    let kind = match r.u8()? {
                        0 => PadKind::Xbox,
                        1 => PadKind::PlayStation,
                        _ => return None,
                    };
                    if slot as usize >= MAX_PADS {
                        return None;
                    }
                    let (buttons, left_trigger, right_trigger) = (r.u16()?, r.u8()?, r.u8()?);
                    let (lx, ly, rx, ry) = (r.u16()? as i16, r.u16()? as i16, r.u16()? as i16, r.u16()? as i16);
                    let flags = r.u8()?;
                    let mut m = [0i16; 6];
                    for v in &mut m {
                        *v = r.u16()? as i16;
                    }
                    let mut touches = [None; 2];
                    for (i, t) in touches.iter_mut().enumerate() {
                        let (x, y) = (r.u16()?, r.u16()?);
                        *t = (flags & (2 << i) != 0).then_some((x, y));
                    }
                    let motion = (flags & 1 != 0).then_some(([m[0], m[1], m[2]], [m[3], m[4], m[5]]));
                    *pad = Pad { slot, kind, buttons, left_trigger, right_trigger, lx, ly, rx, ry, motion, touches };
                }
                Msg::Pads(p)
            }
            RUMBLE => Msg::Rumble { slot: r.u8()?, large: r.u8()?, small: r.u8()? },
            FEEDBACK => Msg::Feedback(Feedback { received: r.u16()?, expected: r.u16()?, frames_lost: r.u16()?, delay_us: r.u32()? as i32 }),
            MONITORS => {
                let mut m = Monitors { count: r.u8()?, streaming: r.u8()?, ..Default::default() };
                if m.count as usize > MAX_MONITORS {
                    return None;
                }
                for mon in &mut m.list[..m.count as usize] {
                    let (width, height, refresh, primary) = (r.u16()?, r.u16()?, r.u16()?, r.u8()? != 0);
                    let len = r.u8()? as usize;
                    if len > MONITOR_NAME_LEN {
                        return None;
                    }
                    let mut name = [0u8; MONITOR_NAME_LEN];
                    for b in &mut name[..len] {
                        *b = r.u8()?;
                    }
                    *mon = Monitor { width, height, refresh, primary, name, name_len: len as u8 };
                }
                Msg::Monitors(m)
            }
            CHOOSE_MONITOR => Msg::ChooseMonitor { index: r.u8()? },
            FRAME_LOST => Msg::FrameLost { first: r.u32()?, last: r.u32()? },
            RECOVERED => Msg::Recovered { from: r.u32()?, lost_upto: r.u32()? },
            LIGHT => Msg::Light { slot: r.u8()?, r: r.u8()?, g: r.u8()?, b: r.u8()? },
            CURSOR => Msg::Cursor { visible: r.u8()? != 0, x: r.u16()?, y: r.u16()?, shape: r.u32()? },
            CURSOR_SHAPE => {
                let c = CursorShape { id: r.u32()?, width: r.u16()?, height: r.u16()?, hot_x: r.u16()?, hot_y: r.u16()?, offset: r.u32()? };
                let payload = r.rest();
                let total = c.width as usize * c.height as usize * 4;
                if c.width == 0 || c.height == 0 || c.width > MAX_CURSOR_SIZE || c.height > MAX_CURSOR_SIZE || c.offset as usize + payload.len() > total {
                    return None;
                }
                Msg::CursorShape(c, payload)
            }
            XFER => Msg::Xfer { id: r.u32()?, index: r.u32()?, payload: r.rest() },
            XFER_ACK => Msg::XferAck { id: r.u32()?, next: r.u32()?, sack: r.u64()? },
            XFER_CANCEL => Msg::XferCancel { id: r.u32()?, reason: r.u8()? },
            SETTINGS => Msg::Settings(Settings {
                bitrate_kbps: r.u32()?,
                fps_cap: r.u16()?,
                fec_percent: r.u8()?,
                adaptive: r.u8().is_none_or(|a| a != 0),
            }),
            kind @ (AUDIO | MIC) => {
                let format = match r.u8()? {
                    0 => AudioFormat::Pcm16,
                    _ => return None,
                };
                let a = Audio { format, seq: r.u32()?, capture_ns: r.u64()? };
                let payload = r.rest();
                // Whole stereo frames only.
                if !payload.len().is_multiple_of(2 * AUDIO_CHANNELS) {
                    return None;
                }
                if kind == AUDIO { Msg::Audio(a, payload) } else { Msg::Mic(a, payload) }
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let payload = [7u8; VIDEO_PAYLOAD];
        let v = Video { frame: 9, frag: 2, frags: 3, parity: 1, last_len: 700, keyframe: true, capture_ns: 42 };
        let mut input = Input {
            seq: 7,
            buttons: BUTTON_LEFT | BUTTON_FORWARD,
            absolute: Some((65535, 3)),
            dx: -5,
            dy: 300,
            wheel: -120,
            hwheel: 1,
            repeat: 0x11E,
            ..Default::default()
        };
        input.set_key(0x1E, true);
        input.set_key(0x15B, true);
        assert!(input.key(0x15B) && !input.key(0x5B));
        let msgs = [
            Msg::Hello { version: VERSION, caps: CAP_AUDIO | CAP_MIC },
            Msg::Ping { id: 1, client_ns: 2 },
            Msg::Pong { id: 1, client_ns: 2, server_ns: 3 },
            Msg::KeyframeRequest,
            Msg::Video(v, &payload),
            Msg::Video(Video { keyframe: false, ..v }, &payload[..10]),
            Msg::Bye,
            Msg::Input(Input::default()),
            Msg::Input(input),
            Msg::Audio(Audio { seq: 5, format: AudioFormat::Pcm16, capture_ns: 99 }, &[1u8; 960]),
            Msg::Mic(Audio { seq: u32::MAX, format: AudioFormat::Pcm16, capture_ns: 0 }, &[]),
            Msg::Settings(Settings { bitrate_kbps: 80_000, fps_cap: 120, fec_percent: 15, adaptive: false }),
            Msg::Xfer { id: 7, index: 0, payload: &[1, 2, 3] },
            Msg::Xfer { id: u32::MAX, index: u32::MAX, payload: &[9; 1024] },
            Msg::XferAck { id: 7, next: 12, sack: 0xDEAD_BEEF_0000_0001 },
            Msg::XferCancel { id: 7, reason: 2 },
        ];
        let mut buf = [0u8; crate::MAX_DATAGRAM];
        for m in msgs {
            let n = m.encode(&mut buf);
            assert!(n <= crate::MAX_PLAIN);
            assert_eq!(Msg::decode(&buf[..n]), Some(m));
        }
    }

    #[test]
    fn settings_are_clamped_to_what_the_server_accepts() {
        let wild = Settings { bitrate_kbps: 1, fps_cap: 60_000, fec_percent: 255, adaptive: true };
        assert_eq!(wild.clamped(), Settings { bitrate_kbps: 2_000, fps_cap: 1000, fec_percent: 50, adaptive: true });
        assert_eq!(Settings::default().clamped(), Settings::default());
        assert_eq!(Settings { bitrate_kbps: 9_999_999, fps_cap: 0, fec_percent: 0, adaptive: true }.clamped().bitrate_kbps, 500_000);
    }

    #[test]
    fn full_audio_chunk_fits() {
        let mut buf = [0u8; crate::MAX_DATAGRAM];
        let payload = [0u8; AUDIO_PAYLOAD];
        let a = Audio { seq: 0, format: AudioFormat::Pcm16, capture_ns: 0 };
        assert_eq!(Msg::Audio(a, &payload).encode(&mut buf), crate::MAX_PLAIN);
    }

    #[test]
    fn rejects_partial_audio_frames_and_unknown_formats() {
        let mut buf = [0u8; 64];
        let a = Audio { seq: 0, format: AudioFormat::Pcm16, capture_ns: 0 };
        let n = Msg::Audio(a, &[0u8; 6]).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
        let n = Msg::Audio(a, &[0u8; 8]).encode(&mut buf);
        buf[3] = 7;
        assert_eq!(Msg::decode(&buf[..n]), None);
    }

    #[test]
    fn parity_fragment_indexes_are_accepted_up_to_the_count() {
        let mut buf = [0u8; 64];
        let v = |frag| Video { frame: 0, frag, frags: 3, parity: 2, last_len: 5, keyframe: false, capture_ns: 0 };
        for frag in [3, 4] {
            let n = Msg::Video(v(frag), &[]).encode(&mut buf);
            assert!(Msg::decode(&buf[..n]).is_some());
        }
        let n = Msg::Video(v(5), &[]).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
    }

    #[test]
    fn rejects_bad_fragment_index() {
        let mut buf = [0u8; 64];
        let v = Video { frame: 0, frag: 3, frags: 3, parity: 0, last_len: 0, keyframe: false, capture_ns: 0 };
        let n = Msg::Video(v, &[]).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
    }

    #[test]
    fn pads_round_trip() {
        let mut p = Pads { seq: 7, count: 2, ..Default::default() };
        p.pads[0] = Pad { slot: 0, buttons: pad::A | pad::DPAD_UP, left_trigger: 255, lx: -32768, ry: 32767, ..Default::default() };
        p.pads[1] = Pad {
            slot: 3,
            kind: PadKind::PlayStation,
            buttons: pad::TOUCHPAD | pad::GUIDE,
            motion: Some(([1, -2, 3], [-8192, 0, 8192])),
            touches: [None, Some((65535, 12))],
            ..Default::default()
        };
        let mut buf = [0u8; crate::MAX_PLAIN];
        let n = Msg::Pads(p).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), Some(Msg::Pads(p)));
        for m in [Msg::Rumble { slot: 1, large: 200, small: 3 }, Msg::Light { slot: 2, r: 1, g: 2, b: 3 }] {
            let n = m.encode(&mut buf);
            assert_eq!(Msg::decode(&buf[..n]), Some(m));
        }
        // A slot out of range is rejected.
        p.pads[1].slot = MAX_PADS as u8;
        let n = Msg::Pads(p).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
    }

    #[test]
    fn cursor_messages_round_trip_and_bad_shapes_are_rejected() {
        let mut buf = [0u8; crate::MAX_PLAIN];
        let m = Msg::Cursor { visible: true, x: 65535, y: 3, shape: 9 };
        let n = m.encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), Some(m));
        let c = CursorShape { id: 9, width: 32, height: 32, hot_x: 1, hot_y: 2, offset: 4096 - CURSOR_PAYLOAD as u32 };
        let payload = [7u8; CURSOR_PAYLOAD];
        let n = Msg::CursorShape(c, &payload).encode(&mut buf);
        assert_eq!(n, crate::MAX_PLAIN);
        assert_eq!(Msg::decode(&buf[..n]), Some(Msg::CursorShape(c, &payload)));
        // Past the end of the image, or too big.
        let n = Msg::CursorShape(CursorShape { offset: 4096 - 10, ..c }, &payload).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
        let n = Msg::CursorShape(CursorShape { width: MAX_CURSOR_SIZE + 1, offset: 0, ..c }, &payload[..4]).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
    }

    #[test]
    fn feedback_round_trips_and_old_settings_mean_adaptive() {
        let mut buf = [0u8; crate::MAX_PLAIN];
        for m in [
            Msg::Feedback(Feedback { received: 900, expected: 1000, frames_lost: 2, delay_us: -1 }),
            Msg::FrameLost { first: 7, last: 9 },
            Msg::Recovered { from: 12, lost_upto: 9 },
        ] {
            let n = m.encode(&mut buf);
            assert_eq!(Msg::decode(&buf[..n]), Some(m));
        }
        // A settings message from before `adaptive` existed: one byte shorter.
        let n = Msg::Settings(Settings { bitrate_kbps: 30_000, fps_cap: 0, fec_percent: 10, adaptive: false }).encode(&mut buf);
        let Some(Msg::Settings(s)) = Msg::decode(&buf[..n - 1]) else { panic!("decodes") };
        assert!(s.adaptive);
    }

    #[test]
    fn monitors_round_trip_with_names_cut_at_a_character() {
        let mut m = Monitors { count: 2, streaming: 1, ..Default::default() };
        m.list[0] = Monitor::new(2560, 1440, 240, true, "LC32G7xT");
        m.list[1] = Monitor::new(3440, 1440, 120, false, &"é".repeat(20));
        assert_eq!(m.list[1].name(), "é".repeat(16));
        let mut buf = [0u8; crate::MAX_PLAIN];
        let n = Msg::Monitors(m).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), Some(Msg::Monitors(m)));
        let n = Msg::ChooseMonitor { index: 1 }.encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), Some(Msg::ChooseMonitor { index: 1 }));
    }
}
