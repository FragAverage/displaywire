//! The window you start in: a tile for every PC this Mac knows (and any found on the network),
//! with whether it is online. One click connects; a PC not paired yet starts pairing. Right-click
//! a tile to rename or forget it. Built from plain AppKit views.

use std::cell::RefCell;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBorderType, NSButton, NSColor, NSFont, NSImage, NSImageView, NSLineBreakMode, NSMenu, NSMenuItem, NSScrollView,
    NSTextField, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

/// What a click in the window asks for.
#[derive(Clone, Debug)]
pub enum Action {
    Connect(String),
    /// Pair with a PC that has no key here yet.
    Pair(String),
    Add,
    Rename(String),
    Forget(String),
    /// The window was closed.
    Closed,
}

/// One tile.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub address: String,
    pub name: String,
    /// This Mac has a key for it.
    pub paired: bool,
    /// Whether it answered just now, `None` before the first check.
    pub online: Option<bool>,
    /// Remembered, as opposed to only seen on the network.
    pub saved: bool,
}

static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

thread_local! {
    static ROWS: RefCell<Vec<Row>> = const { RefCell::new(Vec::new()) };
}

fn send(action: Action) {
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(UserEvent::Launcher(action));
    }
}

fn row(tag: isize) -> Option<Row> {
    ROWS.with(|r| r.borrow().get(tag as usize).cloned())
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class has no ivars or Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DisplayWireLauncherTarget"]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSWindowDelegate for Target {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            send(Action::Closed);
        }
    }

    impl Target {
        #[unsafe(method(card:))]
        fn card(&self, sender: &NSButton) {
            if let Some(r) = row(sender.tag()) {
                send(if r.paired { Action::Connect(r.address) } else { Action::Pair(r.address) });
            }
        }

        #[unsafe(method(add:))]
        fn add(&self, _sender: Option<&AnyObject>) {
            send(Action::Add);
        }

        #[unsafe(method(rename:))]
        fn rename(&self, sender: &NSMenuItem) {
            if let Some(r) = row(sender.tag()) {
                send(Action::Rename(r.address));
            }
        }

        #[unsafe(method(forget:))]
        fn forget(&self, sender: &NSMenuItem) {
            if let Some(r) = row(sender.tag()) {
                send(Action::Forget(r.address));
            }
        }
    }
);

const WIN_W: f64 = 756.0;
const WIN_H: f64 = 560.0;
const HEADER_H: f64 = 86.0;
const CARD_W: f64 = 224.0;
const CARD_H: f64 = 136.0;
const GAP: f64 = 16.0;
const COLS: usize = 3;
const PAD: f64 = 24.0;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

pub struct Launcher {
    window: Retained<NSWindow>,
    scroll: Retained<NSScrollView>,
    target: Retained<Target>,
    shown: RefCell<Option<Vec<Row>>>,
}

