//! The notification-area icon: a tooltip with the connection status, a
//! balloon when a file arrives, and a small menu. It lives in the server
//! process in the user's session; things that should happen as the user
//! (opening folders, the device-management window) are launched as them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GWLP_USERDATA,
    GetCursorPos, GetMessageW, GetWindowLongPtrW, HICON, ICONINFO, MF_DISABLED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::Shared;

const WM_TRAY: u32 = WM_APP + 1;
const WM_BALLOON: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;
const TIMER_ID: usize = 1;

const CMD_OPEN_FILES: usize = 10;
const CMD_PAIR: usize = 11;
const CMD_MANAGE: usize = 12;
const CMD_UPDATE: usize = 13;
const CMD_CHECK: usize = 14;
const CMD_COPY_ADDR: usize = 20;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static BALLOON: Mutex<Option<String>> = Mutex::new(None);

struct State {
    shared: Arc<Shared>,
    exe: PathBuf,
    icon: HICON,
    /// Addresses clients can use, for the "copy address" items.
    addresses: Vec<String>,
    taskbar_created: u32,
}

/// Starts the tray icon on its own thread.
pub fn start(shared: Arc<Shared>) {
    let _ = std::thread::Builder::new().name("tray".into()).spawn(move || unsafe { run(shared) });
}

/// Shows a balloon notification, if the tray is up.
pub fn notify(text: &str) {
    *BALLOON.lock().unwrap() = Some(text.to_string());
    let hwnd = HWND_RAW.load(Relaxed);
    if hwnd != 0 {
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_BALLOON, WPARAM(0), LPARAM(0)) };
    }
}

fn wide<const N: usize>(s: &str) -> [u16; N] {
    let mut out = [0u16; N];
    for (o, c) in out.iter_mut().zip(s.encode_utf16().take(N - 1)) {
        *o = c;
    }
    out
}

