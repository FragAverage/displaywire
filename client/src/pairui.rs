//! The pairing window: asks the PC to pair, shows what is happening ("waiting for you to allow
//! this Mac on the PC", then a PIN prompt) and returns the key the PC hands over. A small
//! AppKit window run modally; a timer polls the `pairing` crate's progress.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSFloatingWindowLevel, NSBackingStoreType, NSButton, NSColor, NSFont, NSModalPanelRunLoopMode, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSDefaultRunLoopMode, NSObject, NSPoint, NSRect, NSRunLoop, NSSize, NSString, NSTimer};
use pairing::{Pairing, Progress};
use secure::Key;

pub enum Outcome {
    Paired { key: Key, server_name: String },
    Cancelled,
    Failed(String),
}

struct State {
    pairing: Pairing,
    window: Retained<NSWindow>,
    status: Retained<NSTextField>,
    pin: Retained<NSTextField>,
    button: Retained<NSButton>,
    shown: Option<Progress>,
    /// When a wrong PIN was reported, to say so for a moment before asking again.
    wrong_since: Option<Instant>,
    outcome: Option<Outcome>,
    submitted: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class has no ivars or Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DisplayWirePairTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: Option<&AnyObject>) {
            tick();
        }

        #[unsafe(method(cancel:))]
        fn cancel(&self, _sender: Option<&AnyObject>) {
            finish(None);
        }
    }
);

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn set(field: &NSTextField, text: &str) {
    field.setStringValue(&NSString::from_str(text));
}

fn computer_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "Mac".into())
}

/// Pairs with the PC at `address` (called `label` in the window). Blocks until it succeeds, fails or is cancelled.
pub fn pair(address: &str, label: &str) -> Outcome {
    let mtm = MainThreadMarker::new().expect("main thread");
    let server: SocketAddr = match transport::resolve(address, 9000) {
        Ok(a) => a,
        Err(e) => return Outcome::Failed(format!("can't find {address}: {e}")),
    };
    let pairing = match Pairing::start(server, &computer_name()) {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    let (w, h) = (400.0, 230.0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Pair with a PC"));
    unsafe { window.setReleasedWhenClosed(false) };
    let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, w, h));

    let title = NSTextField::labelWithString(&NSString::from_str(&format!("Pair with {label}")), mtm);
    title.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
    title.setFrame(rect(24.0, h - 52.0, w - 48.0, 24.0));
    content.addSubview(&title);

    let status = NSTextField::wrappingLabelWithString(&NSString::from_str("Contacting the PC…"), mtm);
    status.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    status.setTextColor(Some(&NSColor::secondaryLabelColor()));
    status.setFrame(rect(24.0, h - 112.0, w - 48.0, 52.0));
    content.addSubview(&status);

    let pin = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(w / 2.0 - 100.0, 64.0, 200.0, 44.0));
    pin.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(30.0, 0.3)));
    pin.setAlignment(NSTextAlignment::Center);
    pin.setPlaceholderString(Some(&NSString::from_str("000000")));
    pin.setHidden(true);
    content.addSubview(&pin);

    let target: Retained<Target> = unsafe { msg_send![super(mtm.alloc::<Target>().set_ivars(())), init] };
    let button = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str("Cancel"), Some(&target), Some(sel!(cancel:)), mtm) };
    button.setFrame(rect(w - 24.0 - 90.0, 18.0, 90.0, 28.0));
    content.addSubview(&button);
    window.setContentView(Some(&content));
    window.center();
    // Above other windows: this is something to deal with now, and it may be launched from behind them.
    window.setLevel(NSFloatingWindowLevel);

    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            pairing,
            window: window.clone(),
            status,
            pin,
            button,
            shown: None,
            wrong_since: None,
            outcome: None,
            submitted: false,
        });
    });
    let timer = unsafe { NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(0.1, &target, sel!(tick:), None, true) };
    let run_loop = NSRunLoop::currentRunLoop();
    unsafe {
        run_loop.addTimer_forMode(&timer, NSDefaultRunLoopMode);
        run_loop.addTimer_forMode(&timer, NSModalPanelRunLoopMode);
    }
    let app = NSApplication::sharedApplication(mtm);
    // Already the case inside the app; needed when run on its own (`--preview pair`), or the window never shows.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
    window.makeKeyAndOrderFront(None);
    app.runModalForWindow(&window);
    timer.invalidate();
    window.close();
    STATE.with(|s| s.borrow_mut().take()).and_then(|st| st.outcome).unwrap_or(Outcome::Cancelled)
}

