use std::collections::BTreeMap;
use std::path::PathBuf;

use crust_core::highlight::HighlightRule;
use crust_core::model::ModActionPreset;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::StorageError;

// AccountEntry: one saved Twitch account

/// A single saved Twitch account (username + optional fallback token).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountEntry {
    pub username: String,
    /// OAuth token stored as fallback when the OS keyring is unavailable.
    #[serde(default)]
    pub oauth_token: String,
}

// AppSettings: user configuration structure

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Chat body font size in points (applied globally; small/heading/tiny derive as offsets).
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// UI scale ratio fed to egui `pixels_per_point` (1.0 = host DPI default).
    #[serde(default = "default_ui_font_size")]
    pub ui_font_size: f32,
    /// Top chrome toolbar label size (pt).
    #[serde(default = "default_topbar_font_size")]
    pub topbar_font_size: f32,
    /// Channel tab chip label size (pt).
    #[serde(default = "default_tabs_font_size")]
    pub tabs_font_size: f32,
    /// Message timestamp size (pt).
    #[serde(default = "default_timestamps_font_size")]
    pub timestamps_font_size: f32,
    /// Room-state / viewer-count pill label size (pt).
    #[serde(default = "default_pills_font_size")]
    pub pills_font_size: f32,
    /// Last focused channel (restored on next launch). Stored as the
    /// serialized ChannelId debug/serde string.
    #[serde(default)]
    pub last_active_channel: String,
    /// Username of the currently active account (mirrors `accounts[n].username`).
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
    /// Favorited emote URLs in the emote picker.
    #[serde(default)]
    pub emote_picker_favorites: Vec<String>,
    /// Recently used emote URLs in the emote picker (most-recent first).
    #[serde(default)]
    pub emote_picker_recent: Vec<String>,
    /// Optional preferred provider boost for emote picker ranking.
    #[serde(default)]
    pub emote_picker_provider_boost: Option<String>,
    /// Message timestamps on/off.
    #[serde(default = "bool_true")]
    pub show_timestamps: bool,
    /// Include seconds in rendered chat timestamps.
    #[serde(default)]
    pub show_timestamp_seconds: bool,
    /// Use 24-hour clock formatting for rendered chat timestamps.
    #[serde(default = "bool_true")]
    pub use_24h_timestamps: bool,
    /// Persist chat messages into the local SQLite log index.
    #[serde(default = "bool_true")]
    pub local_log_indexing_enabled: bool,
    /// Slash command usage counts for autocomplete ranking.
    #[serde(default)]
    pub slash_usage_counts: BTreeMap<String, u32>,
    /// OAuth token for the *active* account (legacy/fallback field kept for
    /// backward compatibility with configs that pre-date multi-account support).
    #[serde(default)]
    pub oauth_token: String,
    /// All saved accounts.  The active one is identified by `username`.
    #[serde(default)]
    pub accounts: Vec<AccountEntry>,
    /// Account that auto-logs in on next startup.  Empty string = use last active.
    #[serde(default)]
    pub default_account: String,
    /// Preferred nickname for generic IRC servers.
    #[serde(default)]
    pub irc_nick: String,
    /// Enable Kick support (beta).
    #[serde(default)]
    pub enable_kick_beta: bool,
    /// Enable generic IRC support (beta).
    #[serde(default)]
    pub enable_irc_beta: bool,
    /// NickServ username for automatic identification on IRC servers.
    #[serde(default)]
    pub irc_nickserv_user: String,
    /// NickServ password for automatic identification on IRC servers.
    #[serde(default)]
    pub irc_nickserv_pass: String,
    /// Keep the window always on top of other windows.
    #[serde(default)]
    pub always_on_top: bool,
    /// Overflow handling for Twitch chat input.
    /// `true` = block extra chars, `false` = allow and highlight overflow.
    #[serde(default = "bool_true")]
    pub prevent_overlong_twitch_messages: bool,
    /// Collapse long messages in the chat list with an ellipsis.
    #[serde(default = "bool_true")]
    pub collapse_long_messages: bool,
    /// Maximum visible lines before long-message collapse applies.
    #[serde(default = "default_collapse_long_message_lines")]
    pub collapse_long_message_lines: usize,
    /// If true, animation repainting runs only while the app window is focused.
    #[serde(default = "bool_true")]
    pub animations_when_focused: bool,
    /// Preferred channel chrome layout: `sidebar` or `top_tabs`.
    #[serde(default = "default_channel_layout")]
    pub channel_layout: String,
    /// Whether the sidebar is visible when using sidebar layout.
    #[serde(default = "bool_true")]
    pub sidebar_visible: bool,
    /// Whether the analytics side panel is visible.
    #[serde(default)]
    pub analytics_visible: bool,
    /// Whether the IRC status panel is visible.
    #[serde(default)]
    pub irc_status_visible: bool,
    /// Tab density/style: `compact` or `normal`.
    #[serde(default = "default_tab_style")]
    pub tab_style: String,
    /// Whether tabs show close buttons on hover/selection.
    #[serde(default = "bool_true")]
    pub show_tab_close_buttons: bool,
    /// Whether tabs show live indicators for live Twitch channels.
    #[serde(default = "bool_true")]
    pub show_tab_live_indicators: bool,
    /// Whether split headers show the stream title when available.
    #[serde(default = "bool_true")]
    pub split_header_show_title: bool,
    /// Whether split headers show the current game/category.
    #[serde(default)]
    pub split_header_show_game: bool,
    /// Whether split headers show viewer counts when available.
    #[serde(default = "bool_true")]
    pub split_header_show_viewer_count: bool,
    // -- Highlight rules (chatterino-style per-rule config) --------------
    /// Structured highlight rules (replaces the flat `highlights` string list).
    /// When empty on load and `highlights` is non-empty, a migration populates it.
    #[serde(default)]
    pub highlight_rules: Vec<HighlightRule>,
    /// Structured filter records for hiding messages.
    #[serde(default)]
    pub filter_records: Vec<crust_core::model::filters::FilterRecord>,
    // -- Moderation action presets ----------------------------------------
    /// Saved moderation action presets shown in the user-card Moderation tab.
    /// When empty, the UI falls back to [`ModActionPreset::defaults()`].
    #[serde(default)]
    pub mod_action_presets: Vec<ModActionPreset>,
    /// User login → custom display name aliases.
    #[serde(default)]
    pub nicknames: Vec<crust_core::model::Nickname>,
    /// Structured per-user ignore list (supports regex + case sensitivity).
    #[serde(default)]
    pub ignored_users: Vec<crust_core::ignores::IgnoredUser>,
    /// Text-pattern ignore list with configurable actions (block/replace/highlight/mention).
    #[serde(default)]
    pub ignored_phrases: Vec<crust_core::ignores::IgnoredPhrase>,
    /// Fetch + display pronouns from alejo.io on the user profile popup.
    /// Off by default to respect privacy preferences.
    #[serde(default)]
    pub show_pronouns_in_usercard: bool,
    // -- Desktop notifications --------------------------------------------
    /// Fire an OS desktop notification when a highlight rule with
    /// `show_in_mentions = true` matches an incoming message.
    #[serde(default)]
    pub desktop_notifications_enabled: bool,
    // -- Watched channels for stream status notifications --------------------
    /// Channels being watched for live/offline notifications.
    #[serde(default)]
    pub watched_channels: Vec<crust_core::notifications::WatchedChannel>,
    // -- Updater settings/state (Windows + Debian-based Linux releases) ------
    /// Whether startup/background update checks are enabled.
    #[serde(default = "bool_true")]
    pub update_checks_enabled: bool,
    /// Last successful or attempted update check timestamp in UTC RFC3339.
    #[serde(default)]
    pub updater_last_checked_at: Option<String>,
    /// Semver string that the user skipped (if any).
    #[serde(default)]
    pub updater_skipped_version: String,
    // -- Streamer mode -----------------------------------------------------
    /// Streamer mode setting: `off`, `auto`, or `on`.
    /// `auto` enables only when broadcasting software (OBS / Streamlabs) is detected.
    #[serde(default = "default_streamer_mode")]
    pub streamer_mode: String,
    /// Hide link preview tooltips while streamer mode is active.
    #[serde(default = "bool_true")]
    pub streamer_hide_link_previews: bool,
    /// Hide viewer counts in split headers while streamer mode is active.
    #[serde(default = "bool_true")]
    pub streamer_hide_viewer_counts: bool,
    /// Suppress sound notifications while streamer mode is active.
    #[serde(default = "bool_true")]
    pub streamer_suppress_sounds: bool,
}

