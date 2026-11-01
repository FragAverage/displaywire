//! Runs the server as a Windows service.
//!
//! Capturing UAC prompts and the lock screen needs SYSTEM, and the capture
//! has to happen inside the session being shown. So the service itself only
//! supervises: it starts a copy of this exe with its own SYSTEM token moved
//! into the active session (the way Sunshine does it) and restarts it when
//! that session changes.
//!
//!   displaywire-server --install-service [server options]   # as administrator
//!   displaywire-server --uninstall-service

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSGetActiveConsoleSessionId,
};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, SetEvent, TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR, w};

const NAME: &str = "DisplayWireServer";
const SEC_MS: u32 = 1000;
/// A child that exits sooner than this after starting is retried only after `RESTART_DELAY_MS`.
const RESTART_DELAY_MS: u64 = 2000;
const NO_SESSION: u32 = 0xFFFF_FFFF;

static STATUS: AtomicIsize = AtomicIsize::new(0);
static WAKE: AtomicIsize = AtomicIsize::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn quote(a: &std::ffi::OsStr) -> String {
    format!("\"{}\"", a.to_string_lossy().replace('"', "\\\""))
}

fn sc(args: &[&str]) -> Result<String, String> {
    let out = Command::new("sc").args(args).output().map_err(|e| format!("sc.exe: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() { Ok(text) } else { Err(format!("sc {}: {text}", args.join(" "))) }
}

fn log_path() -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("DisplayWire");
    // Admins only, like the key next to it: the log names clients and may print a new key.
    if let Err(e) = crate::auth::lock_dir(&dir) {
        eprintln!("{e}");
    }
    dir.join("server.log")
}

/// Installs and starts the service. `args` are the server options to run it with.
pub fn install(args: &[OsString]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut bin = format!("{} --service", quote(exe.as_os_str()));
    for a in args {
        bin += &format!(" {}", quote(a));
    }
    // Replace any earlier install.
    let _ = sc(&["stop", NAME]);
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let _ = sc(&["delete", NAME]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    sc(&["create", NAME, "binPath=", &bin, "start=", "auto", "DisplayName=", "DisplayWire Server"])
        .map_err(|e| format!("{e}\n(run this from an administrator terminal)"))?;
    let _ = sc(&["description", NAME, "Streams this PC's screen to a DisplayWire client, including UAC prompts and the lock screen."]);
    let _ = sc(&["failure", NAME, "reset=", "86400", "actions=", "restart/5000"]);
    // udp/9000 and friends: allow the program itself.
    let _ = Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", "name=DisplayWire Server"])
        .output();
    let _ = Command::new("netsh")
        .args(["advfirewall", "firewall", "add", "rule", "name=DisplayWire Server", "dir=in", "action=allow", "protocol=UDP"])
        .arg(format!("program={}", exe.display()))
        .output();
    sc(&["start", NAME])?;
    println!("service {NAME} installed and started; it logs to {}", log_path().display());
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let _ = sc(&["stop", NAME]);
    std::thread::sleep(std::time::Duration::from_millis(1000));
    sc(&["delete", NAME]).map_err(|e| format!("{e}\n(run this from an administrator terminal)"))?;
    let _ = Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", "name=DisplayWire Server"])
        .output();
    println!("service {NAME} removed");
    Ok(())
}

/// Entry point when the service control manager starts us. Returns when the service stops.
pub fn run() -> Result<(), String> {
    let mut name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(|e| format!("not started by the service manager: {e}"))
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let handle = SERVICE_STATUS_HANDLE(STATUS.load(Relaxed) as *mut c_void);
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 5000,
    };
    let _ = unsafe { SetServiceStatus(handle, &status) };
}

unsafe extern "system" fn handler(control: u32, _event: u32, _data: *mut c_void, _ctx: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Relaxed);
            set_state(SERVICE_STOP_PENDING);
            let _ = unsafe { SetEvent(HANDLE(WAKE.load(Relaxed) as *mut c_void)) };
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            let _ = unsafe { SetEvent(HANDLE(WAKE.load(Relaxed) as *mut c_void)) };
        }
        SERVICE_CONTROL_INTERROGATE => {}
        _ => {}
    }
    0
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let Ok(status) = (unsafe { RegisterServiceCtrlHandlerExW(w!("DisplayWireServer"), Some(handler), None) }) else {
        return;
    };
    STATUS.store(status.0 as isize, Relaxed);
    set_state(SERVICE_START_PENDING);
    if let Ok(event) = unsafe { CreateEventW(None, false, false, None) } {
        WAKE.store(event.0 as isize, Relaxed);
    }
    set_state(SERVICE_RUNNING);
    supervise();
    set_state(SERVICE_STOPPED);
}

