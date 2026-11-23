//! HEVC Annex B helpers: NAL splitting, access units, and conversion to the
//! length-prefixed samples VideoToolbox wants.

pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;
pub const AUD: u8 = 35;

pub fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3f
}

/// Offsets of every start code: (start of the 00 00 [00] 01 prefix, first NAL byte).
fn start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let prefix = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            out.push((prefix, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// NAL units of an Annex B stream, without start codes or trailing zeros.
pub fn nals(data: &[u8]) -> Vec<&[u8]> {
    let starts = start_codes(data);
    let mut out = Vec::with_capacity(starts.len());
    for (n, &(_, s)) in starts.iter().enumerate() {
        let mut e = starts.get(n + 1).map_or(data.len(), |&(prefix, _)| prefix);
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Splits a stream into access units at access unit delimiters. Each slice
/// keeps its start codes.
pub fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let auds: Vec<usize> = start_codes(data)
        .into_iter()
        .filter(|&(_, s)| s < data.len() && nal_type(&data[s..]) == AUD)
        .map(|(prefix, _)| prefix)
        .collect();
    auds.iter()
        .enumerate()
        .map(|(n, &a)| &data[a..auds.get(n + 1).copied().unwrap_or(data.len())])
        .collect()
}

/// True if the access unit holds an IRAP picture (IDR/CRA/BLA), i.e. decoding can start here.
pub fn is_keyframe(au: &[u8]) -> bool {
    nals(au).iter().any(|n| (16..=21).contains(&nal_type(n)))
}

pub struct Sample {
    /// VPS, SPS, PPS when the access unit carries all three.
    pub param_sets: Option<[Vec<u8>; 3]>,
    /// VCL NALs, each prefixed by a 4-byte big-endian length.
    pub data: Vec<u8>,
}

pub fn to_sample(au: &[u8]) -> Sample {
    let mut ps: [Option<Vec<u8>>; 3] = Default::default();
    let mut data = Vec::with_capacity(au.len());
    for nal in nals(au) {
        match nal_type(nal) {
            t @ (VPS | SPS | PPS) => ps[(t - VPS) as usize] = Some(nal.to_vec()),
            0..=31 => {
                data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                data.extend_from_slice(nal);
            }
            _ => {}
        }
    }
    let [v, s, p] = ps;
    let param_sets = match (v, s, p) {
        (Some(v), Some(s), Some(p)) => Some([v, s, p]),
        _ => None,
    };
    Sample { param_sets, data }
}

/// Reads an SPS NAL (with its two header bytes) as far as the picture size: the visible width and
/// height in pixels, after the conformance window's cropping. `None` if it doesn't parse.
pub fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    // Drop the emulation prevention bytes (00 00 03 → 00 00).
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &b in sps.get(2..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { data: &rbsp, at: 0 };
    r.skip(4)?;
    let sub_layers = r.bits(3)? as usize;
    r.skip(1)?;
    // profile_tier_level: the general part, then what each sub-layer says it carries.
    r.skip(88 + 8)?;
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((r.bits(1)? == 1, r.bits(1)? == 1));
    }
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers))?;
    }
    for (profile, level) in present {
        r.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
    }
    r.ue()?;
    let chroma = r.ue()?;
    if chroma == 3 {
        r.skip(1)?;
    }
    let (mut width, mut height) = (r.ue()?, r.ue()?);
    if r.bits(1)? == 1 {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (sub_w, sub_h) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        width = width.checked_sub(sub_w * (left.checked_add(right)?))?;
        height = height.checked_sub(sub_h * (top.checked_add(bottom)?))?;
    }
    (width > 0 && height > 0 && width <= 16_384 && height <= 16_384).then_some((width, height))
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            v = (v << 1) | ((byte >> (7 - self.at % 8)) & 1) as u32;
            self.at += 1;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.at += n;
        (self.at <= self.data.len() * 8).then_some(())
    }

    /// An unsigned Exp-Golomb number.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NAL header byte = type << 1.
    fn nal(ty: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, ty << 1, 1];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn reads_the_picture_size_from_an_sps() {
        // From NVENC at 1920x1080: coded as 1920x1088, cropped by the conformance window.
        let hex = "420101216000000300900000030000030096a003c0801107cb96274a425909f5550c05a808080820000003002000000f01";
        let sps: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        assert_eq!(sps_dimensions(&sps), Some((1920, 1080)));
        assert_eq!(sps_dimensions(&sps[..8]), None, "cut short");
        assert_eq!(sps_dimensions(&[]), None);
    }

    #[test]
    fn splits_and_converts() {
        let mut stream = Vec::new();
        for part in [nal(AUD, &[0x50]), nal(VPS, &[1]), nal(SPS, &[2]), nal(PPS, &[3]), nal(19, &[4, 5, 0])] {
            stream.extend(part);
        }
        stream.extend(nal(AUD, &[0x50]));
        stream.extend([0, 0, 1, 1 << 1, 1, 6]); // 3-byte start code, TRAIL_R slice

        let aus = access_units(&stream);
        assert_eq!(aus.len(), 2);
        assert!(is_keyframe(aus[0]));
        assert!(!is_keyframe(aus[1]));

        let s = to_sample(aus[0]);
        let ps = s.param_sets.expect("param sets");
        assert_eq!(ps[1], vec![SPS << 1, 1, 2]);
        // Trailing zero of the slice is stripped (it belongs to the next start code).
        assert_eq!(s.data, vec![0, 0, 0, 4, 19 << 1, 1, 4, 5]);

        let s = to_sample(aus[1]);
        assert!(s.param_sets.is_none());
        assert_eq!(s.data, vec![0, 0, 0, 3, 1 << 1, 1, 6]);
    }
}
