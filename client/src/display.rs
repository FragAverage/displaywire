//! Presents decoded frames by handing their IOSurface straight to a
//! CoreAnimation layer: no copies, no shaders, colour handled by the system.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSColor, NSView};
use objc2_quartz_core::{CAAutoresizingMask, CALayer, CATransaction, kCAGravityResizeAspect};
use videotoolbox::Image;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

pub struct Layer(Retained<CALayer>);

// CALayer may be updated from any thread inside an explicit CATransaction.
unsafe impl Send for Layer {}

impl Layer {
    /// Adds a video layer filling `window`'s content view. Main thread only.
    pub fn attach(window: &Window) -> Layer {
        let RawWindowHandle::AppKit(handle) = window.window_handle().expect("window handle").as_raw() else {
            panic!("not an AppKit window");
        };
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        if let Some(w) = view.window() {
            w.setBackgroundColor(Some(&NSColor::blackColor()));
        }
        view.setWantsLayer(true);
        let root = view.layer().expect("layer-backed view");
        let layer = CALayer::new();
        layer.setFrame(root.bounds());
        layer.setAutoresizingMask(CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable);
        layer.setContentsGravity(unsafe { kCAGravityResizeAspect });
        root.addSublayer(&layer);
        Layer(layer)
    }

    /// Shows `image`. The caller keeps `image` alive until the next call.
    pub fn show(&self, image: &Image) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        unsafe { self.0.setContents(Some(&*(image.iosurface() as *const AnyObject))) };
        CATransaction::commit();
    }
}
