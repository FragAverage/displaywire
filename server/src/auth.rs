//! Private files on disk (the paired devices), and the connection to the one
//! authenticated client. See `docs/AUTH.md` and `docs/PAIRING.md`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use secure::Session;

/// The authenticated client that video, audio and replies go to.
#[derive(Clone)]
pub struct Link {
    pub addr: SocketAddr,
    pub session: Arc<Session>,
    /// Which paired device this is (its id in `clients`).
    pub client: Arc<str>,
}

impl Link {
    /// Encrypts `plain` and sends it. `out` is scratch space of `MAX_DATAGRAM` bytes.
    pub fn send(&self, sock: &std::net::UdpSocket, plain: &[u8], out: &mut [u8]) -> usize {
        let n = self.session.seal(plain, out);
        let _ = sock.send_to(&out[..n], self.addr);
        n
    }
}

/// Where the server keeps its data: the paired devices (and, from earlier versions, the single shared key).
pub fn default_dir() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("DisplayWire")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".config/displaywire")
    }
}

/// Reads a private file, `None` if it isn't there.
pub fn read_private(path: &Path) -> Result<Option<String>, String> {
    guard(path)?;
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e} (run as administrator, or use --data-dir)", path.display())),
    }
}

/// Writes a private file, replacing any old one. Written under a temporary name that is already
/// private, then renamed, so the contents are never readable by others and a crash can't leave half a file.
pub fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    guard(path)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e} (run as administrator, or use --data-dir)", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, "").map_err(|e| format!("write {}: {e}", tmp.display()))?;
    protect(&tmp)?;
    std::fs::write(&tmp, contents).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))
}

/// Whether `path` is in the default data folder (as opposed to one chosen with `--data-dir`).
#[cfg(windows)]
fn in_default_dir(path: &Path) -> bool {
    path.parent() == Some(default_dir().as_path())
}

/// Before a file in the default folder is read or written, locks the folder down and makes
/// sure nobody but SYSTEM or an administrator put the file there. Otherwise a
/// standard user could create `ProgramData\DisplayWire\clients` before the server
/// is installed, and the server would accept their keys.
#[cfg(windows)]
fn guard(path: &Path) -> Result<(), String> {
    if !in_default_dir(path) {
        return Ok(()); // --data-dir: wherever the administrator chose
    }
    if let Some(dir) = path.parent() {
        lock_dir(dir).map_err(|e| format!("{e} (run as administrator, or use --data-dir)"))?;
    }
    if path.exists() && !acl::owned_by_admins(path)? {
        return Err(format!(
            "{} is not owned by SYSTEM or Administrators, so someone else may have put it there: delete it and pair your devices again",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn guard(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Creates `dir` and gives it to SYSTEM and administrators alone, ignoring
/// whatever permissions its parent or a previous owner set. Files created in
/// it later (the key, the service's log) inherit that.
#[cfg(windows)]
pub fn lock_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    acl::set(dir, "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
}

#[cfg(windows)]
fn protect(path: &Path) -> Result<(), String> {
    if in_default_dir(path) {
        // Owned by Administrators whatever the default-owner policy says, so `guard` accepts it.
        acl::set(path, "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)")
    } else {
        // SYSTEM, administrators and the file's owner (whoever chose --data-dir).
        acl::set(path, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)")
    }
}

#[cfg(not(windows))]
fn protect(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("protect {}: {e}", path.display()))
}

#[cfg(windows)]
mod acl {
    //! File permissions through the Win32 security API.
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, WinBuiltinAdministratorsSid,
        WinLocalSystemSid,
    };
    use windows::core::{BOOL, PCWSTR};

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// Applies the owner (if any) and protected DACL of an SDDL string to `path`.
    pub fn set(path: &Path, sddl: &str) -> Result<(), String> {
        let fail = |what: &str| format!("set permissions on {}: {what}", path.display());
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None) }
            .map_err(|e| fail(&e.to_string()))?;
        let result = (|| {
            let (mut present, mut defaulted) = (BOOL::default(), BOOL::default());
            let mut dacl: *mut ACL = std::ptr::null_mut();
            unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) }.map_err(|e| fail(&e.to_string()))?;
            let mut owner = PSID::default();
            unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) }.map_err(|e| fail(&e.to_string()))?;
            let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
            if !owner.is_invalid() {
                info |= OWNER_SECURITY_INFORMATION;
            }
            let name = wide(path);
            let owner = (!owner.is_invalid()).then_some(owner);
            let err = unsafe { SetNamedSecurityInfoW(PCWSTR(name.as_ptr()), SE_FILE_OBJECT, info, owner, None, Some(dacl), None) };
            if err == ERROR_SUCCESS { Ok(()) } else { Err(fail(&windows::core::Error::from(err.to_hresult()).to_string())) }
        })();
        unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        result
    }

    /// Whether `path` is owned by SYSTEM or the Administrators group.
    pub fn owned_by_admins(path: &Path) -> Result<bool, String> {
        let name = wide(path);
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let err = unsafe {
            GetNamedSecurityInfoW(PCWSTR(name.as_ptr()), SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION, Some(&mut owner), None, None, None, &mut sd)
        };
        if err != ERROR_SUCCESS {
            return Err(format!("read owner of {}: {}", path.display(), windows::core::Error::from(err.to_hresult())));
        }
        let trusted = unsafe { IsWellKnownSid(owner, WinLocalSystemSid).as_bool() || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool() };
        unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        Ok(trusted)
    }
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
