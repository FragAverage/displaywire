//! The user's stream settings, kept in `~/Library/Application Support/Streamy/settings`
//! (one `key=value` per line). Most go to the server; volume and mute are local.

use proto::stream::Settings;

use crate::dialog::state_file;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub bitrate_mbps: u32,
    /// 0 for no limit.
    pub fps_cap: u16,
    pub fec_percent: u8,
    /// 0..=100.
    pub volume: u8,
    pub muted: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        let s = Settings::default();
        Prefs { bitrate_mbps: s.bitrate_kbps / 1000, fps_cap: s.fps_cap, fec_percent: s.fec_percent, volume: 100, muted: false }
    }
}

impl Prefs {
    /// What the server is asked to apply.
    pub fn for_server(&self) -> Settings {
        Settings { bitrate_kbps: self.bitrate_mbps * 1000, fps_cap: self.fps_cap, fec_percent: self.fec_percent }.clamped()
    }

    /// Linear gain for the audio output.
    pub fn gain(&self) -> f32 {
        if self.muted { 0.0 } else { (self.volume.min(100) as f32 / 100.0).powi(2) }
    }

    pub fn parse(text: &str) -> Prefs {
        let mut p = Prefs::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "bitrate_mbps" => p.bitrate_mbps = v.parse().unwrap_or(p.bitrate_mbps),
                "fps_cap" => p.fps_cap = v.parse().unwrap_or(p.fps_cap),
                "fec_percent" => p.fec_percent = v.parse().unwrap_or(p.fec_percent),
                "volume" => p.volume = v.parse().unwrap_or(p.volume),
                "muted" => p.muted = v == "true",
                _ => {}
            }
        }
        p.clamped()
    }

    /// The same settings with every value in a range the server accepts, so the
    /// sheet never shows nonsense.
    pub fn clamped(mut self) -> Prefs {
        let s = self.for_server();
        self.bitrate_mbps = s.bitrate_kbps / 1000;
        self.fec_percent = s.fec_percent;
        self.fps_cap = s.fps_cap;
        self.volume = self.volume.min(100);
        self
    }

    pub fn load() -> Prefs {
        state_file("settings").and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Prefs::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(path) = state_file("settings") {
            let _ = path.parent().map(std::fs::create_dir_all);
            let text = format!(
                "bitrate_mbps={}\nfps_cap={}\nfec_percent={}\nvolume={}\nmuted={}\n",
                self.bitrate_mbps, self.fps_cap, self.fec_percent, self.volume, self.muted
            );
            let _ = std::fs::write(path, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_server_defaults() {
        let p = Prefs::default();
        assert_eq!(p.for_server(), Settings::default());
        assert_eq!(Prefs::parse(""), p);
    }

    #[test]
    fn parses_and_clamps() {
        let p = Prefs::parse("bitrate_mbps=80\nfps_cap=120\nfec_percent=20\nvolume=40\nmuted=true\n");
        assert_eq!((p.bitrate_mbps, p.fps_cap, p.fec_percent, p.volume, p.muted), (80, 120, 20, 40, true));
        let wild = Prefs::parse("bitrate_mbps=99999\nfec_percent=200\nvolume=900\njunk\nfps_cap=notanumber");
        assert_eq!((wild.bitrate_mbps, wild.fec_percent, wild.volume, wild.fps_cap), (500, 50, 100, 0));
    }

    #[test]
    fn gain_follows_volume_and_mute() {
        let mut p = Prefs { volume: 100, ..Prefs::default() };
        assert_eq!(p.gain(), 1.0);
        p.volume = 50;
        assert!((p.gain() - 0.25).abs() < 1e-6);
        p.muted = true;
        assert_eq!(p.gain(), 0.0);
    }
}
