use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::StorageError;

// AppSettings: user configuration structure

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
    /// OAuth token stored as a fallback when the OS keyring is unavailable.
    #[serde(default)]
    pub oauth_token: String,
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
            oauth_token: String::new(),
        }
    }
}

// SettingsStore: persistent settings and token management

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

    // Token / keyring management

    pub fn save_token(&self, token: &str) -> Result<(), StorageError> {
        // Try OS keyring first; fall back silently to settings file.
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY) {
            if let Err(e) = entry.set_password(token) {
                tracing::warn!("Keyring unavailable ({e}), storing token in settings file");
            }
        }
        // Always persist to settings file as a reliable fallback.
        let mut settings = self.load();
        settings.oauth_token = token.to_owned();
        self.save(&settings)
    }

    /// Try to save the token to the OS keyring only — does not touch the settings file.
    pub fn try_save_keyring(&self, token: &str) {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY) {
            if let Err(e) = entry.set_password(token) {
                tracing::debug!("Keyring save skipped: {e}");
            }
        }
    }

    pub fn load_token(&self) -> Option<String> {
        // Try OS keyring first.
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY) {
            if let Ok(token) = entry.get_password() {
                if !token.is_empty() {
                    return Some(token);
                }
            }
        }
        // Fall back to settings file.
        let token = self.load().oauth_token;
        if token.is_empty() { None } else { Some(token) }
    }

    pub fn delete_token(&self) -> Result<(), StorageError> {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY) {
            let _ = entry.delete_credential();
        }
        // Also clear from settings file.
        let mut settings = self.load();
        settings.oauth_token = String::new();
        self.save(&settings)
    }
}