/// A 32x32 icon drawn in code: a blue rounded square with a white "play" triangle.
unsafe fn make_icon() -> HICON {
    const S: i32 = 32;
    let mut px = vec![0u8; (S * S * 4) as usize];
    for y in 0..S {
        for x in 0..S {
            let (dx, dy) = ((x as f32 - 15.5).abs(), (y as f32 - 15.5).abs());
            let inside = dx.max(dy) < 15.0 && (dx - 8.0).max(0.0).hypot((dy - 8.0).max(0.0)) < 7.0;
            let tri = (11..=24).contains(&x) && (y as f32 - 15.5).abs() < (24.0 - x as f32) * 0.62 + 0.5;
            let (b, g, r, a) = if !inside {
                (0, 0, 0, 0)
            } else if tri {
                (255, 255, 255, 255)
            } else {
                // Blue, a little lighter towards the top.
                (230u8, (150 - y * 2) as u8, (50 + y) as u8, 255)
            };
            let i = ((y * S + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[b, g, r, a]);
        }
    }
    let color = unsafe { CreateBitmap(S, S, 1, 32, Some(px.as_ptr().cast())) };
    let mask = unsafe { CreateBitmap(S, S, 1, 1, None) };
    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = unsafe { CreateIconIndirect(&info) }.unwrap_or_default();
    unsafe {
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
    }
    icon
}

fn local_addresses() -> Vec<String> {
    // The routable addresses of this PC, found by asking the OS which interface reaches common destinations.
    let mut v = Vec::new();
    for target in ["192.168.1.1:9", "10.0.0.1:9", "100.100.100.100:9"] {
        if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
            && s.connect(target).is_ok()
            && let Ok(a) = s.local_addr()
        {
            let ip = a.ip().to_string();
            if !ip.starts_with("0.") && !v.contains(&ip) {
                v.push(ip);
            }
        }
    }
    v
}

unsafe fn run(shared: Arc<Shared>) {
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            println!("tray: no module handle");
            return;
        };
        let class = w!("DisplayWireTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: class, ..Default::default() };
        if RegisterClassW(&wc) == 0 {
            println!("tray: can't register the window class: {}", windows::core::Error::from_thread());
            return;
        }
        let hwnd = match CreateWindowExW(WS_EX_TOOLWINDOW, class, w!("DisplayWire"), WS_POPUP, 0, 0, 0, 0, None, None, Some(module.into()), None) {
            Ok(h) => h,
            Err(e) => {
                println!("tray: can't create the window: {e}");
                return;
            }
        };
        println!("tray: icon window ready");
        HWND_RAW.store(hwnd.0 as isize, Relaxed);
        let state = Box::new(State {
            shared,
            exe: std::env::current_exe().unwrap_or_default(),
            icon: make_icon(),
            addresses: local_addresses(),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
        SetTimer(Some(hwnd), TIMER_ID, 1000, None);
        add_icon(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a State> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() }
}

fn data(hwnd: HWND, flags: windows::Win32::UI::Shell::NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ICON_ID, uFlags: flags, ..Default::default() }
}

fn tooltip(st: &State) -> String {
    match st.shared.link.lock().unwrap().as_ref() {
        Some(link) => {
            let fps = st.shared.fps.load(Relaxed);
            let mbps = st.shared.mbps_x10.load(Relaxed) as f32 / 10.0;
            format!("DisplayWire Server\nConnected: {}\n{fps} fps, {mbps:.0} Mbps", link.addr.ip())
        }
        None => "DisplayWire Server\nWaiting for a client".to_string(),
    }
}

unsafe fn add_icon(hwnd: HWND) {
    let Some(st) = (unsafe { state(hwnd) }) else { return };
    let mut nid = data(hwnd, NIF_ICON | NIF_MESSAGE | NIF_TIP);
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = st.icon;
    nid.szTip = wide(&tooltip(st));
    if unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
        println!("tray: icon added");
    } else {
        println!("tray: the shell didn't take the icon yet ({}); will retry when it restarts", windows::core::Error::from_thread());
    }
}

/// When the window was last started, so a second click doesn't start another while the first is still opening.
static UI_LAUNCHED_NS: AtomicU64 = AtomicU64::new(0);

/// Shows the device-management window: brings the open one forward, or starts it as the logged-in user.
fn open_ui(st: &State) {
    let shared = &st.shared;
    if shared.ui_open.load(Relaxed) > 0 {
        shared.ui_front.store(true, Relaxed);
        return;
    }
    let now = timing::now_ns();
    if now.saturating_sub(UI_LAUNCHED_NS.load(Relaxed)) < 5_000_000_000 {
        return;
    }
    UI_LAUNCHED_NS.store(now, Relaxed);
    let Some(handle) = shared.ui_handle.get() else { return };
    let (port, token) = (handle.port.to_string(), handle.token.clone());
    let cmd = format!("\"{}\" --ui {port} {token}", st.exe.display());
    println!("ui: starting the device window");
    if let Err(e) = super::shell::spawn_as_user(&cmd) {
        // Not running as a service (no other user to launch as): start it as ourselves.
        println!("ui: can't start the window as the logged-in user ({e}); starting it as this user");
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new(&st.exe).args(["--ui", &port, &token]).creation_flags(0x0800_0000).spawn();
    }
}

/// A device asking to pair opens the window if it isn't open.
fn auto_open(st: &State) {
    use crate::pairing::View;
    if matches!(st.shared.pairing.snapshot(timing::now_ns()).view, View::Asking { .. } | View::Pin { .. }) {
        open_ui(st);
    }
}

/// Asks, then downloads and installs the update (on its own thread: the tray keeps running).
fn update_dialog() {
    let _ = std::thread::Builder::new().name("update-install".into()).spawn(|| {
        let Some(a) = super::updater::available() else { return };
        let ask = format!(
            "DisplayWire {} is available (this is {}).\n\nThe update is downloaded and checked, then the server restarts. A stream in progress is dropped for a few seconds.\n\nInstall it now?",
            a.version,
            super::updater::VERSION
        );
        if message(&ask, "DisplayWire update", true) {
            notify("Downloading the update…");
            if let Err(e) = super::updater::install(&a) {
                message(&format!("The update didn't install: {e}"), "DisplayWire update", false);
            }
        }
    });
}

/// A message box on the user's desktop; true if they chose Yes.
fn message(text: &str, title: &str, question: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW};
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let flags = MB_SETFOREGROUND | MB_TOPMOST | if question { MB_YESNO | MB_ICONQUESTION } else { MB_OK | MB_ICONINFORMATION };
    unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), flags) == IDYES }
}

