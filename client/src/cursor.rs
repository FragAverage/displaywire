//! The PC's pointer drawn as this Mac's own cursor (`CAP_CURSOR`): it moves with the mouse at
//! no extra latency instead of a round trip behind it, and takes the PC's shape (I-beam, resize
//! arrows, a game's hidden pointer). Images arrive in pieces (`Msg::CursorShape`), are put back
//! together here, and are scaled to the size the PC would show them at in the window.

use std::collections::HashMap;
use std::sync::Arc;

use proto::stream::CursorShape;

pub struct Image {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub hot: (u32, u32),
    pub rgba: Vec<u8>,
}

/// Receiver side: pieces → images.
#[derive(Default)]
pub struct Assembler {
    /// Image id → bytes so far, and which pieces (by offset) have arrived.
    partial: HashMap<u32, (CursorShape, Vec<u8>, Vec<u32>)>,
    /// The last image completed, so its periodic resends are ignored.
    done: Option<u32>,
}

impl Assembler {
    pub fn push(&mut self, shape: CursorShape, payload: &[u8]) -> Option<Arc<Image>> {
        if self.done == Some(shape.id) {
            return None;
        }
        // Only the newest image is worth finishing.
        self.partial.retain(|&id, _| id == shape.id);
        let total = shape.width as usize * shape.height as usize * 4;
        let (first, data, offsets) = self.partial.entry(shape.id).or_insert_with(|| (shape, vec![0; total], Vec::new()));
        if (first.width, first.height) != (shape.width, shape.height) || offsets.contains(&shape.offset) {
            return None;
        }
        let at = shape.offset as usize;
        data[at..at + payload.len()].copy_from_slice(payload);
        offsets.push(shape.offset);
        let pieces = total.div_ceil(proto::stream::CURSOR_PAYLOAD);
        if offsets.len() < pieces {
            return None;
        }
        let (s, rgba, _) = self.partial.remove(&shape.id)?;
        self.done = Some(s.id);
        Some(Arc::new(Image { id: s.id, width: s.width as u32, height: s.height as u32, hot: (s.hot_x as u32, s.hot_y as u32), rgba }))
    }
}

/// `image` resized by `factor` (nearest neighbour; pointers are small and hard-edged). Returns the
/// RGBA, its size and the hotspot.
pub fn scaled(image: &Image, factor: f64) -> (Vec<u8>, u16, u16, u16, u16) {
    let w = ((image.width as f64 * factor).round() as u32).clamp(1, 256);
    let h = ((image.height as f64 * factor).round() as u32).clamp(1, 256);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let sy = (y * image.height / h).min(image.height - 1);
        for x in 0..w {
            let sx = (x * image.width / w).min(image.width - 1);
            let (d, s) = (((y * w + x) * 4) as usize, ((sy * image.width + sx) * 4) as usize);
            out[d..d + 4].copy_from_slice(&image.rgba[s..s + 4]);
        }
    }
    let hot = |v: u32, size: u32, scaled: u32| ((v as f64 * scaled as f64 / size as f64) as u32).min(scaled - 1) as u16;
    (out, w as u16, h as u16, hot(image.hot.0, image.width, w), hot(image.hot.1, image.height, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::stream::CURSOR_PAYLOAD;

    #[test]
    fn pieces_in_any_order_make_one_image_once() {
        let shape = CursorShape { id: 5, width: 32, height: 32, hot_x: 3, hot_y: 4, offset: 0 };
        let rgba: Vec<u8> = (0..32 * 32 * 4).map(|i| i as u8).collect();
        let pieces: Vec<(u32, &[u8])> = rgba.chunks(CURSOR_PAYLOAD).enumerate().map(|(i, c)| ((i * CURSOR_PAYLOAD) as u32, c)).collect();
        let mut a = Assembler::default();
        let mut done = None;
        for &(offset, chunk) in pieces.iter().rev() {
            assert!(done.is_none());
            done = a.push(CursorShape { offset, ..shape }, chunk);
        }
        let img = done.expect("complete");
        assert_eq!((img.width, img.hot, &img.rgba), (32, (3, 4), &rgba));
        // A resend of the same image is ignored.
        assert!(a.push(shape, pieces[0].1).is_none());
    }

    #[test]
    fn scaling_keeps_the_hotspot_in_place() {
        let img = Image { id: 1, width: 32, height: 32, hot: (16, 8), rgba: vec![255; 32 * 32 * 4] };
        let (rgba, w, h, hx, hy) = scaled(&img, 0.5);
        assert_eq!((w, h, hx, hy, rgba.len()), (16, 16, 8, 4, 16 * 16 * 4));
    }
}