fn default_theme() -> String {
    "dark".to_owned()
}
fn default_font_size() -> f32 {
    13.5
}
fn default_ui_font_size() -> f32 {
    1.0
}
fn default_topbar_font_size() -> f32 {
    0.0
}
fn default_tabs_font_size() -> f32 {
    0.0
}
fn default_timestamps_font_size() -> f32 {
    0.0
}
fn default_pills_font_size() -> f32 {
    0.0
}
fn bool_true() -> bool {
    true
}
fn bool_false() -> bool {
    false
}
fn default_collapse_long_message_lines() -> usize {
    8
}
fn default_channel_layout() -> String {
    "sidebar".to_owned()
}
fn default_tab_style() -> String {
    "compact".to_owned()
}
fn default_streamer_mode() -> String {
    "off".to_owned()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            font_size: default_font_size(),
            ui_font_size: default_ui_font_size(),
            topbar_font_size: default_topbar_font_size(),
            tabs_font_size: default_tabs_font_size(),
            timestamps_font_size: default_timestamps_font_size(),
            pills_font_size: default_pills_font_size(),
            last_active_channel: String::new(),
            username: String::new(),
            auto_join: Vec::new(),
            highlights: Vec::new(),
            ignores: Vec::new(),
            emote_picker_favorites: Vec::new(),
            emote_picker_recent: Vec::new(),
            emote_picker_provider_boost: None,
            show_timestamps: true,
            show_timestamp_seconds: bool_false(),
            use_24h_timestamps: true,
            local_log_indexing_enabled: true,
            slash_usage_counts: BTreeMap::new(),
            oauth_token: String::new(),
            accounts: Vec::new(),
            default_account: String::new(),
            irc_nick: String::new(),
            enable_kick_beta: false,
            enable_irc_beta: false,
            irc_nickserv_user: String::new(),
            irc_nickserv_pass: String::new(),
            always_on_top: false,
            prevent_overlong_twitch_messages: true,
            collapse_long_messages: true,
            collapse_long_message_lines: default_collapse_long_message_lines(),
            animations_when_focused: true,
            channel_layout: default_channel_layout(),
            sidebar_visible: true,
            analytics_visible: false,
            irc_status_visible: false,
            tab_style: default_tab_style(),
            show_tab_close_buttons: true,
            show_tab_live_indicators: true,
            split_header_show_title: true,
            split_header_show_game: false,
            split_header_show_viewer_count: true,
            highlight_rules: Vec::new(),
            filter_records: Vec::new(),
            mod_action_presets: Vec::new(),
            nicknames: Vec::new(),
            ignored_users: Vec::new(),
            ignored_phrases: Vec::new(),
            show_pronouns_in_usercard: false,
            desktop_notifications_enabled: false,
            watched_channels: Vec::new(),
            update_checks_enabled: true,
            updater_last_checked_at: None,
            updater_skipped_version: String::new(),
            streamer_mode: default_streamer_mode(),
            streamer_hide_link_previews: true,
            streamer_hide_viewer_counts: true,
            streamer_suppress_sounds: true,
        }
    }
}

