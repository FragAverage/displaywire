//! AV1 in IVF → temporal-unit samples plus an `av1C` configuration record.

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

pub struct Stream {
    pub width: i32,
    pub height: i32,
    /// AV1CodecConfigurationRecord for the format description.
    pub av1c: Vec<u8>,
    /// One sample per temporal unit, temporal delimiters removed.
    pub frames: Vec<Vec<u8>>,
}

fn leb128(data: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    for i in 0..8 {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as usize) << (i * 7);
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// (type, whole OBU bytes, payload bytes).
type Obu<'a> = (u8, &'a [u8], &'a [u8]);

/// Each OBU in a temporal unit.
fn obus(tu: &[u8]) -> Result<Vec<Obu<'_>>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < tu.len() {
        let start = pos;
        let header = tu[pos];
        let ty = (header >> 3) & 0xf;
        pos += 1 + ((header >> 2) & 1) as usize;
        if header & 0b10 == 0 {
            return Err("OBU without size field".into());
        }
        let size = leb128(tu, &mut pos).ok_or("bad OBU size")?;
        let payload = tu.get(pos..pos + size).ok_or("truncated OBU")?;
        pos += size;
        out.push((ty, &tu[start..pos], payload));
    }
    Ok(out)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn f(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8).ok_or("sequence header truncated")?;
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Ok(v)
    }

    fn uvlc(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.f(1)? == 0 {
            zeros += 1;
        }
        Ok(if zeros >= 32 { u32::MAX } else { self.f(zeros)? + ((1 << zeros) - 1) })
    }
}

/// Reads profile, level and tier of operating point 0 (AV1 spec 5.5.1).
/// Bit depth and chroma are assumed 8-bit 4:2:0, which is all the server sends.
fn av1c(seq_header_payload: &[u8], seq_header_obu: &[u8]) -> Result<Vec<u8>, String> {
    let mut b = Bits { data: seq_header_payload, pos: 0 };
    let profile = b.f(3)?;
    let _still_picture = b.f(1)?;
    let reduced = b.f(1)?;
    let (level, tier) = if reduced == 1 {
        (b.f(5)?, 0)
    } else {
        let mut decoder_model = false;
        let mut buffer_delay_len = 0;
        if b.f(1)? == 1 {
            // timing_info
            b.f(32)?;
            b.f(32)?;
            if b.f(1)? == 1 {
                b.uvlc()?;
            }
            decoder_model = b.f(1)? == 1;
            if decoder_model {
                buffer_delay_len = b.f(5)? + 1;
                b.f(32)?;
                b.f(5)?;
                b.f(5)?;
            }
        }
        let initial_display_delay = b.f(1)? == 1;
        let _op_count_minus_1 = b.f(5)?;
        let _idc = b.f(12)?;
        let level = b.f(5)?;
        let tier = if level > 7 { b.f(1)? } else { 0 };
        if decoder_model && b.f(1)? == 1 {
            b.f(buffer_delay_len * 2 + 1)?;
        }
        if initial_display_delay && b.f(1)? == 1 {
            b.f(4)?;
        }
        (level, tier)
    };
    if profile != 0 {
        return Err(format!("AV1 profile {profile} not supported (expected Main)"));
    }
    // marker=1, version=1 | profile, level | tier, 8-bit, 4:2:0 (subsampling x/y set) | no delay
    let mut rec = vec![0x81, ((profile << 5) | level) as u8, ((tier << 7) | 0b0000_1100) as u8, 0];
    rec.extend_from_slice(seq_header_obu);
    Ok(rec)
}

pub fn parse(data: &[u8]) -> Result<Stream, String> {
    if data.len() < 32 || &data[0..4] != b"DKIF" || &data[8..12] != b"AV01" {
        return Err("not an AV1 IVF file".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
    let (width, height) = (u16_at(12) as i32, u16_at(14) as i32);
    let mut pos = u16_at(6) as usize;

    let mut av1c_rec = None;
    let mut frames = Vec::new();
    while pos + 12 <= data.len() {
        let size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let tu = data.get(pos + 12..pos + 12 + size).ok_or("truncated IVF frame")?;
        pos += 12 + size;
        let mut sample = Vec::with_capacity(size);
        for (ty, obu, payload) in obus(tu)? {
            if ty == OBU_TEMPORAL_DELIMITER {
                continue;
            }
            if ty == OBU_SEQUENCE_HEADER && av1c_rec.is_none() {
                av1c_rec = Some(av1c(payload, obu)?);
            }
            sample.extend_from_slice(obu);
        }
        frames.push(sample);
    }
    let av1c = av1c_rec.ok_or("no sequence header")?;
    Ok(Stream { width, height, av1c, frames })
}
