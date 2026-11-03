//! Graph overlay: frame rate, bitrate, latency and audio delay over the last 20 seconds.
//!
//! A panel of CoreAnimation layers above the video. Nothing is drawn while it
//! is hidden, and it never touches the decode/present path: the UI thread
//! redraws it a few times a second from the receiver's `History`.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions, NSProgressIndicator, NSProgressIndicatorStyle, NSView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGMutablePath};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CAAutoresizingMask, CALayer, CAShapeLayer, CATextLayer, CATransaction, kCAAlignmentCenter, kCAAlignmentLeft,
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
const ROWS: usize = 4;
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

/// A centred pill at the top of the window with a short status line
/// ("Connecting…"). Only real state goes here, never quality warnings.
pub struct Banner {
    pill: Retained<CALayer>,
    text: Retained<CATextLayer>,
}

const BANNER_W: f64 = 380.0;
const BANNER_H: f64 = 30.0;

impl Banner {
    /// Main thread only.
    pub fn attach(window: &Window) -> Banner {
        let RawWindowHandle::AppKit(handle) = window.window_handle().expect("window handle").as_raw() else {
            panic!("not an AppKit window");
        };
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let root = view.layer().expect("layer-backed view");
        let flipped = root.isGeometryFlipped();
        let bounds = root.bounds().size;
        let x = (bounds.width - BANNER_W) / 2.0;
        let y = if flipped { 12.0 } else { bounds.height - BANNER_H - 12.0 };
        let pill = CALayer::new();
        pill.setFrame(rect(x, y, BANNER_W, BANNER_H));
        pill.setGeometryFlipped(flipped);
        // Centred, and kept at the top, when the window is resized.
        let top = if flipped { CAAutoresizingMask::LayerMaxYMargin } else { CAAutoresizingMask::LayerMinYMargin };
        pill.setAutoresizingMask(top | CAAutoresizingMask::LayerMinXMargin | CAAutoresizingMask::LayerMaxXMargin);
        pill.setBackgroundColor(Some(&color((0.0, 0.0, 0.0, 0.7))));
        pill.setCornerRadius(BANNER_H / 2.0);
        pill.setHidden(true);
        let text = CATextLayer::new();
        text.setFrame(rect(0.0, 7.0, BANNER_W, BANNER_H - 7.0));
        text.setFontSize(13.0);
        text.setContentsScale(window.scale_factor());
        text.setForegroundColor(Some(&color(WHITE)));
        text.setAlignmentMode(unsafe { kCAAlignmentCenter });
        pill.addSublayer(&text);
        root.addSublayer(&pill);
        Banner { pill, text }
    }

    pub fn show(&self, message: &str) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        set_text(&self.text, message);
        self.pill.setHidden(false);
        CATransaction::commit();
    }

    pub fn hide(&self) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.pill.setHidden(true);
        CATransaction::commit();
    }
}

/// Fills the window until the first picture arrives: the PC's name, what is happening, and a
/// spinner, instead of a black window.
pub struct Placeholder {
    layer: Retained<CALayer>,
    detail: Retained<CATextLayer>,
    spinner: Retained<NSProgressIndicator>,
}

impl Placeholder {
    /// Main thread only.
    pub fn attach(window: &Window, title: &str) -> Placeholder {
        let RawWindowHandle::AppKit(handle) = window.window_handle().expect("window handle").as_raw() else {
            panic!("not an AppKit window");
        };
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let root = view.layer().expect("layer-backed view");
        let flipped = root.isGeometryFlipped();
        let bounds = root.bounds().size;
        let scale = window.scale_factor();
        let all = CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable;
        let centred = CAAutoresizingMask::LayerMinXMargin
            | CAAutoresizingMask::LayerMaxXMargin
            | CAAutoresizingMask::LayerMinYMargin
            | CAAutoresizingMask::LayerMaxYMargin;
        let layer = CALayer::new();
        layer.setFrame(rect(0.0, 0.0, bounds.width, bounds.height));
        layer.setAutoresizingMask(all);
        layer.setBackgroundColor(Some(&color((0.09, 0.09, 0.1, 1.0))));
        // Offsets from the centre, upwards on screen.
        let at = |up: f64, h: f64| {
            let y = bounds.height / 2.0 - h / 2.0 + if flipped { -up } else { up };
            rect(0.0, y, bounds.width, h)
        };
        let text = |size: f64, rgba: Rgba, up: f64| {
            let t = CATextLayer::new();
            t.setFrame(at(up, size + 8.0));
            t.setAutoresizingMask(CAAutoresizingMask::LayerWidthSizable | centred);
            t.setFontSize(size);
            t.setContentsScale(scale);
            t.setForegroundColor(Some(&color(rgba)));
            t.setAlignmentMode(unsafe { kCAAlignmentCenter });
            layer.addSublayer(&t);
            t
        };
        set_text(&text(22.0, WHITE, -4.0), title);
        let detail = text(13.0, (1.0, 1.0, 1.0, 0.55), -34.0);
        root.addSublayer(&layer);

        let mtm = MainThreadMarker::new().expect("main thread");
        let size = 32.0;
        let y = view.bounds().size.height / 2.0 - size / 2.0 + if view.isFlipped() { -40.0 } else { 40.0 };
        let spinner = NSProgressIndicator::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(view.bounds().size.width / 2.0 - size / 2.0, y), NSSize::new(size, size)),
        );
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        // Light on the dark background whatever the system appearance.
        if let Some(dark) = unsafe { NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua) } {
            spinner.setAppearance(Some(&dark));
        }
        view.addSubview(&spinner);
        unsafe { spinner.startAnimation(None) };
        Placeholder { layer, detail, spinner }
    }

    pub fn set_detail(&self, text: &str) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        set_text(&self.detail, text);
        CATransaction::commit();
    }
}

