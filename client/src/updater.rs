//! Updating DisplayWire.app from GitHub releases (the checking, signature and download are in the `update` crate).
//! Installing swaps the app bundle in place once this process has quit, then opens the new one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use update::{Available, Platform};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::dialog;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version to compare against. Debug builds can pretend to be older, to test an update.
fn current() -> String {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("DISPLAYWIRE_FAKE_VERSION") {
        return v;
    }
    VERSION.to_string()
}
const EVERY: Duration = Duration::from_secs(24 * 3600);

/// What a check found. `manual` ones were asked for, so "no update" and errors are reported.
pub struct Found {
    pub manual: bool,
    pub result: Result<Option<Available>, String>,
}

fn stamp() -> Option<PathBuf> {
    dialog::state_file("update-checked")
}

fn due() -> bool {
    if std::env::var_os("DISPLAYWIRE_NO_UPDATE_CHECK").is_some() {
        return false;
    }
    let last = stamp().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
    last.is_none_or(|t| SystemTime::now().duration_since(t).unwrap_or_default() > EVERY)
}

/// Looks for an update in the background, at most once a day unless `manual`.
pub fn spawn_check(proxy: EventLoopProxy<UserEvent>, manual: bool) {
    if !manual && !due() {
        return;
    }
    let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
        let result = update::check(&current(), Platform::Macos);
        if result.is_ok()
            && let Some(p) = stamp()
        {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, VERSION);
        }
        let _ = proxy.send_event(UserEvent::UpdateFound(Found { manual, result }));
    });
}

/// The `.app` this process is running from.
fn bundle() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
        .ok_or_else(|| "this copy isn't running from an app, so it can't update itself".to_string())
}

const SWAP: &str = r#"
pid=$1; app=$2; new=$3; stage=$4
i=0
while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 150 ]; do sleep 0.2; i=$((i+1)); done
old="$stage/previous.app"
if mv "$app" "$old"; then
  if mv "$new" "$app"; then rm -rf "$stage"; else mv "$old" "$app"; fi
fi
open "$app"
"#;

/// Downloads and checks the new version and sets up the swap. On `Ok` the caller quits; the swap and relaunch follow.
pub fn prepare(a: &Available) -> Result<(), String> {
    let app = bundle()?;
    let parent = app.parent().ok_or("no folder for the app")?;
    let stage = parent.join(".DisplayWire-update");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir(&stage).map_err(|e| format!("can't write to {}: {e}", parent.display()))?;
    let result = (|| {
        let zip = stage.join("update.zip");
        update::download(a, &zip)?;
        let out = stage.join("new");
        let status = Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(&zip).arg(&out).status().map_err(|e| e.to_string())?;
        let new = out.join("DisplayWire.app");
        if !status.success() || !new.join("Contents/MacOS/displaywire-client").is_file() {
            return Err("the downloaded update is not a DisplayWire app".to_string());
        }
        let _ = std::fs::remove_file(&zip);
        // Anything we fetched ourselves isn't quarantined, but be sure.
        let _ = Command::new("/usr/bin/xattr").args(["-cr"]).arg(&new).status();
        Command::new("/bin/sh")
            .args(["-c", SWAP, "sh", &std::process::id().to_string()])
            .arg(&app)
            .arg(&new)
            .arg(&stage)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}
