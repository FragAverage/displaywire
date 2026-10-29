//! The pairing key on disk, and the connection to the one authenticated
//! client. See `docs/AUTH.md`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use secure::{Key, Session};

/// The authenticated client that video, audio and replies go to.
#[derive(Clone)]
pub struct Link {
    pub addr: SocketAddr,
    pub session: Arc<Session>,
}

impl Link {
    /// Encrypts `plain` and sends it. `out` is scratch space of `MAX_DATAGRAM` bytes.
    pub fn send(&self, sock: &std::net::UdpSocket, plain: &[u8], out: &mut [u8]) -> usize {
        let n = self.session.seal(plain, out);
        let _ = sock.send_to(&out[..n], self.addr);
        n
    }
}

pub fn default_key_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("Streamy").join("key")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".config/streamy/key")
    }
}

/// Reads the key, creating it first if there is none. Returns whether it was just created.
pub fn load_or_create(path: &Path) -> Result<(Key, bool), String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Key::parse(&text).map(|k| (k, false)).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_new(path).map(|k| (k, true)),
        Err(e) => Err(format!("read {}: {e} (run as administrator, or use --key-file)", path.display())),
    }
}

/// Replaces the key. Every paired client stops working until it gets the new one.
pub fn write_new(path: &Path) -> Result<Key, String> {
    let key = Key::generate();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e} (run as administrator, or use --key-file)", dir.display()))?;
    }
    // Written without the key readable by others first, then filled in.
    std::fs::write(path, "").map_err(|e| format!("write {}: {e}", path.display()))?;
    protect(path);
    std::fs::write(path, format!("{key}\n")).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(key)
}

#[cfg(windows)]
fn protect(path: &Path) {
    // Only SYSTEM and administrators may read the key.
    let _ = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant", "*S-1-5-18:F", "/grant", "*S-1-5-32-544:F"])
        .output();
}

#[cfg(not(windows))]
fn protect(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
