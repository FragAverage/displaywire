//! Messages for `tools/netprobe`: a fake video stream used to measure the link.

use crate::{Reader, Writer};

/// Bytes taken by a `Data` header; the rest of the datagram is padding.
pub const DATA_HEADER_LEN: usize = 2 + 1 + 4 + 4 + 2 + 2 + 8;

const HELLO: u8 = 1;
const DATA: u8 = 2;
const PING: u8 = 3;
const PONG: u8 = 4;
const DONE: u8 = 5;
const BYE: u8 = 6;

/// Client → server: start streaming with these parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub kbps: u32,
    pub fps: u16,
    /// Fraction of the frame interval each frame's packets are spread over, in 1/1000.
    pub pacing_permille: u16,
    pub packet_len: u16,
    pub secs: u16,
}

/// Server → client: one packet of a fake frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Data {
    /// Global packet sequence number.
    pub seq: u32,
    pub frame: u32,
    pub idx: u16,
    /// Packets in this frame.
    pub count: u16,
    /// Server monotonic clock at send time.
    pub send_ns: u64,
}

/// Server → client: stream finished, with the server's own pacing stats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Done {
    pub packets_sent: u64,
    pub send_errors: u32,
    /// Frames that started more than half an interval after their slot.
    pub late_frames: u32,
    pub max_late_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    Hello(Hello),
    Data(Data),
    /// Client → server; echoed back unchanged as `Pong`.
    Ping { id: u32, t_ns: u64 },
    Pong { id: u32, t_ns: u64 },
    Done(Done),
    Bye,
}

impl Msg {
    /// Encodes into `buf` and returns the length written. `buf` must hold at
    /// least `DATA_HEADER_LEN` bytes.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(buf);
        w.bytes(&crate::MAGIC);
        match *self {
            Msg::Hello(h) => {
                w.u8(HELLO);
                w.u32(h.kbps);
                w.u16(h.fps);
                w.u16(h.pacing_permille);
                w.u16(h.packet_len);
                w.u16(h.secs);
            }
            Msg::Data(d) => {
                w.u8(DATA);
                w.u32(d.seq);
                w.u32(d.frame);
                w.u16(d.idx);
                w.u16(d.count);
                w.u64(d.send_ns);
            }
            Msg::Ping { id, t_ns } | Msg::Pong { id, t_ns } => {
                w.u8(if matches!(self, Msg::Ping { .. }) { PING } else { PONG });
                w.u32(id);
                w.u64(t_ns);
            }
            Msg::Done(d) => {
                w.u8(DONE);
                w.u64(d.packets_sent);
                w.u32(d.send_errors);
                w.u32(d.late_frames);
                w.u64(d.max_late_ns);
            }
            Msg::Bye => w.u8(BYE),
        }
        w.len()
    }

    pub fn decode(buf: &[u8]) -> Option<Msg> {
        let mut r = Reader::new(buf);
        r.magic()?;
        Some(match r.u8()? {
            HELLO => Msg::Hello(Hello {
                kbps: r.u32()?,
                fps: r.u16()?,
                pacing_permille: r.u16()?,
                packet_len: r.u16()?,
                secs: r.u16()?,
            }),
            DATA => Msg::Data(Data {
                seq: r.u32()?,
                frame: r.u32()?,
                idx: r.u16()?,
                count: r.u16()?,
                send_ns: r.u64()?,
            }),
            PING => Msg::Ping { id: r.u32()?, t_ns: r.u64()? },
            PONG => Msg::Pong { id: r.u32()?, t_ns: r.u64()? },
            DONE => Msg::Done(Done {
                packets_sent: r.u64()?,
                send_errors: r.u32()?,
                late_frames: r.u32()?,
                max_late_ns: r.u64()?,
            }),
            BYE => Msg::Bye,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = [
            Msg::Hello(Hello { kbps: 80_000, fps: 120, pacing_permille: 500, packet_len: 1200, secs: 30 }),
            Msg::Data(Data { seq: 7, frame: 3, idx: 1, count: 9, send_ns: 123_456_789 }),
            Msg::Ping { id: 1, t_ns: 2 },
            Msg::Pong { id: 3, t_ns: 4 },
            Msg::Done(Done { packets_sent: 5, send_errors: 6, late_frames: 7, max_late_ns: 8 }),
            Msg::Bye,
        ];
        let mut buf = [0u8; 64];
        for m in msgs {
            let n = m.encode(&mut buf);
            assert_eq!(Msg::decode(&buf[..n]), Some(m));
        }
        assert_eq!(Msg::Data(Data { seq: 0, frame: 0, idx: 0, count: 0, send_ns: 0 }).encode(&mut buf), DATA_HEADER_LEN);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Msg::decode(b""), None);
        assert_eq!(Msg::decode(b"XX\x01"), None);
        assert_eq!(Msg::decode(b"DW\x02\x00"), None);
    }
}
