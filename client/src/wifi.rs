//! Keeps AWDL, the Wi-Fi side channel behind AirDrop, AirPlay and Handoff, switched off
//! while streaming: macOS hops channels for it every so often, and each hop is a burst
//! of added delay on the stream.
//!
//! Switching it needs root, so the app can't do it as the logged-in user. A one-time
//! setup (`install_rule`, from the Connection menu) adds a sudoers rule for exactly
//! two commands, `ifconfig awdl0 down` and `ifconfig awdl0 up`, and from then on this
//! runs them without a password. Remove it with `sudo rm /etc/sudoers.d/displaywire-awdl`.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::Duration;

const SUDO: &str = "/usr/bin/sudo";
const IFCONFIG: &str = "/sbin/ifconfig";
const RULE_FILE: &str = "/etc/sudoers.d/displaywire-awdl";
/// macOS brings AWDL back by itself now and then: put it down again this often.
const REASSERT: Duration = Duration::from_secs(5);

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn set(state: &str) -> bool {
    quiet(Command::new(SUDO).args(["-n", IFCONFIG, "awdl0", state]))
}

/// Whether the one-time setup has been done (sudo allows the commands without a password).
pub fn ready() -> bool {
    quiet(Command::new(SUDO).args(["-n", "-l", IFCONFIG, "awdl0", "down"]))
}

/// Adds the sudoers rule, asking for the administrator password once.
pub fn install_rule() -> Result<(), String> {
    let user = std::env::var("USER").map_err(|_| "no USER in the environment")?;
    if user.is_empty() || !user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(format!("won't write a rule for the user name {user:?}"));
    }
    // visudo checks the file before it is installed, so a mistake can't lock sudo out.
    let script = format!(
        "tmp=$(mktemp) && printf '%s\\n' '{user} ALL=(root) NOPASSWD: {IFCONFIG} awdl0 down, {IFCONFIG} awdl0 up' > \"$tmp\" \
         && /usr/sbin/visudo -cf \"$tmp\" && /usr/bin/install -m 0440 -o root -g wheel \"$tmp\" {RULE_FILE}; r=$?; rm -f \"$tmp\"; exit $r"
    );
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    let apple = format!(
        "do shell script \"{escaped}\" with prompt \"DisplayWire wants to pause AirDrop and AirPlay's Wi-Fi link while you stream.\" with administrator privileges"
    );
    let out = Command::new("/usr/bin/osascript").args(["-e", &apple]).output().map_err(|e| e.to_string())?;
    if out.status.success() && ready() {
        Ok(())
    } else {
        let why = String::from_utf8_lossy(&out.stderr);
        Err(if why.contains("-128") { "cancelled".to_string() } else { why.trim().to_string() })
    }
}

/// AWDL is held down for as long as this exists, and brought back when it is dropped.
pub struct Quiet {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Quiet {
    pub fn start() -> Quiet {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("wifi".into())
            .spawn(move || {
                while !flag.load(Relaxed) {
                    set("down");
                    let mut waited = Duration::ZERO;
                    while waited < REASSERT && !flag.load(Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        waited += Duration::from_millis(100);
                    }
                }
            })
            .ok();
        Quiet { stop, thread }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        set("up");
    }
}