unsafe fn update_tooltip(hwnd: HWND) {
    let Some(st) = (unsafe { state(hwnd) }) else { return };
    auto_open(st);
    let mut nid = data(hwnd, NIF_TIP);
    nid.szTip = wide(&tooltip(st));
    // If the icon went missing (the shell restarted before we noticed), put it back.
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool() {
        unsafe { add_icon(hwnd) };
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Some(st) = (unsafe { state(hwnd) }) else { return };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let title = wide::<128>(&tooltip(st).replace('\n', " | "));
        let _ = AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, PCWSTR(title.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        for (i, a) in st.addresses.iter().enumerate() {
            let text = wide::<64>(&format!("Copy address {a}"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_COPY_ADDR + i, PCWSTR(text.as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_FILES, w!("Open received files"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_PAIR, w!("Pair new device…"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_MANAGE, w!("Manage devices…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        match super::updater::available() {
            Some(a) => {
                let text = wide::<64>(&format!("Update to {}…", a.version));
                let _ = AppendMenuW(menu, MF_STRING, CMD_UPDATE, PCWSTR(text.as_ptr()));
            }
            None => {
                let text = wide::<64>(&format!("Check for updates (this is {})", super::updater::VERSION));
                let _ = AppendMenuW(menu, MF_STRING, CMD_CHECK, PCWSTR(text.as_ptr()));
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        if cmd.0 != 0 {
            let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(cmd.0 as usize), LPARAM(0));
        }
    }
}

unsafe fn command(hwnd: HWND, cmd: usize) {
    let Some(st) = (unsafe { state(hwnd) }) else { return };
    match cmd {
        CMD_OPEN_FILES => {
            if let Ok(dir) = super::shell::uploads_dir() {
                let _ = super::shell::as_user(|| std::fs::create_dir_all(&dir));
                let _ = super::shell::spawn_as_user(&format!("explorer.exe \"{}\"", dir.display()));
            }
        }
        CMD_PAIR => {
            st.shared.pairing.start(timing::now_ns());
            open_ui(st);
        }
        CMD_MANAGE => open_ui(st),
        CMD_UPDATE => update_dialog(),
        CMD_CHECK => {
            let _ = std::thread::Builder::new().name("update-check".into()).spawn(|| match super::updater::check() {
                Ok(Some(a)) => notify(&format!("DisplayWire {} is available: right-click this icon to install it.", a.version)),
                Ok(None) => notify(&format!("DisplayWire {} is the latest version.", super::updater::VERSION)),
                Err(e) => notify(&format!("Can't check for updates: {e}")),
            });
        }
        c if c >= CMD_COPY_ADDR => {
            if let Some(a) = st.addresses.get(c - CMD_COPY_ADDR) {
                super::shell::set_clipboard_text(a);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER if wparam.0 == TIMER_ID => update_tooltip(hwnd),
            WM_TRAY => {
                let event = (lparam.0 & 0xFFFF) as u32;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU || event == WM_LBUTTONUP {
                    show_menu(hwnd);
                }
            }
            WM_BALLOON => {
                if let Some(text) = BALLOON.lock().unwrap().take() {
                    let mut nid = data(hwnd, NIF_INFO);
                    nid.szInfoTitle = wide("DisplayWire");
                    nid.szInfo = wide(&text);
                    nid.dwInfoFlags = NIIF_INFO;
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                }
            }
            WM_COMMAND => command(hwnd, wparam.0 & 0xFFFF),
            WM_DESTROY => {
                let nid = data(hwnd, windows::Win32::UI::Shell::NOTIFY_ICON_DATA_FLAGS(0));
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                PostQuitMessage(0);
            }
            m if state(hwnd).is_some_and(|s| m == s.taskbar_created) => add_icon(hwnd),
            _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
        }
        LRESULT(0)
    }
}
