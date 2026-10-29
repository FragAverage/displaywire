//! Streaming protocol between `streamy-server` and `streamy-client`.

use crate::{Reader, Writer};

pub const VERSION: u16 = 2;

/// Bytes taken by a `Video` header.
pub const VIDEO_HEADER_LEN: usize = 2 + 1 + 1 + 4 + 2 + 2 + 8;
/// Largest video payload per datagram.
pub const VIDEO_PAYLOAD: usize = crate::MAX_DATAGRAM - VIDEO_HEADER_LEN;

const HELLO: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;
const KEYFRAME_REQUEST: u8 = 4;
const VIDEO: u8 = 5;
const BYE: u8 = 6;
const INPUT: u8 = 7;

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

/// One fragment of an encoded video frame (HEVC Annex B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Video {
    pub frame: u32,
    pub frag: u16,
    pub frags: u16,
    pub keyframe: bool,
    /// Server clock when the frame was captured.
    pub capture_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    /// Client → server: start streaming to me.
    Hello { version: u16 },
    /// Client → server, every few ms; doubles as the session heartbeat.
    Ping { id: u32, client_ns: u64 },
    /// Server → client, for RTT and clock offset.
    Pong { id: u32, client_ns: u64, server_ns: u64 },
    /// Client → server: the decoder lost sync, send an IDR.
    KeyframeRequest,
    Video(Video, &'a [u8]),
    Bye,
    Input(Input),
}

impl<'a> Msg<'a> {
    /// Encodes into `buf` (at least `MAX_DATAGRAM` bytes) and returns the length.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(buf);
        w.bytes(&crate::MAGIC);
        match *self {
            Msg::Hello { version } => {
                w.u8(HELLO);
                w.u16(version);
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
        }
        w.len()
    }

    pub fn decode(buf: &'a [u8]) -> Option<Msg<'a>> {
        let mut r = Reader::new(buf);
        r.magic()?;
        Some(match r.u8()? {
            HELLO => Msg::Hello { version: r.u16()? },
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
                    capture_ns: r.u64()?,
                };
                if v.frags == 0 || v.frag >= v.frags {
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
        let v = Video { frame: 9, frag: 2, frags: 3, keyframe: true, capture_ns: 42 };
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
            Msg::Hello { version: VERSION },
            Msg::Ping { id: 1, client_ns: 2 },
            Msg::Pong { id: 1, client_ns: 2, server_ns: 3 },
            Msg::KeyframeRequest,
            Msg::Video(v, &payload),
            Msg::Video(Video { keyframe: false, ..v }, &payload[..10]),
            Msg::Bye,
            Msg::Input(Input::default()),
            Msg::Input(input),
        ];
        let mut buf = [0u8; crate::MAX_DATAGRAM];
        for m in msgs {
            let n = m.encode(&mut buf);
            assert!(n <= crate::MAX_DATAGRAM);
            assert_eq!(Msg::decode(&buf[..n]), Some(m));
        }
    }

    #[test]
    fn rejects_bad_fragment_index() {
        let mut buf = [0u8; 64];
        let v = Video { frame: 0, frag: 3, frags: 3, keyframe: false, capture_ns: 0 };
        let n = Msg::Video(v, &[]).encode(&mut buf);
        assert_eq!(Msg::decode(&buf[..n]), None);
    }
}
