//! Installing and removing the server: copy it to Program Files, register the
//! service and firewall rule, add an entry to "Installed apps". Double-clicking
//! the exe offers this; `--setup` and `--uninstall` are the elevated steps.

use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

const SERVICE: &str = "StreamyServer";
const UNINSTALL_KEY: &str = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\StreamyServer";

pub fn is_installed() -> bool {
    Command::new("sc").args(["query", SERVICE]).output().is_ok_and(|o| o.status.success())
}

fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenElevation, Some((&raw mut elevation).cast()), size_of::<TOKEN_ELEVATION>() as u32, &mut len).is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Runs this exe again with `args`, asking for administrator rights (a UAC prompt).
fn relaunch_elevated(args: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let args: Vec<u16> = args.encode_utf16().chain(Some(0)).collect();
    let r = unsafe { ShellExecuteW(None, w!("runas"), PCWSTR(exe.as_ptr()), PCWSTR(args.as_ptr()), PCWSTR::null(), SW_SHOWNORMAL) };
    if r.0 as usize > 32 { Ok(()) } else { Err("administrator permission was not given".into()) }
}

fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into())).join("Streamy")
}

fn run(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).output();
}

/// The elevated install: needs administrator rights.
pub fn setup() -> Result<(), String> {
    let dir = install_dir();
    let dest = dir.join("streamy-server.exe");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    if me != dest {
        // An older copy may be running as the service.
        run("sc", &["stop", SERVICE]);
        std::thread::sleep(std::time::Duration::from_millis(2500));
        std::fs::copy(&me, &dest).map_err(|e| format!("copy to {}: {e}", dest.display()))?;
    }
    println!("installing the service from {}", dest.display());
    let status = Command::new(&dest).arg("--install-service").status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("the service could not be installed".into());
    }
    let version = env!("CARGO_PKG_VERSION");
    let uninstall = format!("\"{}\" --uninstall --pause", dest.display());
    let entries: [(&str, &str, &str); 7] = [
        ("DisplayName", "REG_SZ", "Streamy Server"),
        ("DisplayVersion", "REG_SZ", version),
        ("Publisher", "REG_SZ", "Streamy"),
        ("InstallLocation", "REG_SZ", &dir.display().to_string()),
        ("DisplayIcon", "REG_SZ", &dest.display().to_string()),
        ("UninstallString", "REG_SZ", &uninstall),
        ("NoModify", "REG_DWORD", "1"),
    ];
    for (name, ty, value) in entries {
        run("reg", &["add", UNINSTALL_KEY, "/v", name, "/t", ty, "/d", value, "/f"]);
    }
    println!("\nStreamy Server is installed and running. To remove it: Settings > Apps > Installed apps, or --uninstall.");
    Ok(())
}

/// Removes the service, firewall rule and install folder. `purge` also deletes the
/// pairing key and logs.
pub fn uninstall(purge: bool) -> Result<(), String> {
    run("sc", &["stop", SERVICE]);
    std::thread::sleep(std::time::Duration::from_millis(2000));
    run("sc", &["delete", SERVICE]);
    run("netsh", &["advfirewall", "firewall", "delete", "rule", "name=Streamy Server"]);
    run("reg", &["delete", UNINSTALL_KEY, "/f"]);
    // This exe may be the one being deleted: finish the job from a helper once it has exited.
    let mut paths = vec![install_dir()];
    if purge {
        paths.push(PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("Streamy"));
    }
    let remove: Vec<String> = paths.iter().map(|p| format!("rmdir /s /q \"{}\"", p.display())).collect();
    // `ping` is the delay (`timeout` needs a console); raw_arg keeps cmd's own quoting intact.
    let _ = Command::new("cmd").raw_arg(format!("/c ping -n 4 127.0.0.1 >nul & {}", remove.join(" & "))).spawn();
    println!("Streamy Server has been removed.{}", if purge { "" } else { " Its pairing key and logs were kept in ProgramData\\Streamy." });
    Ok(())
}

fn ask(prompt: &str) -> String {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim().to_lowercase()
}

/// What running the exe with no arguments in a console does. Returns true to
/// carry on and run the server in this window.
pub fn interactive() -> bool {
    if !std::io::stdin().is_terminal() {
        return true; // started by the service or another program: just run
    }
    println!("Streamy Server {}\n", env!("CARGO_PKG_VERSION"));
    if is_installed() {
        println!("Streamy Server is installed as a Windows service.");
        println!("  u = uninstall it    k = show the pairing key    r = run a copy in this window instead    Enter = close");
        match ask("> ").as_str() {
            "u" => finish(if is_elevated() { uninstall(false) } else { relaunch_elevated("--uninstall --pause") }, false),
            "k" => finish(relaunch_elevated("--show-key --pause"), false),
            "r" => return true,
            _ => {}
        }
        return false;
    }
    println!("Streamy Server streams this PC's screen to your Mac. Installing it as a Windows service starts it with");
    println!("Windows and lets it show UAC prompts and the login screen. It needs administrator permission.\n");
    match ask("Install now? [Y]es / [n]o / [r]un once in this window: ").as_str() {
        "" | "y" | "yes" => {
            if is_elevated() {
                finish(setup(), true);
            } else {
                finish(relaunch_elevated("--setup --pause"), false);
                println!("Continue in the window that just opened.");
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            false
        }
        "r" | "run" => true,
        _ => false,
    }
}

fn finish(result: Result<(), String>, pause: bool) {
    if let Err(e) = result {
        eprintln!("error: {e}");
    }
    if pause {
        println!("\nPress Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}
