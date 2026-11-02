//! Updating from GitHub releases. Each release carries `latest.txt`, a small manifest naming the version and the
//! SHA-256 of each download, and `latest.txt.sig`, an Ed25519 signature of it made in CI with a key that is not in
//! the repository. The apps hold the public half. A download is only used if its hash is the one in a manifest
//! that verifies, so publishing a release is not enough to run code on a PC: the signing key is needed too.
//! Downloads go through the system `curl` (present on macOS and Windows 10+), which keeps a TLS stack out of the apps.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use ring::digest::{Context, SHA256};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

pub const REPO: &str = "FragAverage/displaywire";
/// The public half of the release signing key, hex.
pub const PUBLIC_KEY: &str = "52751f62de12937bb84c9587e34a60a1e94316aaa35a6cf73c1d6a2e34be206f";

const MANIFEST: &str = "latest.txt";
const MAGIC: &str = "displaywire-update 1";
const MAX_MANIFEST: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Windows,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(windows) { Platform::Windows } else { Platform::Macos }
    }

    fn key(self) -> &'static str {
        match self {
            Platform::Macos => "macos",
            Platform::Windows => "windows",
        }
    }

    /// The file name of this platform's download in a release.
    pub fn asset(self) -> &'static str {
        match self {
            Platform::Macos => "DisplayWire-macos.zip",
            Platform::Windows => "displaywire-server-windows.zip",
        }
    }
}

/// A newer release for this platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Available {
    pub version: String,
    pub url: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub files: Vec<(String, [u8; 32], u64)>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// `1.2.3` as numbers. Anything else (a suffix, extra parts, signs) is refused, so a version can't smuggle
/// characters into the download URL.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let mut next = || {
        let p = it.next()?;
        (!p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok()).flatten()
    };
    let r = (next()?, next()?, next()?);
    it.next().is_none().then_some(r)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Option<Manifest> {
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let version = lines.next()?.strip_prefix("version ")?.to_string();
        parse_version(&version)?;
        let mut files = Vec::new();
        for line in lines {
            let mut p = line.split(' ');
            let (name, sum, size) = (p.next()?, p.next()?, p.next()?);
            if p.next().is_some() {
                return None;
            }
            let sum: [u8; 32] = unhex(sum)?.try_into().ok()?;
            files.push((name.to_string(), sum, size.parse().ok()?));
        }
        Some(Manifest { version, files })
    }

    /// The download for `platform`, if this manifest is newer than `current`.
    pub fn available(&self, platform: Platform, current: &str) -> Option<Available> {
        if !is_newer(&self.version, current) {
            return None;
        }
        let &(_, sha256, size) = self.files.iter().find(|(n, _, _)| n == platform.key())?;
        let url = match test_base() {
            Some(base) => format!("{base}/{}", platform.asset()),
            None => format!("https://github.com/{REPO}/releases/download/v{}/{}", self.version, platform.asset()),
        };
        Some(Available { version: self.version.clone(), url, sha256, size })
    }
}

/// Checks `sig` (hex) over `text` and parses it.
pub fn verify_manifest(text: &[u8], sig: &str, public_key: &str) -> Result<Manifest, String> {
    let key = unhex(public_key).filter(|k| k.len() == 32).ok_or("the update key is not set")?;
    let sig = unhex(sig).ok_or("the signature is not valid hex")?;
    UnparsedPublicKey::new(&ED25519, &key).verify(text, &sig).map_err(|_| "the update's signature does not match")?;
    let text = std::str::from_utf8(text).map_err(|_| "the manifest is not text")?;
    Manifest::parse(text).ok_or_else(|| "the manifest is malformed".to_string())
}

fn curl() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join(r"System32\curl.exe")
    } else {
        PathBuf::from("/usr/bin/curl")
    }
}

/// Debug builds only: serve the release files from somewhere else (a local folder served over http) to test updating.
#[cfg(debug_assertions)]
fn test_base() -> Option<String> {
    std::env::var("DISPLAYWIRE_UPDATE_BASE").ok()
}
#[cfg(not(debug_assertions))]
fn test_base() -> Option<String> {
    None
}

fn run_curl(url: &str, max_bytes: u64, out: Option<&Path>, seconds: u32) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(curl());
    let (proto, redir) = if test_base().is_some() { ("=http,https", "=http,https") } else { ("=https", "=https") };
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--proto", proto, "--proto-redir", redir, "--tlsv1.2"])
        .args(["--max-time", &seconds.to_string(), "--max-filesize", &max_bytes.to_string()]);
    match out {
        Some(p) => cmd.arg("-o").arg(p),
        None => cmd.args(["-o", "-"]),
    };
    cmd.arg(url);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let o = cmd.output().map_err(|e| format!("can't run curl: {e}"))?;
    if !o.status.success() {
        let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
        return Err(if why.is_empty() { "the download failed".into() } else { why });
    }
    Ok(o.stdout)
}

