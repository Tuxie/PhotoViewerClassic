use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Default, Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Config {
    pub geometry: Option<WindowGeometry>,
    pub fullscreen: bool,
}

/// Resolve the config directory using an injected environment-variable getter.
///
/// Priority:
/// 1. `PVC_HOME` (any platform)
/// 2. Windows: `%APPDATA%\PhotoViewerClassic`
/// 3. Non-Windows: `$HOME/.config/pvc`
///
/// Returns `None` if the needed base variable is absent.
pub fn config_dir_from(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(val) = get("PVC_HOME") {
        return Some(PathBuf::from(val));
    }

    #[cfg(windows)]
    {
        get("APPDATA").map(|base| PathBuf::from(base).join("PhotoViewerClassic"))
    }

    #[cfg(not(windows))]
    {
        get("HOME").map(|base| PathBuf::from(base).join(".config/pvc"))
    }
}

/// Resolve the config directory using real environment variables.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(|k| std::env::var(k).ok())
}

/// Load config from `<dir>/config.toml`. Missing file or parse errors return `Config::default()`.
pub fn load_from(dir: &Path) -> Config {
    let path = dir.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Config::default(),
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Load config, resolving the directory via real env vars. Any error returns `Config::default()`.
pub fn load() -> Config {
    match config_dir() {
        Some(dir) => load_from(&dir),
        None => Config::default(),
    }
}

/// Write `cfg` as TOML to `<dir>/config.toml`, creating the directory if needed.
pub fn save_to(dir: &Path, cfg: &Config) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string(cfg)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(dir.join("config.toml"), text)
}

/// Save config, resolving the directory via real env vars.
pub fn save(cfg: &Config) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "config directory could not be determined (PVC_HOME / HOME / APPDATA not set)",
        )
    })?;
    save_to(&dir, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── config_dir_from: PVC_HOME takes precedence on any platform ──────────

    #[test]
    fn config_dir_pvc_home_wins() {
        let dir = config_dir_from(|k| match k {
            "PVC_HOME" => Some("/custom/pvc".into()),
            "APPDATA" => Some("C:\\Users\\user\\AppData\\Roaming".into()),
            "HOME" => Some("/home/user".into()),
            _ => None,
        });
        assert_eq!(dir, Some(PathBuf::from("/custom/pvc")));
    }

    // ── config_dir_from: platform-specific fallback ──────────────────────────

    #[test]
    fn config_dir_fallback_platform() {
        let dir = config_dir_from(|k| match k {
            "PVC_HOME" => None,
            #[cfg(windows)]
            "APPDATA" => Some("C:\\Users\\user\\AppData\\Roaming".into()),
            #[cfg(not(windows))]
            "HOME" => Some("/home/user".into()),
            _ => None,
        });

        #[cfg(windows)]
        assert_eq!(
            dir,
            Some(PathBuf::from(
                "C:\\Users\\user\\AppData\\Roaming\\PhotoViewerClassic"
            ))
        );

        #[cfg(not(windows))]
        assert_eq!(dir, Some(PathBuf::from("/home/user/.config/pvc")));
    }

    // ── config_dir_from: returns None when base var absent ──────────────────

    #[test]
    fn config_dir_none_when_base_absent() {
        let dir = config_dir_from(|_| None);
        assert_eq!(dir, None);
    }

    // ── serde round-trip WITH geometry ───────────────────────────────────────

    #[test]
    fn serde_round_trip_with_geometry() {
        let cfg = Config {
            geometry: Some(WindowGeometry {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            }),
            fullscreen: true,
        };
        let text = toml::to_string(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(cfg, parsed);
    }

    // ── serde round-trip WITHOUT geometry (None) ─────────────────────────────

    #[test]
    fn serde_round_trip_without_geometry() {
        let cfg = Config {
            geometry: None,
            fullscreen: false,
        };
        let text = toml::to_string(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(cfg, parsed);
    }

    // ── corrupt TOML → load_from returns default ─────────────────────────────

    #[test]
    fn corrupt_toml_yields_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, b"this is ][[ not valid toml !!!").expect("write");
        assert_eq!(load_from(tmp.path()), Config::default());
    }

    // ── save_to → load_from round-trip ───────────────────────────────────────

    #[test]
    fn save_load_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Config {
            geometry: Some(WindowGeometry {
                x: -5,
                y: 0,
                w: 1920,
                h: 1080,
            }),
            fullscreen: false,
        };
        save_to(tmp.path(), &cfg).expect("save");
        let loaded = load_from(tmp.path());
        assert_eq!(cfg, loaded);
    }

    // ── missing config.toml → load_from returns default ─────────────────────

    #[test]
    fn missing_file_yields_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(load_from(tmp.path()), Config::default());
    }
}
