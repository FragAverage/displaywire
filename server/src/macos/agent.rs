//! Starting the server at login: a LaunchAgent in `~/Library/LaunchAgents`, so it runs in the
//! user's session (ScreenCaptureKit and input injection need one) and is restarted if it stops.
//! There's no streaming of the login window: that would need a system daemon.

use std::path::PathBuf;
use std::process::Command;

pub const LABEL: &str = "app.displaywire.server";

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "no home directory".into())
}

fn plist_path() -> Result<PathBuf, String> {
    Ok(home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

pub fn log_path() -> Result<PathBuf, String> {
    Ok(home()?.join("Library/Logs/DisplayWire/server.log"))
}

fn domain() -> Result<String, String> {
    let out = Command::new("/usr/bin/id").arg("-u").output().map_err(|e| format!("id: {e}"))?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&out.stdout).trim()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn installed() -> bool {
    plist_path().is_ok_and(|p| p.exists())
}

/// Whether this process is the one launchd started.
/// The app opened from Finder (no arguments): the one thing a user runs. It makes sure the
/// LaunchAgent exists and is running (which starts the server now and at every login, and
/// opens the permissions window while they're missing), then this process leaves it to that.
/// Returns false to run normally (not from an app bundle, or already the agent).
pub fn first_launch() -> bool {
    if running_as_agent() {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    if !exe.ancestors().any(|p| p.extension().is_some_and(|e| e == "app")) {
        return false;
    }
    let uid = unsafe { getuid() };
    let target = format!("gui/{uid}/{LABEL}");
    let running = std::process::Command::new("/bin/launchctl").args(["print", &target]).output().is_ok_and(|o| {
        o.status.success() && String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim_start().starts_with("pid = "))
    });
    // An agent for another copy of the app (moved, or an older install) is replaced.
    let ours = plist_path().ok().and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|plist| plist.contains(&*exe.canonicalize().unwrap_or(exe.clone()).to_string_lossy()));
    if running && ours {
        // Already running in the menu bar: opening the app again shows the permissions.
        super::setup::open();
        return true;
    }
    if installed() {
        let _ = uninstall(false);
    }
    match install(&[], true) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("start at login: {e}; running here instead");
            false
        }
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

pub fn running_as_agent() -> bool {
    std::env::var("XPC_SERVICE_NAME").is_ok_and(|n| n == LABEL)
}

/// Writes the LaunchAgent (running this executable with `args`); `start_now` also starts it
/// (not from a server that's already running: two can't share the port).
pub fn install(args: &[std::ffi::OsString], start_now: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let log = log_path()?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let mut program = vec![exe.display().to_string(), "--run".into()];
    program.extend(args.iter().map(|a| a.to_string_lossy().into_owned()).filter(|a| a != "--run"));
    let program: String = program.iter().map(|a| format!("\n    <string>{}</string>", xml_escape(a))).collect();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>{program}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(&log.display().to_string())
    );
    let path = plist_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, plist).map_err(|e| format!("write {}: {e}", path.display()))?;
    if !start_now {
        return Ok(());
    }
    // Replace a running one.
    let domain = domain()?;
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    let out = Command::new("/bin/launchctl").args(["bootstrap", &domain]).arg(&path).output().map_err(|e| format!("launchctl: {e}"))?;
    if !out.status.success() {
        return Err(format!("launchctl bootstrap: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    println!("DisplayWire now starts at login ({}); its log is {}", path.display(), log.display());
    println!("The first time, macOS asks to allow Screen & System Audio Recording and Accessibility for it.");
    Ok(())
}

/// Stops the LaunchAgent and removes it. `keep_running`: leave this process alone (the menu's
/// "Start at Login" being turned off from inside the running server).
pub fn uninstall(keep_running: bool) -> Result<(), String> {
    let path = plist_path()?;
    if !keep_running {
        let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{}/{LABEL}", domain()?)]).output();
    }
    match std::fs::remove_file(&path) {
        Ok(()) => println!("DisplayWire no longer starts at login"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("DisplayWire wasn't set to start at login"),
        Err(e) => return Err(format!("remove {}: {e}", path.display())),
    }
    Ok(())
}