/// Looks for a release newer than `current`. `Ok(None)` if this is the latest.
pub fn check(current: &str, platform: Platform) -> Result<Option<Available>, String> {
    let base = test_base().unwrap_or_else(|| format!("https://github.com/{REPO}/releases/latest/download"));
    let text = run_curl(&format!("{base}/{MANIFEST}"), MAX_MANIFEST as u64, None, 20)?;
    let sig = run_curl(&format!("{base}/{MANIFEST}.sig"), 512, None, 20)?;
    let manifest = verify_manifest(&text, &String::from_utf8_lossy(&sig), PUBLIC_KEY)?;
    Ok(manifest.available(platform, current))
}

pub fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(path)?;
    let mut ctx = Context::new(&SHA256);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx.finish().as_ref().try_into().expect("sha256 is 32 bytes"))
}

/// Downloads to `dest` and checks it is exactly what the signed manifest named. `dest` is removed if not.
pub fn download(a: &Available, dest: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(dest);
    let result = run_curl(&a.url, a.size, Some(dest), 600).and_then(|_| {
        let meta = std::fs::metadata(dest).map_err(|e| e.to_string())?;
        let sum = sha256_file(dest).map_err(|e| e.to_string())?;
        if meta.len() != a.size || sum != a.sha256 {
            return Err("the download does not match the signed update".to_string());
        }
        Ok(())
    });
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Publishing side (the `dwsign` tool run by CI).
pub mod sign {
    use super::*;

    /// A new key: (seed, public key), hex.
    pub fn generate() -> Result<(String, String), String> {
        let mut seed = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed).map_err(|_| "no randomness")?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| "bad seed")?;
        Ok((hex(&seed), hex(pair.public_key().as_ref())))
    }

    pub fn manifest(version: &str, macos: &Path, windows: &Path) -> Result<String, String> {
        parse_version(version).ok_or("the version must look like 1.2.3")?;
        let mut out = format!("{MAGIC}\nversion {version}\n");
        for (key, path) in [("macos", macos), ("windows", windows)] {
            let sum = sha256_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
            out += &format!("{key} {} {size}\n", hex(&sum));
        }
        Ok(out)
    }

    pub fn signature(seed_hex: &str, text: &[u8]) -> Result<String, String> {
        let seed = unhex(seed_hex).ok_or("the signing key is not hex")?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| "the signing key is not valid")?;
        Ok(hex(pair.sign(text).as_ref()))
    }

    pub fn public_key(seed_hex: &str) -> Result<String, String> {
        let seed = unhex(seed_hex).ok_or("the signing key is not hex")?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| "the signing key is not valid")?;
        Ok(hex(pair.public_key().as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(version: &str) -> String {
        format!("{MAGIC}\nversion {version}\nmacos {} 10\nwindows {} 20\n", hex(&[1; 32]), hex(&[2; 32]))
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.6.0"), Some((0, 6, 0)));
        for bad in ["", "1", "1.2", "1.2.3.4", "1.2.x", "v1.2.3", "1.2.3-rc1", "1..3", "-1.2.3", "1.2.3/../x", "123456.0.0"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.6.0", "0.6.0"));
        assert!(!is_newer("0.5.9", "0.6.0"));
        assert!(!is_newer("junk", "0.6.0"));
    }

    #[test]
    fn signed_manifest_is_accepted_and_tampering_is_not() {
        let (seed, public) = sign::generate().unwrap();
        let text = sample("0.7.0");
        let sig = sign::signature(&seed, text.as_bytes()).unwrap();
        let m = verify_manifest(text.as_bytes(), &sig, &public).unwrap();
        let a = m.available(Platform::Windows, "0.6.0").unwrap();
        assert_eq!(a.size, 20);
        assert_eq!(a.url, "https://github.com/FragAverage/displaywire/releases/download/v0.7.0/displaywire-server-windows.zip");
        assert!(m.available(Platform::Macos, "0.7.0").is_none());

        assert!(verify_manifest(sample("0.9.0").as_bytes(), &sig, &public).is_err());
        let (other_seed, _) = sign::generate().unwrap();
        let forged = sign::signature(&other_seed, text.as_bytes()).unwrap();
        assert!(verify_manifest(text.as_bytes(), &forged, &public).is_err());
        assert!(verify_manifest(text.as_bytes(), "zz", &public).is_err());
        assert!(verify_manifest(text.as_bytes(), &sig, "00").is_err());
    }

    #[test]
    fn malformed_manifests() {
        assert!(Manifest::parse("hello").is_none());
        assert!(Manifest::parse(&sample("1.2.x")).is_none());
        assert!(Manifest::parse(&format!("{MAGIC}\nversion 1.0.0\nmacos abc 1\n")).is_none());
        assert!(Manifest::parse(&format!("{MAGIC}\nversion 1.0.0\nmacos {} 1 extra\n", hex(&[0; 32]))).is_none());
    }

    #[test]
    fn manifest_covers_both_files() {
        let dir = std::env::temp_dir().join(format!("dw-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.zip"), dir.join("b.zip"));
        std::fs::write(&a, b"mac").unwrap();
        std::fs::write(&b, b"windows!").unwrap();
        let text = sign::manifest("0.7.0", &a, &b).unwrap();
        let m = Manifest::parse(&text).unwrap();
        assert_eq!(m.files[0].2, 3);
        assert_eq!(m.files[1].2, 8);
        assert_eq!(m.files[0].1, sha256_file(&a).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