/// Ends the modal session. `None` is the user cancelling.
fn finish(outcome: Option<Outcome>) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            if let Some(o) = outcome {
                st.outcome = Some(o);
            } else {
                st.pairing.cancel();
                st.outcome.get_or_insert(Outcome::Cancelled);
            }
        }
    });
    NSApplication::sharedApplication(mtm).stopModal();
}

fn tick() {
    let mut done: Option<Outcome> = None;
    STATE.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        let progress = st.pairing.progress();
        // The PIN box: digits only, six of them, sent as soon as the sixth is typed.
        // Debug builds only: type the PIN from a file named in the environment, so the flow can be tested without a keyboard.
        #[cfg(debug_assertions)]
        if matches!(progress, Progress::NeedPin)
            && let Ok(path) = std::env::var("DISPLAYWIRE_TEST_PIN_FILE")
            && let Ok(pin) = std::fs::read_to_string(path)
            && !pin.trim().is_empty()
        {
            set(&st.pin, pin.trim());
        }
        if matches!(progress, Progress::NeedPin) {
            let typed: String = st.pin.stringValue().to_string().chars().filter(char::is_ascii_digit).take(6).collect();
            if typed != st.pin.stringValue().to_string() {
                set(&st.pin, &typed);
            }
            if typed.len() == 6 && !st.submitted {
                st.submitted = true;
                st.pairing.submit_pin(&typed);
            }
        }
        if let (Progress::WrongPin { .. }, Some(t)) = (&progress, st.wrong_since)
            && t.elapsed() > Duration::from_millis(1500)
        {
            st.pairing.retry();
            st.wrong_since = None;
            st.submitted = false;
            set(&st.pin, "");
        }
        if st.shown.as_ref() == Some(&progress) {
            return;
        }
        st.shown = Some(progress.clone());
        let (text, show_pin, button) = match &progress {
            Progress::Contacting => ("Contacting the PC…".to_string(), false, "Cancel"),
            Progress::NoReply => (
                "No reply yet. On the PC, click the DisplayWire icon in the tray (bottom right), choose “Pair new device…”, and check the address is right.".to_string(),
                false,
                "Cancel",
            ),
            Progress::WaitingForAllow => ("Waiting for you to allow this Mac on the PC. A DisplayWire window there asks Allow or Deny.".to_string(), false, "Cancel"),
            Progress::NeedPin => ("Type the 6-digit PIN now shown on the PC.".to_string(), true, "Cancel"),
            Progress::Checking => ("Checking the PIN…".to_string(), true, "Cancel"),
            Progress::WrongPin { tries_left } => {
                st.wrong_since = Some(Instant::now());
                (format!("That PIN was wrong. {tries_left} {} left.", if *tries_left == 1 { "try" } else { "tries" }), true, "Cancel")
            }
            Progress::Denied => ("The PC denied the request.".to_string(), false, "Close"),
            Progress::Busy => ("Another device is pairing with this PC right now. Try again in a minute.".to_string(), false, "Close"),
            Progress::Locked => ("Pairing was stopped: too many wrong PINs, or the PIN expired. Start again from the PC (Pair new device…).".to_string(), false, "Close"),
            Progress::Cancelled => (String::new(), false, "Close"),
            Progress::Failed(e) => (e.clone(), false, "Close"),
            Progress::Done { .. } => ("Paired.".to_string(), false, "Close"),
        };
        set(&st.status, &text);
        st.pin.setHidden(!show_pin);
        st.button.setTitle(&NSString::from_str(button));
        if matches!(progress, Progress::NeedPin) {
            st.window.makeFirstResponder(Some(&st.pin));
        }
        match progress {
            Progress::Done { key, server_name } => done = Some(Outcome::Paired { key, server_name }),
            Progress::Cancelled => done = Some(Outcome::Cancelled),
            _ => {}
        }
    });
    if done.is_some() {
        finish(done);
    }
}
