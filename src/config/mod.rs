//! Persistent settings + shortcut bindings.
//!
//! Stored as TOML in the OS config dir:
//! - Windows: `%APPDATA%\HushKey\config\config.toml`
//! - macOS:   `~/Library/Application Support/HushKey/config.toml`
//! - Linux:   `$XDG_CONFIG_HOME/hushkey/config.toml`
//!
//! Writes are atomic (temp file + rename) so a crash never leaves a half-written file.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub launch_on_startup: bool,
    pub start_minimized: bool,
    /// Show the small on-screen confirmation when a shortcut fires.
    pub show_toast: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { theme: Theme::Dark, launch_on_startup: false, start_minimized: false, show_toast: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MuteAllBinding {
    /// Toggles mute-all on/off. Human-readable accelerator, e.g. `"Ctrl+Alt+M"`.
    pub hotkey: Option<String>,
    /// Optional dedicated "restore everything" shortcut.
    pub restore_hotkey: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppBinding {
    /// Stable app key (see `audio::AppKey`), e.g. `"spotify.exe"`.
    pub app: String,
    /// Last known friendly name, so bindings for apps that aren't running still read nicely.
    #[serde(default)]
    pub display_name: String,
    pub hotkey: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    pub mute_all: MuteAllBinding,
    #[serde(rename = "bindings")]
    pub bindings: Vec<AppBinding>,
}

impl Config {
    pub fn binding_for(&self, app: &str) -> Option<&AppBinding> {
        self.bindings.iter().find(|b| b.app == app)
    }

    /// Set (or clear with `None`) the shortcut for an app.
    pub fn set_binding(&mut self, app: &str, display_name: &str, hotkey: Option<String>) {
        self.bindings.retain(|b| b.app != app);
        if let Some(hotkey) = hotkey {
            self.bindings.push(AppBinding { app: app.to_owned(), display_name: display_name.to_owned(), hotkey });
            self.bindings.sort_by(|a, b| a.app.cmp(&b.app));
        }
    }
}

pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Store at the platform default location.
    pub fn default_location() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("", "", "HushKey")?;
        Some(Self::new(dirs.config_dir().join("config.toml")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the config. A missing file yields defaults; a corrupt file is backed up
    /// to `config.toml.bak` and defaults are used, so the app still starts.
    pub fn load(&self) -> Config {
        match fs::read_to_string(&self.path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::error!("config at {} is invalid ({e}); backing up and using defaults", self.path.display());
                    let _ = fs::copy(&self.path, self.path.with_extension("toml.bak"));
                    Config::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                log::error!("cannot read config {}: {e}", self.path.display());
                Config::default()
            }
        }
    }

    pub fn save(&self, cfg: &Config) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(cfg).map_err(io::Error::other)?;
        let tmp = self.path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> ConfigStore {
        let dir = std::env::temp_dir().join(format!("hushkey-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ConfigStore::new(dir.join("config.toml"))
    }

    #[test]
    fn missing_file_gives_defaults() {
        let store = temp_store("missing");
        assert_eq!(store.load(), Config::default());
    }

    #[test]
    fn roundtrip() {
        let store = temp_store("roundtrip");
        let mut cfg = Config::default();
        cfg.settings.theme = Theme::Light;
        cfg.mute_all.hotkey = Some("Ctrl+Alt+M".into());
        cfg.set_binding("spotify.exe", "Spotify", Some("Ctrl+Alt+S".into()));
        store.save(&cfg).unwrap();
        assert_eq!(store.load(), cfg);
    }

    #[test]
    fn corrupt_file_falls_back_and_backs_up() {
        let store = temp_store("corrupt");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "this is = = not toml").unwrap();
        assert_eq!(store.load(), Config::default());
        assert!(store.path().with_extension("toml.bak").exists());
    }

    #[test]
    fn set_binding_replaces_and_clears() {
        let mut cfg = Config::default();
        cfg.set_binding("a.exe", "A", Some("Ctrl+1".into()));
        cfg.set_binding("a.exe", "A", Some("Ctrl+2".into()));
        assert_eq!(cfg.bindings.len(), 1);
        assert_eq!(cfg.binding_for("a.exe").unwrap().hotkey, "Ctrl+2");
        cfg.set_binding("a.exe", "A", None);
        assert!(cfg.bindings.is_empty());
    }

    #[test]
    fn partial_file_uses_field_defaults() {
        let cfg: Config = toml::from_str("[settings]\ntheme = \"light\"\n").unwrap();
        assert_eq!(cfg.settings.theme, Theme::Light);
        assert!(cfg.settings.show_toast);
    }
}