// SettingsStore: persistent settings and token management

const KEYRING_SERVICE: &str = "crust-twitch-client";
const KEYRING_ENTRY: &str = "oauth-token";

fn account_keyring_key(username: &str) -> String {
    format!("oauth-token-{}", username.to_lowercase())
}

pub struct SettingsStore {
    config_path: PathBuf,
}

fn remove_account_from_settings(settings: &mut AppSettings, username: &str) {
    settings.accounts.retain(|a| a.username != username);

    if settings.default_account == username {
        settings.default_account.clear();
    }

    if settings.username == username {
        if let Some(next) = settings.accounts.first() {
            settings.username = next.username.clone();
            settings.oauth_token = next.oauth_token.clone();
        } else {
            settings.username.clear();
            settings.oauth_token.clear();
        }
    }
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
            Ok(s) => {
                let mut cfg: AppSettings = toml::from_str(&s).unwrap_or_else(|e| {
                    error!("Failed to parse settings ({e}), using defaults");
                    AppSettings::default()
                });
                // Migration: if `accounts` is empty but the legacy single-account
                // fields are populated, seed the accounts list from them.
                if cfg.accounts.is_empty() && !cfg.username.is_empty() {
                    cfg.accounts.push(AccountEntry {
                        username: cfg.username.clone(),
                        oauth_token: cfg.oauth_token.clone(),
                    });
                }
                if cfg.collapse_long_message_lines == 0 {
                    cfg.collapse_long_message_lines = default_collapse_long_message_lines();
                }
                if !cfg.font_size.is_finite() {
                    cfg.font_size = default_font_size();
                } else {
                    cfg.font_size = cfg.font_size.clamp(8.0, 32.0);
                }
                if !cfg.ui_font_size.is_finite() {
                    cfg.ui_font_size = default_ui_font_size();
                } else {
                    cfg.ui_font_size = cfg.ui_font_size.clamp(0.75, 1.75);
                }
                for slot in [
                    &mut cfg.topbar_font_size,
                    &mut cfg.tabs_font_size,
                    &mut cfg.timestamps_font_size,
                    &mut cfg.pills_font_size,
                ] {
                    // 0.0 = "auto-follow chat font"; otherwise clamp to section range.
                    if !slot.is_finite() || *slot < 0.0 {
                        *slot = 0.0;
                    } else if *slot > 0.0 {
                        *slot = slot.clamp(8.0, 28.0);
                    }
                }
                if !matches!(cfg.channel_layout.as_str(), "sidebar" | "top_tabs") {
                    cfg.channel_layout = default_channel_layout();
                }
                if !matches!(cfg.tab_style.as_str(), "compact" | "normal") {
                    cfg.tab_style = default_tab_style();
                }
                if !matches!(cfg.streamer_mode.as_str(), "off" | "auto" | "on") {
                    cfg.streamer_mode = default_streamer_mode();
                }
                // Migration: convert legacy plain-string highlights to structured rules.
                if cfg.highlight_rules.is_empty() && !cfg.highlights.is_empty() {
                    cfg.highlight_rules = cfg
                        .highlights
                        .iter()
                        .filter(|s| !s.trim().is_empty())
                        .map(|kw| HighlightRule::new(kw.trim()))
                        .collect();
                }
                // Migration: seed `ignored_users` from the legacy flat `ignores`
                // string list on first load of the new structured config.
                if cfg.ignored_users.is_empty() && !cfg.ignores.is_empty() {
                    cfg.ignored_users = cfg
                        .ignores
                        .iter()
                        .filter(|s| !s.trim().is_empty())
                        .map(|login| crust_core::ignores::IgnoredUser::new(login.trim()))
                        .collect();
                }
                cfg
            }
            Err(_) => AppSettings::default(),
        }
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), StorageError> {
        let s = toml::to_string_pretty(settings).map_err(|e| StorageError::Serde(e.to_string()))?;
        std::fs::write(&self.config_path, s)?;
        info!("Settings saved to {:?}", self.config_path);
        Ok(())
    }

    // --- Per-account token management ---

    /// Best-effort: write the token for `username` to the per-account keyring
    /// slot only - does NOT touch the settings file.  Use this after
    /// `save()` has already persisted the full settings struct so we avoid a
    /// second load→modify→save cycle.
    pub fn try_save_account_keyring(&self, username: &str, token: &str) {
        let key = account_keyring_key(username);
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &key) {
            if let Err(e) = entry.set_password(token) {
                tracing::debug!("Keyring write skipped for {username}: {e}");
            }
        }
    }

    /// Save a token for the given account in the OS keyring and settings file.
    pub fn save_account_token(&self, username: &str, token: &str) -> Result<(), StorageError> {
        let key = account_keyring_key(username);
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &key) {
            if let Err(e) = entry.set_password(token) {
                tracing::warn!("Keyring write failed for {username} ({e}), using settings file");
            }
        }
        let mut settings = self.load();
        if let Some(acc) = settings
            .accounts
            .iter_mut()
            .find(|a| a.username == username)
        {
            acc.oauth_token = token.to_owned();
        } else {
            settings.accounts.push(AccountEntry {
                username: username.to_owned(),
                oauth_token: token.to_owned(),
            });
        }
        self.save(&settings)
    }

    /// Load a token for the given account from keyring or settings file.
    pub fn load_account_token(&self, username: &str) -> Option<String> {
        let key = account_keyring_key(username);
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &key) {
            if let Ok(t) = entry.get_password() {
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        let settings = self.load();
        let t = settings
            .accounts
            .iter()
            .find(|a| a.username == username)
            .map(|a| a.oauth_token.clone())
            .unwrap_or_default();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    }

    /// Delete a saved account and its token entirely.
    pub fn delete_account(&self, username: &str) -> Result<(), StorageError> {
        // Remove from keyring
        let key = account_keyring_key(username);
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &key) {
            let _ = entry.delete_credential();
        }
        let mut settings = self.load();
        remove_account_from_settings(&mut settings, username);
        self.save(&settings)
    }

    // --- Legacy single-account token management (kept for backward compatibility) ---

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

    /// Try to save the token to the OS keyring only - does not touch the settings file.
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
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
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