struct Child {
    process: HANDLE,
    session: u32,
    started_ms: u64,
}

fn now_ms() -> u64 {
    timing::now_ns() / 1_000_000
}

/// The session whose screen to stream: the console if someone is active
/// there, else any other active session (e.g. remote desktop), else the console.
fn target_session() -> Option<u32> {
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    let mut active = Vec::new();
    if unsafe { WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut sessions, &mut count) }.is_ok() {
        let list = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
        active = list.iter().filter(|s| s.State == WTSActive && s.SessionId != 0).map(|s| s.SessionId).collect();
        unsafe { WTSFreeMemory(sessions.cast()) };
    }
    if console != NO_SESSION && active.contains(&console) {
        return Some(console);
    }
    active.first().copied().or((console != NO_SESSION).then_some(console))
}

fn spawn(session: u32, args: &[OsString]) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // `--run`: this is the server itself, never the interactive installer.
    let mut cmdline = format!("{} --run", quote(exe.as_os_str()));
    for a in args {
        cmdline += &format!(" {}", quote(a));
    }
    let mut cmdline = wide(&cmdline);
    let mut desktop = wide("winsta0\\default");

    // Our SYSTEM token, moved into the target session.
    let mut own = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) }.map_err(|e| format!("OpenProcessToken: {e}"))?;
    let mut token = HANDLE::default();
    let dup = unsafe { DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token) };
    let _ = unsafe { CloseHandle(own) };
    dup.map_err(|e| format!("DuplicateTokenEx: {e}"))?;
    let result = (|| {
        unsafe { SetTokenInformation(token, TokenSessionId, (&raw const session).cast(), 4) }
            .map_err(|e| format!("SetTokenInformation: {e}"))?;

        // stdout/stderr → log file (the child has no console to show them on).
        let inheritable = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, bInheritHandle: true.into(), ..Default::default() };
        let log = wide(&log_path().to_string_lossy());
        let log = unsafe {
            CreateFileW(
                PCWSTR(log.as_ptr()),
                FILE_APPEND_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                Some(&inheritable),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| format!("open log: {e}"))?;

        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: log,
            hStdError: log,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let created = unsafe {
            CreateProcessAsUserW(
                Some(token),
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup,
                &mut info,
            )
        };
        let _ = unsafe { CloseHandle(log) };
        created.map_err(|e| format!("CreateProcessAsUser: {e}"))?;
        let _ = unsafe { CloseHandle(info.hThread) };
        Ok(Child { process: info.hProcess, session, started_ms: now_ms() })
    })();
    let _ = unsafe { CloseHandle(token) };
    result
}

fn kill(child: Child) {
    unsafe {
        let _ = TerminateProcess(child.process, 0);
        let _ = WaitForSingleObject(child.process, 2 * SEC_MS);
        let _ = CloseHandle(child.process);
    }
}

fn supervise() {
    // The server's own options, minus the flag that made this the service.
    let args: Vec<OsString> = std::env::args_os().skip(1).filter(|a| a != "--service").collect();
    let wake = HANDLE(WAKE.load(Relaxed) as *mut c_void);
    let mut child: Option<Child> = None;
    let mut retry_at = 0u64;
    while !STOP.load(Relaxed) {
        let want = target_session();
        if let Some(c) = &child {
            let exited = unsafe { WaitForSingleObject(c.process, 0) } == WAIT_OBJECT_0;
            if exited || Some(c.session) != want {
                let c = child.take().unwrap();
                if exited && now_ms() - c.started_ms < RESTART_DELAY_MS {
                    retry_at = now_ms() + RESTART_DELAY_MS;
                }
                kill(c);
            }
        }
        if child.is_none() && now_ms() >= retry_at && let Some(session) = want {
            match spawn(session, &args) {
                Ok(c) => child = Some(c),
                Err(e) => {
                    eprintln!("service: can't start the server in session {session}: {e}");
                    retry_at = now_ms() + RESTART_DELAY_MS;
                }
            }
        }
        unsafe { WaitForSingleObject(wake, SEC_MS) };
    }
    if let Some(c) = child {
        kill(c);
    }
}
