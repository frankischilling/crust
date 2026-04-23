//! Sound event configuration shared between storage, runtime, and UI.
//!
//! Mirrors Chatterino's per-event sound notifications:
//!
//! - mention / whisper / subscribe / raid / custom highlight each have a
//!   user-selected file path and a per-event volume (0.0 - 1.0),
//! - the [`SoundController`](../../crust-ui/src/sound.rs) consumes these
//!   settings to decide *what* to play; the actual playback uses `rodio`,
//! - streamer mode integration (mute while broadcasting) is layered on top
//!   by the controller when `streamer_suppress_sounds` is active.
//!
//! The data types live in `crust-core` so both `crust-storage` (which
//! persists them) and `crust-ui` (which consumes them) can share a single
//! definition without a circular dependency.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Discrete sound-event category. `as_key()` returns a stable kebab-ish
/// identifier that is used both as the `[sounds]` TOML map key and as the
/// wire-format key in [`crate::AppCommand::SetSoundSettings`] /
/// [`crate::AppEvent::SoundSettingsUpdated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundEvent {
    /// Chat message that @-mentions the local user.
    Mention,
    /// Incoming Twitch whisper (or self-echo) in a private-message thread.
    Whisper,
    /// USERNOTICE `sub` / `resub` / `subgift` event.
    Subscribe,
    /// USERNOTICE `raid` event.
    Raid,
    /// Highlight rule fired (rule with `has_sound = true`, no custom URL).
    /// Per-rule overrides take precedence via
    /// [`crate::highlight::HighlightRule::sound_url`].
    CustomHighlight,
}

impl SoundEvent {
    /// Stable identifier used as the settings key (`"mention"`, `"whisper"`, ...).
    pub const fn as_key(self) -> &'static str {
        match self {
            SoundEvent::Mention => "mention",
            SoundEvent::Whisper => "whisper",
            SoundEvent::Subscribe => "subscribe",
            SoundEvent::Raid => "raid",
            SoundEvent::CustomHighlight => "custom_highlight",
        }
    }

    /// Human-facing label shown in the settings editor.
    pub const fn display_name(self) -> &'static str {
        match self {
            SoundEvent::Mention => "Mention",
            SoundEvent::Whisper => "Whisper",
            SoundEvent::Subscribe => "Subscribe / Resub",
            SoundEvent::Raid => "Raid",
            SoundEvent::CustomHighlight => "Custom highlight",
        }
    }

    /// All events in settings-page presentation order.
    pub const fn all() -> &'static [SoundEvent] {
        &[
            SoundEvent::Mention,
            SoundEvent::Whisper,
            SoundEvent::Subscribe,
            SoundEvent::Raid,
            SoundEvent::CustomHighlight,
        ]
    }

    /// Parse back from the stable key produced by [`Self::as_key`].
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "mention" => Some(SoundEvent::Mention),
            "whisper" => Some(SoundEvent::Whisper),
            "subscribe" | "sub" => Some(SoundEvent::Subscribe),
            "raid" => Some(SoundEvent::Raid),
            "custom_highlight" | "highlight" => Some(SoundEvent::CustomHighlight),
            _ => None,
        }
    }
}

/// Per-event sound configuration. Absent / empty `path` means "use the
/// built-in default ping"; the controller still honours `enabled` and
/// `volume`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoundEventSetting {
    /// When false the event is silent, even if `path` is valid.
    #[serde(default = "bool_true")]
    pub enabled: bool,
    /// Absolute or relative file path to a WAV / OGG / MP3 / FLAC file.
    /// Empty string = built-in default ping.
    #[serde(default)]
    pub path: String,
    /// Linear playback volume in the range `[0.0, 1.0]`. Anything outside
    /// is clamped on load by [`SoundSettings::normalised`].
    #[serde(default = "default_volume")]
    pub volume: f32,
}

fn bool_true() -> bool {
    true
}

fn default_volume() -> f32 {
    0.7
}

impl Default for SoundEventSetting {
    fn default() -> Self {
        Self {
            enabled: true,
            path: String::new(),
            volume: default_volume(),
        }
    }
}

impl SoundEventSetting {
    /// Volume clamped into `[0.0, 1.0]` and coerced out of NaN.
    pub fn clamped_volume(&self) -> f32 {
        if !self.volume.is_finite() {
            default_volume()
        } else {
            self.volume.clamp(0.0, 1.0)
        }
    }

    /// Whether a custom file path is configured. Empty or whitespace-only
    /// paths fall back to the built-in default ping.
    pub fn has_custom_path(&self) -> bool {
        !self.path.trim().is_empty()
    }
}

/// Container for the full set of sound event settings.
///
/// Stored on disk under `[sounds.events]` as a map keyed by the stable
/// event key ([`SoundEvent::as_key`]). Missing entries fall back to
/// [`SoundEventSetting::default`] so upgrading the app never silently wipes
/// a user's tweaks, and adding a new event in a future version doesn't
/// require a migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SoundSettings {
    /// Per-event configuration, keyed by [`SoundEvent::as_key`].
    pub events: BTreeMap<String, SoundEventSetting>,
}