#[cfg(test)]
mod tests {
    use super::remove_account_from_settings;
    use super::AccountEntry;
    use super::AppSettings;

    #[test]
    fn legacy_configs_pick_up_new_appearance_defaults() {
        let cfg: AppSettings = toml::from_str(
            r#"
theme = "dark"
font_size = 13.0
show_timestamps = true
"#,
        )
        .expect("legacy config should parse");

        assert_eq!(cfg.channel_layout, "sidebar");
        assert!(cfg.sidebar_visible);
        assert_eq!(cfg.tab_style, "compact");
        assert!(cfg.show_tab_close_buttons);
        assert!(cfg.show_tab_live_indicators);
        assert!(cfg.split_header_show_title);
        assert!(cfg.split_header_show_viewer_count);
        assert!(!cfg.split_header_show_game);
        assert!(!cfg.analytics_visible);
        assert!(!cfg.irc_status_visible);
    }

    #[test]
    fn explicit_appearance_settings_round_trip_from_toml() {
        let cfg: AppSettings = toml::from_str(
            r#"
channel_layout = "top_tabs"
sidebar_visible = false
analytics_visible = true
irc_status_visible = true
tab_style = "normal"
show_tab_close_buttons = false
show_tab_live_indicators = false
split_header_show_title = false
split_header_show_game = true
split_header_show_viewer_count = false
"#,
        )
        .expect("appearance config should parse");

        assert_eq!(cfg.channel_layout, "top_tabs");
        assert!(!cfg.sidebar_visible);
        assert!(cfg.analytics_visible);
        assert!(cfg.irc_status_visible);
        assert_eq!(cfg.tab_style, "normal");
        assert!(!cfg.show_tab_close_buttons);
        assert!(!cfg.show_tab_live_indicators);
        assert!(!cfg.split_header_show_title);
        assert!(cfg.split_header_show_game);
        assert!(!cfg.split_header_show_viewer_count);
    }

