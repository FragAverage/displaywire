//! Streaming protocol between `streamy-server` and `streamy-client`.

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

const FLAG_KEYFRAME: u8 = 1;
const FLAG_ABSOLUTE: u8 = 1;

/// Bits in `Input::keys`: one per Windows set-1 scancode, `+ 0x100` for E0-extended keys.
pub const KEY_BITS: usize = 512;
pub const BUTTON_LEFT: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;
pub const BUTTON_MIDDLE: u8 = 4;
pub const BUTTON_BACK: u8 = 8;
pub const BUTTON_FORWARD: u8 = 16;

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
}

impl Default for Settings {
    /// The server's own defaults.
    fn default() -> Self {
        Settings { bitrate_kbps: 50_000, fps_cap: 0, fec_percent: 10 }
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
            XFER => Msg::Xfer { id: r.u32()?, index: r.u32()?, payload: r.rest() },
            XFER_ACK => Msg::XferAck { id: r.u32()?, next: r.u32()?, sack: r.u64()? },
            XFER_CANCEL => Msg::XferCancel { id: r.u32()?, reason: r.u8()? },
            SETTINGS => Msg::Settings(Settings { bitrate_kbps: r.u32()?, fps_cap: r.u16()?, fec_percent: r.u8()? }),
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
            Msg::Settings(Settings { bitrate_kbps: 80_000, fps_cap: 120, fec_percent: 15 }),
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
        let wild = Settings { bitrate_kbps: 1, fps_cap: 60_000, fec_percent: 255 };
        assert_eq!(wild.clamped(), Settings { bitrate_kbps: 2_000, fps_cap: 1000, fec_percent: 50 });
        assert_eq!(Settings::default().clamped(), Settings::default());
        assert_eq!(Settings { bitrate_kbps: 9_999_999, fps_cap: 0, fec_percent: 0 }.clamped().bitrate_kbps, 500_000);
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
}