impl SoundSettings {
    /// Build a fresh instance with every known [`SoundEvent`] populated
    /// using [`SoundEventSetting::default`].
    pub fn with_defaults() -> Self {
        let mut events = BTreeMap::new();
        for ev in SoundEvent::all() {
            events.insert(ev.as_key().to_owned(), SoundEventSetting::default());
        }
        Self { events }
    }

    /// Lookup a single event's configuration. Returns the default setting
    /// (enabled, no custom path, 70% volume) when the key is missing -
    /// this keeps new event types functional the moment they land in a
    /// release even when users have an older `settings.toml`.
    pub fn get(&self, event: SoundEvent) -> SoundEventSetting {
        self.events
            .get(event.as_key())
            .cloned()
            .unwrap_or_default()
    }

    /// Replace a single event's configuration.
    pub fn set(&mut self, event: SoundEvent, setting: SoundEventSetting) {
        self.events.insert(event.as_key().to_owned(), setting);
    }

    /// Return a sanitised copy: keys that don't match a known event are
    /// dropped, volumes are clamped, and missing events are back-filled.
    pub fn normalised(&self) -> Self {
        let mut events = BTreeMap::new();
        for ev in SoundEvent::all() {
            let mut setting = self.get(*ev);
            setting.volume = setting.clamped_volume();
            setting.path = setting.path.trim().to_owned();
            events.insert(ev.as_key().to_owned(), setting);
        }
        Self { events }
    }

    /// Flatten into a sorted list of `(key, setting)` pairs suitable for
    /// transporting across command / event channels.
    pub fn to_pairs(&self) -> Vec<(String, SoundEventSetting)> {
        SoundEvent::all()
            .iter()
            .map(|ev| (ev.as_key().to_owned(), self.get(*ev)))
            .collect()
    }

    /// Rebuild from a list of `(key, setting)` pairs (the inverse of
    /// [`Self::to_pairs`]). Unknown keys are silently dropped.
    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (String, SoundEventSetting)>,
    {
        let mut settings = Self::with_defaults();
        for (key, setting) in pairs {
            if SoundEvent::from_key(&key).is_some() {
                settings.events.insert(key, setting);
            }
        }
        settings.normalised()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_events_have_stable_keys_and_roundtrip() {
        for ev in SoundEvent::all() {
            let key = ev.as_key();
            assert_eq!(SoundEvent::from_key(key), Some(*ev));
        }
    }

    #[test]
    fn defaults_include_every_event_type() {
        let settings = SoundSettings::with_defaults();
        for ev in SoundEvent::all() {
            assert!(settings.events.contains_key(ev.as_key()));
        }
        let pairs = settings.to_pairs();
        assert_eq!(pairs.len(), SoundEvent::all().len());
    }

    #[test]
    fn normalised_clamps_volume_and_trims_path() {
        let mut settings = SoundSettings::with_defaults();
        settings.set(
            SoundEvent::Mention,
            SoundEventSetting {
                enabled: true,
                path: "  /tmp/a.wav  ".to_owned(),
                volume: 5.5,
            },
        );
        settings.set(
            SoundEvent::Whisper,
            SoundEventSetting {
                enabled: true,
                path: String::new(),
                volume: f32::NAN,
            },
        );

        let norm = settings.normalised();
        let m = norm.get(SoundEvent::Mention);
        assert_eq!(m.path, "/tmp/a.wav");
        assert_eq!(m.volume, 1.0);

        let w = norm.get(SoundEvent::Whisper);
        assert!(w.path.is_empty());
        assert_eq!(w.volume, default_volume());
    }

    #[test]
    fn from_pairs_roundtrips_known_keys_and_drops_unknown() {
        let pairs = vec![
            (
                "mention".to_owned(),
                SoundEventSetting {
                    enabled: false,
                    path: "mention.wav".to_owned(),
                    volume: 0.42,
                },
            ),
            (
                "totally-made-up".to_owned(),
                SoundEventSetting::default(),
            ),
        ];
        let settings = SoundSettings::from_pairs(pairs);
        let m = settings.get(SoundEvent::Mention);
        assert!(!m.enabled);
        assert_eq!(m.path, "mention.wav");
        assert!((m.volume - 0.42).abs() < f32::EPSILON);
        // Unknown keys stripped; all known events still present with defaults.
        assert!(!settings.events.contains_key("totally-made-up"));
        assert!(settings.events.contains_key("raid"));
    }

    #[test]
    fn missing_event_falls_back_to_default_setting() {
        let settings = SoundSettings {
            events: BTreeMap::new(),
        };
        let m = settings.get(SoundEvent::Raid);
        assert!(m.enabled);
        assert!(m.path.is_empty());
        assert!((m.volume - default_volume()).abs() < f32::EPSILON);
    }
}