impl Drop for Placeholder {
    fn drop(&mut self) {
        unsafe { self.spinner.stopAnimation(None) };
        self.spinner.removeFromSuperview();
        self.layer.removeFromSuperlayer();
    }
}

pub struct Overlay {
    panel: Retained<CALayer>,
    /// FPS (+ loss ticks), bitrate, latency, audio delay.
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
        // AppKit may give us a flipped layer (origin top left) or not (bottom left).
        // Either way the panel sits in the top left and its own contents are y-up.
        let flipped = root.isGeometryFlipped();
        let top = if flipped { MARGIN } else { root.bounds().size.height - PANEL_H - MARGIN };
        panel.setFrame(rect(MARGIN, top, PANEL_W, PANEL_H));
        panel.setGeometryFlipped(flipped);
        panel.setAutoresizingMask(if flipped {
            CAAutoresizingMask::LayerMaxYMargin
        } else {
            CAAutoresizingMask::LayerMinYMargin
        });
        panel.setBackgroundColor(Some(&color((0.0, 0.0, 0.0, 0.6))));
        panel.setCornerRadius(8.0);
        panel.setHidden(!visible);
        root.addSublayer(&panel);

        let colors: [&[Rgba]; ROWS] = [&[WHITE, RED], &[CYAN], &[ORANGE, WHITE, GREEN], &[CYAN, ORANGE]];
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
        set_path(&self.rows[0].lines[0], &bins, |b| Some(b.fps / fps_scale));
        set_ticks(&self.rows[0].lines[1], &bins);

        let mbps_scale = nice_ceil(max(|b| b.mbps).max(10.0));
        set_text(&self.rows[1].label, &format!("Bitrate {:.0} Mbps   peak {:.0}", avg(|b| b.mbps), max(|b| b.mbps)));
        set_path(&self.rows[1].lines[0], &bins, |b| Some(b.mbps / mbps_scale));

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
        // Latency is only measured while frames arrive; an idle desktop leaves gaps.
        let when_frames = |b: &Bin, v: f32| (b.fps > 0.0).then_some(v / ms_scale);
        set_path(&self.rows[2].lines[0], &bins, |b| when_frames(b, b.latency_p99_ms));
        set_path(&self.rows[2].lines[1], &bins, |b| when_frames(b, b.latency_p50_ms));
        set_path(&self.rows[2].lines[2], &bins, |b| Some(b.rtt_ms / ms_scale));

        // Audio: total delay, and the cushion the player is aiming for. Gaps while silent.
        let audio_scale = nice_ceil(max(|b| b.audio_ms).max(20.0));
        set_text(
            &self.rows[3].label,
            &format!("Audio {:.0} ms   cushion {:.0}   peak {:.0}", avg(|b| b.audio_ms), avg(|b| b.audio_cushion_ms), max(|b| b.audio_ms)),
        );
        let when_audio = |b: &Bin, v: f32| (b.audio_ms > 0.0).then_some(v / audio_scale);
        set_path(&self.rows[3].lines[0], &bins, |b| when_audio(b, b.audio_ms));
        set_path(&self.rows[3].lines[1], &bins, |b| when_audio(b, b.audio_cushion_ms));

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

/// A polyline through `value(bin)` (0..=1 of the graph height); `None` leaves a gap.
fn set_path(layer: &CAShapeLayer, bins: &[Bin], value: impl Fn(&Bin) -> Option<f32>) {
    let path = CGMutablePath::new();
    let mut pen_down = false;
    for (i, b) in bins.iter().enumerate() {
        let Some(v) = value(b) else {
            pen_down = false;
            continue;
        };
        let (x, y) = (x_at(i, bins.len()), (v as f64).clamp(0.0, 1.0) * GRAPH_H);
        unsafe {
            if pen_down {
                CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), x, y);
            } else {
                CGMutablePath::move_to_point(Some(&path), std::ptr::null(), x, y);
            }
        }
        pen_down = true;
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
