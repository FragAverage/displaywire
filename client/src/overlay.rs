//! Graph overlay: frame rate, bitrate and latency over the last 20 seconds.
//!
//! A panel of CoreAnimation layers above the video. Nothing is drawn while it
//! is hidden, and it never touches the decode/present path: the UI thread
//! redraws it a few times a second from the receiver's `History`.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSView;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGMutablePath};
use objc2_foundation::NSString;
use objc2_quartz_core::{
    CAAutoresizingMask, CALayer, CAShapeLayer, CATextLayer, CATransaction, kCAAlignmentLeft,
};
use timing::history::{Bin, History};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Bins shown: 20 s of 100 ms.
const SHOWN: usize = 200;
const MARGIN: f64 = 10.0;
const PAD: f64 = 8.0;
const PANEL_W: f64 = 300.0;
const GRAPH_W: f64 = PANEL_W - 2.0 * PAD;
const GRAPH_H: f64 = 38.0;
const LABEL_H: f64 = 14.0;
const GAP: f64 = 6.0;
const ROWS: usize = 3;
const ROW_H: f64 = LABEL_H + GRAPH_H + GAP;
const PANEL_H: f64 = ROWS as f64 * ROW_H + PAD;

type Rgba = (f64, f64, f64, f64);
const WHITE: Rgba = (1.0, 1.0, 1.0, 0.95);
const CYAN: Rgba = (0.35, 0.8, 1.0, 1.0);
const ORANGE: Rgba = (1.0, 0.65, 0.2, 1.0);
const GREEN: Rgba = (0.4, 0.9, 0.5, 1.0);
const RED: Rgba = (1.0, 0.3, 0.3, 1.0);

struct Row {
    label: Retained<CATextLayer>,
    lines: Vec<Retained<CAShapeLayer>>,
}

pub struct Overlay {
    panel: Retained<CALayer>,
    /// FPS (+ loss ticks), bitrate, latency.
    rows: [Row; ROWS],
}

fn color((r, g, b, a): Rgba) -> objc2_core_foundation::CFRetained<CGColor> {
    CGColor::new_generic_rgb(r, g, b, a)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

impl Overlay {
    /// Adds the panel (top left) to `window`'s content view. Main thread only.
    pub fn attach(window: &Window, visible: bool) -> Overlay {
        let RawWindowHandle::AppKit(handle) = window.window_handle().expect("window handle").as_raw() else {
            panic!("not an AppKit window");
        };
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let root = view.layer().expect("layer-backed view");
        let scale = window.scale_factor();

        let panel = CALayer::new();
        let top = root.bounds().size.height - PANEL_H - MARGIN;
        panel.setFrame(rect(MARGIN, top, PANEL_W, PANEL_H));
        // Stay in the top left when the window is resized (the origin is bottom left).
        panel.setAutoresizingMask(CAAutoresizingMask::LayerMinYMargin);
        panel.setBackgroundColor(Some(&color((0.0, 0.0, 0.0, 0.6))));
        panel.setCornerRadius(8.0);
        panel.setHidden(!visible);
        root.addSublayer(&panel);

        let colors: [&[Rgba]; ROWS] = [&[WHITE, RED], &[CYAN], &[ORANGE, WHITE, GREEN]];
        let rows = std::array::from_fn(|i| {
            // Row 0 is at the top.
            let y = PANEL_H - (i as f64 + 1.0) * ROW_H;
            let label = CATextLayer::new();
            label.setFrame(rect(PAD, y + GRAPH_H + GAP / 2.0, GRAPH_W, LABEL_H));
            label.setFontSize(11.0);
            label.setContentsScale(scale);
            label.setForegroundColor(Some(&color(WHITE)));
            label.setAlignmentMode(unsafe { kCAAlignmentLeft });
            panel.addSublayer(&label);
            let lines = colors[i]
                .iter()
                .map(|&c| {
                    let line = CAShapeLayer::new();
                    line.setFrame(rect(PAD, y, GRAPH_W, GRAPH_H));
                    line.setFillColor(None);
                    line.setStrokeColor(Some(&color(c)));
                    line.setLineWidth(1.0);
                    panel.addSublayer(&line);
                    line
                })
                .collect();
            Row { label, lines }
        });
        Overlay { panel, rows }
    }

    pub fn set_visible(&self, visible: bool) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.panel.setHidden(!visible);
        CATransaction::commit();
    }

    /// Redraws from `history`. Main thread only.
    pub fn update(&self, history: &History) {
        let bins: Vec<Bin> = history.iter().skip(history.len().saturating_sub(SHOWN)).copied().collect();
        let Some(last) = bins.last() else { return };
        let recent = &bins[bins.len().saturating_sub(10)..];
        let avg = |f: fn(&Bin) -> f32| recent.iter().map(f).sum::<f32>() / recent.len() as f32;
        let min = |f: fn(&Bin) -> f32| bins.iter().map(f).fold(f32::MAX, f32::min);
        let max = |f: fn(&Bin) -> f32| bins.iter().map(f).fold(0.0, f32::max);
        let lost: u32 = bins.iter().map(|b| b.lost).sum();

        CATransaction::begin();
        CATransaction::setDisableActions(true);

        let fps_scale = nice_ceil(max(|b| b.fps).max(30.0));
        set_text(&self.rows[0].label, &format!("FPS {:.0}   min {:.0}   lost {lost}", avg(|b| b.fps), min(|b| b.fps)));
        set_path(&self.rows[0].lines[0], &bins, |b| b.fps / fps_scale);
        set_ticks(&self.rows[0].lines[1], &bins);

        let mbps_scale = nice_ceil(max(|b| b.mbps).max(10.0));
        set_text(&self.rows[1].label, &format!("Bitrate {:.0} Mbps   peak {:.0}", avg(|b| b.mbps), max(|b| b.mbps)));
        set_path(&self.rows[1].lines[0], &bins, |b| b.mbps / mbps_scale);

        let ms_scale = nice_ceil(max(|b| b.latency_p99_ms).max(max(|b| b.rtt_ms)).max(10.0));
        set_text(
            &self.rows[2].label,
            &format!(
                "Latency {:.1} ms (p99 {:.1})   rtt {:.1}   decode {:.1}",
                avg(|b| b.latency_p50_ms),
                last.latency_p99_ms,
                last.rtt_ms,
                avg(|b| b.decode_ms)
            ),
        );
        set_path(&self.rows[2].lines[0], &bins, |b| b.latency_p99_ms / ms_scale);
        set_path(&self.rows[2].lines[1], &bins, |b| b.latency_p50_ms / ms_scale);
        set_path(&self.rows[2].lines[2], &bins, |b| b.rtt_ms / ms_scale);

        CATransaction::commit();
    }
}