    #[test]
    fn legacy_config_without_highlight_rules_parses() {
        let cfg: AppSettings = toml::from_str(
            r#"
theme = "dark"
font_size = 13.0
"#,
        )
        .expect("legacy config should parse without highlight_rules");

        assert!(cfg.highlight_rules.is_empty());
        assert!(cfg.filter_records.is_empty());
        assert!(cfg.mod_action_presets.is_empty());
    }

    #[test]
    fn removing_default_account_clears_default_field() {
        let mut cfg = AppSettings {
            default_account: "alpha".to_owned(),
            accounts: vec![
                AccountEntry {
                    username: "alpha".to_owned(),
                    oauth_token: "tok-a".to_owned(),
                },
                AccountEntry {
                    username: "beta".to_owned(),
                    oauth_token: "tok-b".to_owned(),
                },
            ],
            ..AppSettings::default()
        };

        remove_account_from_settings(&mut cfg, "alpha");

        assert!(cfg.default_account.is_empty());
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].username, "beta");
    }

    #[test]
    fn removing_active_account_moves_to_first_remaining_and_syncs_token() {
        let mut cfg = AppSettings {
            username: "alpha".to_owned(),
            oauth_token: "tok-a".to_owned(),
            accounts: vec![
                AccountEntry {
                    username: "alpha".to_owned(),
                    oauth_token: "tok-a".to_owned(),
                },
                AccountEntry {
                    username: "beta".to_owned(),
                    oauth_token: "tok-b".to_owned(),
                },
            ],
            ..AppSettings::default()
        };

        remove_account_from_settings(&mut cfg, "alpha");

        assert_eq!(cfg.username, "beta");
        assert_eq!(cfg.oauth_token, "tok-b");
    }

    #[test]
    fn removing_last_active_account_clears_identity_fields() {
        let mut cfg = AppSettings {
            username: "alpha".to_owned(),
            oauth_token: "tok-a".to_owned(),
            default_account: "alpha".to_owned(),
            accounts: vec![AccountEntry {
                username: "alpha".to_owned(),
                oauth_token: "tok-a".to_owned(),
            }],
            ..AppSettings::default()
        };

        remove_account_from_settings(&mut cfg, "alpha");

        assert!(cfg.accounts.is_empty());
        assert!(cfg.username.is_empty());
        assert!(cfg.oauth_token.is_empty());
        assert!(cfg.default_account.is_empty());
    }
}
