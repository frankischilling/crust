use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::StorageError;

// ─── AppSettings ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default)]
    pub username: String,
    /// Channels to auto-join on connect.
    #[serde(default)]
    pub auto_join: Vec<String>,
    /// Highlight keywords.
    #[serde(default)]
    pub highlights: Vec<String>,
    /// Ignored usernames (lowercase).
    #[serde(default)]
    pub ignores: Vec<String>,
    /// Message timestamps on/off.
    #[serde(default = "bool_true")]
    pub show_timestamps: bool,
}

fn default_theme() -> String { "dark".to_owned() }
fn default_font_size() -> f32 { 13.0 }
fn bool_true() -> bool { true }

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            font_size: default_font_size(),
            username: String::new(),
            auto_join: Vec::new(),
            highlights: Vec::new(),
            ignores: Vec::new(),
            show_timestamps: true,
        }
    }
}

// ─── SettingsStore ───────────────────────────────────────────────────────────

const KEYRING_SERVICE: &str = "crust-twitch-client";
const KEYRING_ENTRY: &str = "oauth-token";

pub struct SettingsStore {
    config_path: PathBuf,
}

impl SettingsStore {
    /// Construct and ensure config directory exists.
    pub fn new() -> Result<Self, StorageError> {
        let dirs = ProjectDirs::from("dev", "crust", "crust")
            .ok_or_else(|| StorageError::Io(std::io::Error::other("cannot find config dir")))?;
        let config_dir = dirs.config_dir().to_path_buf();
        std::fs::create_dir_all(&config_dir)?;
        Ok(Self {
            config_path: config_dir.join("settings.toml"),
        })
    }

    pub fn load(&self) -> AppSettings {
        match std::fs::read_to_string(&self.config_path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                error!("Failed to parse settings ({e}), using defaults");
                AppSettings::default()
            }),
            Err(_) => AppSettings::default(),
        }
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), StorageError> {
        let s = toml::to_string_pretty(settings)
            .map_err(|e| StorageError::Serde(e.to_string()))?;
        std::fs::write(&self.config_path, s)?;
        info!("Settings saved to {:?}", self.config_path);
        Ok(())
    }

    // ─── Token / keyring ─────────────────────────────────────────────────

    pub fn save_token(&self, token: &str) -> Result<(), StorageError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
            .map_err(|e| StorageError::Keyring(e.to_string()))?;
        entry
            .set_password(token)
            .map_err(|e| StorageError::Keyring(e.to_string()))
    }

    pub fn load_token(&self) -> Option<String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY).ok()?;
        entry.get_password().ok()
    }

    pub fn delete_token(&self) -> Result<(), StorageError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
            .map_err(|e| StorageError::Keyring(e.to_string()))?;
        entry
            .delete_credential()
            .map_err(|e| StorageError::Keyring(e.to_string()))
    }
}
