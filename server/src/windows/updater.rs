//! Updating the server from GitHub releases. The check, signature and download are in the `update` crate. Installing
//! runs the new exe's own `--setup` (the same step as a fresh install: stop the service, replace the files, start it),
//! from a detached process, since the service stopping ends this one.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use update::{Available, Platform};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

static AVAILABLE: Mutex<Option<Available>> = Mutex::new(None);

/// A newer version found by the last check.
pub fn available() -> Option<Available> {
    AVAILABLE.lock().unwrap().clone()
}

/// Checks now and remembers the result.
pub fn check() -> Result<Option<Available>, String> {
    let found = update::check(VERSION, Platform::Windows)?;
    *AVAILABLE.lock().unwrap() = found.clone();
    Ok(found)
}

/// Checks a minute after start and then twice a day, telling the user (once per version) when there is something new.
pub fn start() {
    if std::env::var_os("DISPLAYWIRE_NO_UPDATE_CHECK").is_some() {
        return;
    }
    let _ = std::thread::Builder::new().name("update-check".into()).spawn(|| {
        let mut told = String::new();
        std::thread::sleep(Duration::from_secs(60));
        loop {
            match check() {
                Ok(Some(a)) if a.version != told => {
                    println!("update: {} is available", a.version);
                    super::tray::notify(&format!("DisplayWire {} is available: right-click this icon to install it.", a.version));
                    told = a.version;
                }
                Ok(_) => {}
                Err(e) => println!("update: can't check: {e}"),
            }
            std::thread::sleep(Duration::from_secs(12 * 3600));
        }
    });
}

fn dir() -> PathBuf {
    crate::auth::default_dir().join("update")
}

/// Downloads and checks `a`, then starts the install. This process is stopped by it, so there is no return to speak of.
pub fn install(a: &Available) -> Result<(), String> {
    let dir = dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let zip = dir.join("update.zip");
    println!("update: downloading {}", a.version);
    update::download(a, &zip)?;
    let out = dir.join("new");
    let tar = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join(r"System32\tar.exe");
    let status = Command::new(tar).arg("-xf").arg(&zip).arg("-C").arg(&out_dir(&out)?).creation_flags(CREATE_NO_WINDOW).status().map_err(|e| format!("can't unpack the update: {e}"))?;
    let exe = out.join("displaywire-server-windows").join("displaywire-server.exe");
    if !status.success() || !exe.is_file() {
        return Err("the downloaded update is not a DisplayWire server".into());
    }
    let _ = std::fs::remove_file(&zip);
    let log = std::fs::File::create(dir.join("update.log")).map_err(|e| e.to_string())?;
    println!("update: installing {}", a.version);
    // Not part of the service's job, so stopping the service doesn't end the install half way.
    let spawn = |flags: u32| {
        Command::new(&exe)
            .arg("--setup")
            .stdin(Stdio::null())
            .stdout(log.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
            .stderr(log.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
            .creation_flags(flags)
            .spawn()
    };
    spawn(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(DETACHED_PROCESS)).map_err(|e| format!("can't start the install: {e}"))?;
    Ok(())
}

fn out_dir(out: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    Ok(out.to_path_buf())
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// `--check-update` and `--update`: for a console, for example over SSH.
pub fn command(install_it: bool) -> Result<(), String> {
    match check()? {
        None => println!("DisplayWire Server {VERSION} is the latest version."),
        Some(a) => {
            println!("DisplayWire Server {} is available (this is {VERSION}).", a.version);
            if install_it {
                install(&a)?;
                println!("Installing: the server restarts in a few seconds. The log is in {}.", dir().join("update.log").display());
            } else {
                println!("Run with --update to install it (needs administrator rights).");
            }
        }
    }
    Ok(())
}