/// Rounds up to 1, 2, 5 × 10^n so the scale doesn't jump around.
fn nice_ceil(v: f32) -> f32 {
    let mag = 10f32.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|&c| c >= v).unwrap_or(v)
}

fn set_text(layer: &CATextLayer, text: &str) {
    let s = NSString::from_str(text);
    unsafe { layer.setString(Some(&*(&*s as *const NSString as *const AnyObject))) };
}

fn x_at(i: usize, n: usize) -> f64 {
    // The newest bin is at the right edge.
    GRAPH_W - (n - 1 - i) as f64 * GRAPH_W / (SHOWN - 1) as f64
}

/// A polyline through `value(bin)` (0..=1 of the graph height).
fn set_path(layer: &CAShapeLayer, bins: &[Bin], value: impl Fn(&Bin) -> f32) {
    let path = CGMutablePath::new();
    for (i, b) in bins.iter().enumerate() {
        let (x, y) = (x_at(i, bins.len()), (value(b) as f64).clamp(0.0, 1.0) * GRAPH_H);
        unsafe {
            if i == 0 {
                CGMutablePath::move_to_point(Some(&path), std::ptr::null(), x, y);
            } else {
                CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), x, y);
            }
        }
    }
    layer.setPath(Some(&path));
}

/// A full-height tick wherever frames were lost.
fn set_ticks(layer: &CAShapeLayer, bins: &[Bin]) {
    let path = CGMutablePath::new();
    for i in (0..bins.len()).filter(|&i| bins[i].lost > 0) {
        let x = x_at(i, bins.len());
        unsafe {
            CGMutablePath::move_to_point(Some(&path), std::ptr::null(), x, 0.0);
            CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), x, GRAPH_H);
        }
    }
    layer.setPath(Some(&path));
}

#[cfg(test)]
mod tests {
    use super::nice_ceil;

    #[test]
    fn scales_round_up_to_1_2_5() {
        assert_eq!(nice_ceil(30.0), 50.0);
        assert_eq!(nice_ceil(120.0), 200.0);
        assert_eq!(nice_ceil(10.0), 10.0);
        assert_eq!(nice_ceil(61.0), 100.0);
        assert_eq!(nice_ceil(0.7), 1.0);
    }
}
