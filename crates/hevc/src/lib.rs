//! HEVC Annex B helpers: NAL splitting, access units, and conversion to the
//! length-prefixed samples VideoToolbox wants. `h264` has the few H.264 equivalents.

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

/// A NAL's payload without emulation prevention bytes (00 00 03 → 00 00).
fn rbsp(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Picture size from an SPS NAL (with its 2-byte header), after the conformance window crop.
pub fn sps_size(sps: &[u8]) -> Option<(u32, u32)> {
    let rbsp = rbsp(sps.get(2..)?);
    let mut r = Bits { data: &rbsp, pos: 0 };
    r.skip(4); // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.skip(1); // temporal_id_nesting
    // profile_tier_level: general profile (88 bits) and level (8), then per sub-layer flags and fields.
    r.skip(96);
    let mut present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        present.push((r.bits(1)? == 1, r.bits(1)? == 1));
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize));
    }
    for (profile, level) in present {
        r.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 });
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        r.skip(1); // separate_colour_plane_flag
    }
    let (mut width, mut height) = (r.ue()?, r.ue()?);
    if r.bits(1)? == 1 {
        let (sub_w, sub_h) = match chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        width = width.checked_sub(sub_w * (left + right))?;
        height = height.checked_sub(sub_h * (top + bottom))?;
    }
    Some((width, height))
}

/// Big-endian bit reader with Exp-Golomb codes.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = v << 1 | (byte >> (7 - self.pos % 8) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

/// The same for H.264 streams (one-byte NAL headers), for clients that can only decode H.264.
pub mod h264 {
    use super::{Bits, nals, rbsp};

    pub const IDR: u8 = 5;
    pub const SPS: u8 = 7;

    pub fn nal_type(nal: &[u8]) -> u8 {
        nal[0] & 0x1f
    }

    /// True if the access unit holds an IDR picture.
    pub fn is_keyframe(au: &[u8]) -> bool {
        nals(au).iter().any(|n| nal_type(n) == IDR)
    }

    /// Picture size from an SPS NAL (with its header), after frame cropping.
    pub fn sps_size(sps: &[u8]) -> Option<(u32, u32)> {
        let rbsp = rbsp(sps.get(1..)?);
        let mut r = Bits { data: &rbsp, pos: 0 };
        let profile_idc = r.bits(8)?;
        r.skip(16); // constraint flags, level_idc
        r.ue()?; // seq_parameter_set_id
        let mut chroma_format_idc = 1;
        if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile_idc) {
            chroma_format_idc = r.ue()?;
            if chroma_format_idc == 3 {
                r.skip(1); // separate_colour_plane_flag
            }
            r.ue()?; // bit_depth_luma_minus8
            r.ue()?; // bit_depth_chroma_minus8
            r.skip(1); // qpprime_y_zero_transform_bypass_flag
            if r.bits(1)? == 1 {
                // seq_scaling_matrix_present_flag: skip each list that is present.
                for i in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
                    if r.bits(1)? == 1 {
                        let size = if i < 6 { 16 } else { 64 };
                        let (mut last, mut next) = (8i32, 8i32);
                        for _ in 0..size {
                            if next != 0 {
                                next = (last + r.se()?).rem_euclid(256);
                            }
                            last = if next == 0 { last } else { next };
                        }
                    }
                }
            }
        }
        r.ue()?; // log2_max_frame_num_minus4
        match r.ue()? {
            0 => {
                r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
            }
            1 => {
                r.skip(1); // delta_pic_order_always_zero_flag
                r.se()?; // offset_for_non_ref_pic
                r.se()?; // offset_for_top_to_bottom_field
                for _ in 0..r.ue()? {
                    r.se()?; // offset_for_ref_frame
                }
            }
            _ => {}
        }
        r.ue()?; // max_num_ref_frames
        r.skip(1); // gaps_in_frame_num_value_allowed_flag
        let width_mbs = r.ue()? + 1;
        let height_units = r.ue()? + 1;
        let frame_mbs_only = r.bits(1)?;
        if frame_mbs_only == 0 {
            r.skip(1); // mb_adaptive_frame_field_flag
        }
        r.skip(1); // direct_8x8_inference_flag
        let mut width = width_mbs * 16;
        let mut height = (2 - frame_mbs_only) * height_units * 16;
        if r.bits(1)? == 1 {
            let (sub_w, sub_h) = match chroma_format_idc {
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
            width = width.checked_sub(sub_w * (left + right))?;
            height = height.checked_sub(sub_h * (2 - frame_mbs_only) * (top + bottom))?;
        }
        Some((width, height))
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

    #[test]
    fn sps_size_of_a_real_nvenc_stream() {
        // 2560x1440 HEVC Main from NVENC (as sent to the webOS client).
        let sps = [
            0x42, 0x01, 0x01, 0x21, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xa0, 0x01,
            0x40, 0x20, 0x05, 0xa1, 0x65, 0x97, 0x4a, 0x42, 0x11, 0x91, 0x75, 0x43, 0x01, 0x6a, 0x02, 0x02, 0x02, 0x08, 0x00, 0x00,
            0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x03, 0xc0, 0x40,
        ];
        assert_eq!(sps_size(&sps), Some((2560, 1440)));
        assert_eq!(sps_size(&sps[..10]), None);
    }

    #[test]
    fn h264_sps_size_with_cropping() {
        // 1920x1080 High profile from x264 (coded as 1088 lines, cropped).
        let sps = [
            0x67, 0x64, 0x00, 0x33, 0xac, 0xb4, 0x03, 0xc0, 0x11, 0x3f, 0x2e, 0x02, 0xd4, 0x04, 0x04, 0x05, 0x00, 0x00, 0x03, 0x00,
            0x01, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x8f, 0x18, 0x32, 0xa0,
        ];
        assert_eq!(h264::sps_size(&sps), Some((1920, 1080)));
        assert_eq!(h264::sps_size(&sps[..6]), None);
    }
}