impl Launcher {
    /// Main thread only.
    pub fn new(mtm: MainThreadMarker, proxy: EventLoopProxy<UserEvent>) -> Launcher {
        let _ = PROXY.set(proxy);
        let target: Retained<Target> = unsafe { msg_send![super(mtm.alloc::<Target>().set_ivars(())), init] };
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(0.0, 0.0, WIN_W, WIN_H),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("DisplayWire"));
        unsafe { window.setReleasedWhenClosed(false) };
        window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*target)));
        let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIN_W, WIN_H));

        let title = NSTextField::labelWithString(&NSString::from_str("Your PCs"), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(24.0)));
        title.setFrame(rect(PAD, WIN_H - 52.0, 400.0, 30.0));
        content.addSubview(&title);
        let sub = NSTextField::labelWithString(&NSString::from_str("Click a PC to connect."), mtm);
        sub.setTextColor(Some(&NSColor::secondaryLabelColor()));
        sub.setFrame(rect(PAD, WIN_H - 76.0, 400.0, 18.0));
        content.addSubview(&sub);
        let add = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str("Add PC…"), Some(&target), Some(sel!(add:)), mtm) };
        add.setFrame(rect(WIN_W - PAD - 100.0, WIN_H - 58.0, 100.0, 30.0));
        content.addSubview(&add);

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, WIN_W, WIN_H - HEADER_H));
        scroll.setHasVerticalScroller(true);
        scroll.setBorderType(NSBorderType::NoBorder);
        scroll.setDrawsBackground(false);
        content.addSubview(&scroll);
        window.setContentView(Some(&content));
        window.center();
        Launcher { window, scroll, target, shown: RefCell::new(None) }
    }

    pub fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
        if let Some(mtm) = MainThreadMarker::new() {
            objc2_app_kit::NSApplication::sharedApplication(mtm).activate();
        }
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }

    /// Redraws the tiles, unless they are the ones already showing (so a click in progress isn't interrupted).
    pub fn set_rows(&self, rows: Vec<Row>) {
        if self.shown.borrow().as_ref() == Some(&rows) {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else { return };
        ROWS.with(|r| *r.borrow_mut() = rows.clone());
        let n = rows.len().max(1);
        let grid_rows = n.div_ceil(COLS);
        let view_h = (grid_rows as f64 * (CARD_H + GAP) + PAD).max(WIN_H - HEADER_H);
        let doc = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIN_W, view_h));
        if rows.is_empty() {
            let hint = NSTextField::wrappingLabelWithString(
                &NSString::from_str("No PCs yet. Click “Add PC…”, then choose “Pair new device…” from the DisplayWire icon in the PC's tray."),
                mtm,
            );
            hint.setTextColor(Some(&NSColor::secondaryLabelColor()));
            hint.setFrame(rect(PAD, view_h - 80.0, WIN_W - 2.0 * PAD, 40.0));
            doc.addSubview(&hint);
        }
        for (i, r) in rows.iter().enumerate() {
            let (col, line) = (i % COLS, i / COLS);
            let x = PAD + col as f64 * (CARD_W + GAP);
            let y = view_h - PAD - (line as f64 + 1.0) * CARD_H - line as f64 * GAP;
            doc.addSubview(&self.card(mtm, i, r, rect(x, y, CARD_W, CARD_H)));
        }
        self.scroll.setDocumentView(Some(&doc));
        // Show the top.
        if let Some(clip) = self.scroll.contentView().into() {
            let clip: Retained<objc2_app_kit::NSClipView> = clip;
            clip.scrollToPoint(NSPoint::new(0.0, (view_h - (WIN_H - HEADER_H)).max(0.0)));
            self.scroll.reflectScrolledClipView(&clip);
        }
        *self.shown.borrow_mut() = Some(rows);
    }

    fn card(&self, mtm: MainThreadMarker, index: usize, r: &Row, frame: NSRect) -> Retained<NSView> {
        let card = NSView::initWithFrame(NSView::alloc(mtm), frame);
        card.setWantsLayer(true);
        if let Some(layer) = card.layer() {
            let layer: Retained<CALayer> = layer;
            layer.setBackgroundColor(Some(&NSColor::controlBackgroundColor().CGColor()));
            layer.setCornerRadius(12.0);
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&NSColor::separatorColor().CGColor()));
        }
        let (w, h) = (frame.size.width, frame.size.height);
        if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("desktopcomputer"), None) {
            let icon = NSImageView::imageViewWithImage(&img, mtm);
            icon.setFrame(rect(16.0, h - 56.0, 40.0, 40.0));
            icon.setContentTintColor(Some(&*if r.online == Some(true) { NSColor::controlAccentColor() } else { NSColor::tertiaryLabelColor() }));
            card.addSubview(&icon);
        }
        let name = NSTextField::labelWithString(&NSString::from_str(&r.name), mtm);
        name.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
        name.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        name.setFrame(rect(16.0, 50.0, w - 32.0, 20.0));
        card.addSubview(&name);
        let address = NSTextField::labelWithString(&NSString::from_str(&r.address), mtm);
        address.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        address.setTextColor(Some(&NSColor::secondaryLabelColor()));
        address.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        address.setFrame(rect(16.0, 32.0, w - 32.0, 16.0));
        card.addSubview(&address);
        let (text, color) = match (r.paired, r.online) {
            (false, _) => ("Not paired: click to pair", NSColor::systemOrangeColor()),
            (true, Some(true)) => ("● Online", NSColor::systemGreenColor()),
            (true, Some(false)) => ("○ Offline", NSColor::tertiaryLabelColor()),
            (true, None) => ("Checking…", NSColor::tertiaryLabelColor()),
        };
        let status = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        status.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        status.setTextColor(Some(&color));
        status.setFrame(rect(16.0, 12.0, w - 32.0, 16.0));
        card.addSubview(&status);

        // The whole tile is one invisible button, with the right-click menu.
        let button = NSButton::initWithFrame(NSButton::alloc(mtm), rect(0.0, 0.0, w, h));
        button.setTransparent(true);
        button.setTag(index as isize);
        unsafe {
            button.setTarget(Some(&self.target));
            button.setAction(Some(sel!(card:)));
        }
        if r.saved {
            let menu = NSMenu::new(mtm);
            for (title, action) in [("Rename…", sel!(rename:)), ("Forget this PC", sel!(forget:))] {
                let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), Some(action), &NSString::from_str("")) };
                item.setTag(index as isize);
                unsafe { item.setTarget(Some(&self.target)) };
                menu.addItem(&item);
            }
            unsafe { button.setMenu(Some(&menu)) };
        }
        card.addSubview(&button);
        card
    }
}
