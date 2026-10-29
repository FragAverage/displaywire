//! The macOS menu bar. Standard items use the responder chain (quit, hide,
//! full screen, minimise); ours go through a small Objective-C target that
//! forwards them to the app's event loop.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSObject, NSString};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Connect,
    Disconnect,
    ToggleStats,
    Settings,
    Mute,
    Shortcuts,
}

static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

fn send(action: Action) {
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(UserEvent::Menu(action));
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class has no ivars or Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StreamyMenuTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(connect:))]
        fn connect(&self, _sender: Option<&AnyObject>) {
            send(Action::Connect);
        }

        #[unsafe(method(disconnect:))]
        fn disconnect(&self, _sender: Option<&AnyObject>) {
            send(Action::Disconnect);
        }

        #[unsafe(method(toggleStats:))]
        fn toggle_stats(&self, _sender: Option<&AnyObject>) {
            send(Action::ToggleStats);
        }

        #[unsafe(method(settings:))]
        fn settings(&self, _sender: Option<&AnyObject>) {
            send(Action::Settings);
        }

        #[unsafe(method(mute:))]
        fn mute(&self, _sender: Option<&AnyObject>) {
            send(Action::Mute);
        }

        #[unsafe(method(shortcuts:))]
        fn shortcuts(&self, _sender: Option<&AnyObject>) {
            send(Action::Shortcuts);
        }
    }
);

fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<Sel>,
    key: &str,
    mods: NSEventModifierFlags,
    target: Option<&AnyObject>,
) -> Retained<NSMenuItem> {
    let it = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), action, &NSString::from_str(key))
    };
    it.setKeyEquivalentModifierMask(mods);
    if let Some(t) = target {
        unsafe { it.setTarget(Some(t)) };
    }
    it
}

fn submenu(mtm: MainThreadMarker, title: &str, items: Vec<Retained<NSMenuItem>>) -> Retained<NSMenuItem> {
    let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
    for i in &items {
        menu.addItem(i);
    }
    let holder = NSMenuItem::new(mtm);
    holder.setSubmenu(Some(&menu));
    holder
}

/// Replaces the menu bar. Main thread only, once the app has finished launching.
pub fn install(mtm: MainThreadMarker, proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(proxy);
    let target: Retained<Target> = unsafe { msg_send![super(mtm.alloc::<Target>().set_ivars(())), init] };
    let t: &AnyObject = &target;
    let none = NSEventModifierFlags::empty();
    let cmd = NSEventModifierFlags::Command;
    let sep = || NSMenuItem::separatorItem(mtm);

    let app_menu = submenu(
        mtm,
        "Streamy",
        vec![
            item(mtm, "About Streamy", Some(sel!(orderFrontStandardAboutPanel:)), "", none, None),
            item(mtm, "Settings…", Some(sel!(settings:)), ",", cmd, Some(t)),
            sep(),
            item(mtm, "Hide Streamy", Some(sel!(hide:)), "h", cmd, None),
            item(mtm, "Hide Others", Some(sel!(hideOtherApplications:)), "h", cmd | NSEventModifierFlags::Option, None),
            item(mtm, "Show All", Some(sel!(unhideAllApplications:)), "", none, None),
            sep(),
            item(mtm, "Quit Streamy", Some(sel!(terminate:)), "q", cmd, None),
        ],
    );
    let connection = submenu(
        mtm,
        "Connection",
        vec![
            item(mtm, "Connect…", Some(sel!(connect:)), "", none, Some(t)),
            item(mtm, "Disconnect", Some(sel!(disconnect:)), "", none, Some(t)),
        ],
    );
    let view = submenu(
        mtm,
        "View",
        vec![
            item(mtm, "Enter Full Screen", Some(sel!(toggleFullScreen:)), "f", cmd | NSEventModifierFlags::Control, None),
            item(mtm, "Show Stats", Some(sel!(toggleStats:)), "s", NSEventModifierFlags::Control | NSEventModifierFlags::Option, Some(t)),
            item(mtm, "Mute Audio", Some(sel!(mute:)), "", none, Some(t)),
        ],
    );
    let window = submenu(
        mtm,
        "Window",
        vec![
            item(mtm, "Minimize", Some(sel!(performMiniaturize:)), "m", cmd, None),
            item(mtm, "Zoom", Some(sel!(performZoom:)), "", none, None),
        ],
    );
    let help = submenu(mtm, "Help", vec![item(mtm, "Keyboard Shortcuts", Some(sel!(shortcuts:)), "", none, Some(t))]);

    let bar = NSMenu::new(mtm);
    for m in [&app_menu, &connection, &view, &window, &help] {
        bar.addItem(m);
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&bar));
    // AppKit doesn't retain a menu item's target: keep ours for the life of the app.
    std::mem::forget(target);
}
