//! Starting at login: a LaunchAgent (`~/Library/LaunchAgents/app.displaywire.server.plist`) that
//! runs this binary in the user's session, restarts it if it crashes (not when it's quit), and
//! sends its output to `~/Library/Logs/DisplayWire/server.log`.

use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "app.displaywire.server";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

pub fn log_path() -> PathBuf {
    home().join("Library/Logs/DisplayWire/server.log")
}

pub fn installed() -> bool {
    plist_path().exists()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Writes the LaunchAgent for this binary with `args`; it takes effect at the next login.
pub fn install_with(args: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find this program: {e}"))?;
    let log = log_path();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, plist(&exe.display().to_string(), args, &log.display().to_string())).map_err(|e| format!("{}: {e}", path.display()))
}

fn plist(exe: &str, args: &[String], log: &str) -> String {
    let program: String = std::iter::once(exe.to_string()).chain(args.iter().cloned()).map(|a| format!("        <string>{}</string>\n", escape(&a))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{program}    </array>
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
        log = escape(log)
    )
}

/// From the menu: this binary as it was started, at every login.
pub fn install() -> Result<(), String> {
    install_with(&std::env::args().skip(1).collect::<Vec<_>>())
}

/// Stops starting at login (this instance keeps running).
pub fn uninstall() -> Result<(), String> {
    match std::fs::remove_file(plist_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", plist_path().display())),
        _ => Ok(()),
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

fn domain() -> String {
    format!("gui/{}", unsafe { getuid() })
}

/// `--install-service`: writes the LaunchAgent and starts it now.
pub fn install_and_start(args: &[String]) -> Result<(), String> {
    install_with(args)?;
    // Replace a running one, if any.
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).status();
    let status = Command::new("/bin/launchctl").args(["bootstrap", &domain()]).arg(plist_path()).status().map_err(|e| format!("launchctl: {e}"))?;
    if !status.success() {
        return Err(format!("launchctl bootstrap failed ({status})"));
    }
    println!("installed: DisplayWire Server runs now and at every login (log: {})", log_path().display());
    Ok(())
}

/// `--uninstall-service`: stops it and removes the LaunchAgent.
pub fn stop_and_uninstall() -> Result<(), String> {
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).status();
    uninstall()?;
    println!("uninstalled: DisplayWire Server no longer starts at login");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_launch_agent_is_a_valid_plist() {
        let text = super::plist("/Applications/DW & Co/displaywire-server", &["--port".into(), "9000".into()], "/Users/x/Library/Logs/DisplayWire/server.log");
        let path = std::env::temp_dir().join(format!("dw-agent-{}.plist", std::process::id()));
        std::fs::write(&path, &text).unwrap();
        let lint = std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&path).output().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        assert!(text.contains("<string>/Applications/DW &amp; Co/displaywire-server</string>"));
    }
}
