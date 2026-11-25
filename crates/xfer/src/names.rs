//! Names for received files: safe on Windows and macOS, and never overwriting one already there.

use std::path::{Path, PathBuf};

/// A file name that can't escape the folder or collide with a Windows device name.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut clean: String = base.chars().filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c)).collect();
    clean = clean.trim_matches([' ', '.']).to_string();
    let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if clean.is_empty() {
        return "file".into();
    }
    if reserved {
        clean.insert(0, '_');
    }
    // Keep the extension when shortening.
    if clean.chars().count() > 150 {
        let ext: String = clean.rsplit_once('.').filter(|(_, e)| e.len() <= 12).map(|(_, e)| format!(".{e}")).unwrap_or_default();
        let keep = 150 - ext.chars().count();
        clean = clean.chars().take(keep).collect::<String>() + &ext;
    }
    clean
}

/// `name`, or `name (1)`, `name (2)`… if that exists already.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let taken = |p: &Path| p.exists() || p.with_file_name(format!("{}.part", p.file_name().unwrap().to_string_lossy())).exists();
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !taken(p))
        .unwrap()
}
