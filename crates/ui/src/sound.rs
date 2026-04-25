//! Sound event playback backend.
//!
//! Wraps a lazily-initialised [`rodio::MixerDeviceSink`] handle and exposes
//! a high-level [`SoundController`] that:
//!
//! 1. honours per-event enable / path / volume configuration supplied via
//!    [`SoundController::apply_settings`] from `AppSettings.sounds`,
//! 2. mutes playback while streamer mode is active and the user has opted
//!    into `streamer_suppress_sounds`,
//! 3. falls back to a short synthetic ping tone when no custom file is
//!    configured (or the configured file fails to decode), and
//! 4. caches decoded bytes so repeated plays don't hit disk on every
//!    incoming mention.
//!
//! The audio device opens on first use, not at app startup, so headless
//! environments (test runners, SSH sessions, CI without an audio card) do
//! not pay any cost and never hit a hard panic - failures degrade to a
//! debug-level log line.

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rodio::mixer::Mixer;
use rodio::source::{SineWave, Source};
use rodio::stream::MixerDeviceSink;
use rodio::DeviceSinkBuilder;

use crust_core::sound::{SoundEvent, SoundEventSetting, SoundSettings};

/// Maximum size of a user-supplied sound file we're willing to load into
/// memory. Larger files are rejected and fall back to the default ping so
/// a misconfigured "pick an MP3 as your mention sound" doesn't OOM the
/// client.  16 MiB is comfortable for any reasonable ping/ding clip.
const MAX_SOUND_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Hard rate limit per event.  Without this, a raid / sub burst can
/// spawn dozens of overlapping tones in the same frame.
const MIN_PLAYBACK_INTERVAL: Duration = Duration::from_millis(120);

/// Cached raw file bytes keyed by absolute-ish path. The controller holds
/// the cache behind a `Mutex` so any thread invoking [`SoundController::play_event`]
/// can decode without blocking the UI thread on disk I/O after the first hit.
#[derive(Default)]
struct SoundCache {
    /// Raw bytes keyed by the configured path. We re-decode on each play
    /// (cheap for short pings) instead of storing decoded samples, which
    /// keeps the cache format-agnostic.
    entries: HashMap<PathBuf, Arc<[u8]>>,
    /// Paths we've already tried and failed to load - avoids re-hitting
    /// the filesystem on every mention.
    failed: HashMap<PathBuf, ()>,
}

/// Main entry point for emitting audio pings.
///
/// Cheap to clone? No - hold a single instance on `CrustApp` and reuse it.
/// The underlying [`MixerDeviceSink`] is intentionally *not* wrapped in an
/// `Arc` because dropping it must stop the audio thread; cloning it would
/// defeat the lifetime story.
pub struct SoundController {
    /// Lazily initialised on first play attempt. `None` until then.
    /// Wrapped in `Option` (not Arc) so init failures can be re-attempted
    /// next time (e.g. audio device plugged in after app start).
    handle: Mutex<Option<AudioHandle>>,
    settings: Mutex<SoundSettings>,
    suppressed: Mutex<bool>,
    cache: Mutex<SoundCache>,
    last_played: Mutex<HashMap<SoundEvent, Instant>>,
}

struct AudioHandle {
    /// Must stay alive for playback to continue; dropping it stops the
    /// audio thread. Stored alongside the mixer so we keep ownership.
    _sink: MixerDeviceSink,
    /// Cloneable mixer handle used to submit new sources.
    mixer: Mixer,
}

impl Default for SoundController {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundController {
    /// Build a new, dormant controller. The audio device is not opened
    /// until the first [`Self::play_event`] call succeeds in selecting a
    /// source to play.
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            settings: Mutex::new(SoundSettings::with_defaults()),
            suppressed: Mutex::new(false),
            cache: Mutex::new(SoundCache::default()),
            last_played: Mutex::new(HashMap::new()),
        }
    }

    /// Replace the cached [`SoundSettings`] snapshot. Called from the UI
    /// whenever the runtime emits `AppEvent::SoundSettingsUpdated`.
    pub fn apply_settings(&self, settings: SoundSettings) {
        let normalised = settings.normalised();
        if let Ok(mut guard) = self.settings.lock() {
            *guard = normalised;
        }
        // Invalidate the cache so a user who re-picks a file gets the new
        // bytes on the next play instead of whatever was decoded for the
        // old path.
        if let Ok(mut cache) = self.cache.lock() {
            cache.entries.clear();
            cache.failed.clear();
        }
    }

    /// Toggle the global suppressed flag. The UI calls this with
    /// `streamer_mode_active && streamer_suppress_sounds` whenever either
    /// input changes.
    pub fn set_suppressed(&self, suppressed: bool) {
        if let Ok(mut guard) = self.suppressed.lock() {
            *guard = suppressed;
        }
    }

    /// Whether playback is currently gated off by streamer mode.
    pub fn is_suppressed(&self) -> bool {
        self.suppressed.lock().map(|g| *g).unwrap_or(false)
    }

    /// Play the default ping for `event`.  Respects streamer-mode
    /// suppression and per-event enable/volume.
    pub fn play_event(&self, event: SoundEvent) {
        let setting = self
            .settings
            .lock()
            .ok()
            .map(|g| g.get(event))
            .unwrap_or_default();
        self.play_event_with(event, &setting, None);
    }

    /// Play a highlight-rule override. The supplied `override_path` comes
    /// from [`crust_core::highlight::HighlightRule::sound_url`]; volume
    /// follows the `CustomHighlight` event's configuration.
    pub fn play_highlight_override(&self, override_path: Option<&str>) {
        let setting = self
            .settings
            .lock()
            .ok()
            .map(|g| g.get(SoundEvent::CustomHighlight))
            .unwrap_or_default();
        self.play_event_with(SoundEvent::CustomHighlight, &setting, override_path);
    }

    /// Force-play a preview of `event` ignoring streamer-mode suppression
    /// and any rate limiter. Used by the settings page "Preview" button so
    /// clicking it while streamer mode is ON still lets the user verify
    /// their chosen file.
    pub fn preview_event(&self, event: SoundEvent) {
        let setting = self
            .settings
            .lock()
            .ok()
            .map(|g| g.get(event))
            .unwrap_or_default();
        self.play_raw(event, &setting, None, /*force=*/ true);
    }

    fn play_event_with(
        &self,
        event: SoundEvent,
        setting: &SoundEventSetting,
        override_path: Option<&str>,
    ) {
        if !setting.enabled {
            return;
        }
        if self.is_suppressed() {
            return;
        }
        self.play_raw(event, setting, override_path, false);
    }

    fn play_raw(
        &self,
        event: SoundEvent,
        setting: &SoundEventSetting,
        override_path: Option<&str>,
        force: bool,
    ) {
        if !force && !self.allow_next_play(event) {
            return;
        }

        let path = override_path
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let trimmed = setting.path.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(trimmed))
                }
            });

        let mixer = match self.ensure_mixer() {
            Some(m) => m,
            None => return,
        };

        let volume = setting.clamped_volume();

        let played = if let Some(ref path) = path {
            self.play_file(&mixer, path, volume)
        } else {
            false
        };

        if !played {
            self.play_default(&mixer, event, volume);
        }
    }

    fn allow_next_play(&self, event: SoundEvent) -> bool {
        let Ok(mut map) = self.last_played.lock() else {
            return true;
        };
        let now = Instant::now();
        let allow = match map.get(&event) {
            Some(last) => now.duration_since(*last) >= MIN_PLAYBACK_INTERVAL,
            None => true,
        };
        if allow {
            map.insert(event, now);
        }
        allow
    }

    fn ensure_mixer(&self) -> Option<Mixer> {
        let mut guard = self.handle.lock().ok()?;
        if guard.is_none() {
            match DeviceSinkBuilder::open_default_sink() {
                Ok(sink) => {
                    let mixer = sink.mixer().clone();
                    *guard = Some(AudioHandle { _sink: sink, mixer });
                }
                Err(e) => {
                    tracing::debug!(
                        "Audio output unavailable, sound notifications disabled: {e}"
                    );
                    return None;
                }
            }
        }
        guard.as_ref().map(|h| h.mixer.clone())
    }

    fn play_file(&self, mixer: &Mixer, path: &Path, volume: f32) -> bool {
        let bytes = self.load_file_bytes(path);
        let Some(bytes) = bytes else {
            return false;
        };
        match rodio::Decoder::try_from(Cursor::new(bytes)) {
            Ok(decoder) => {
                mixer.add(decoder.amplify(volume));
                true
            }
            Err(e) => {
                tracing::debug!(
                    "Failed to decode sound file {}: {e}; falling back to default ping",
                    path.display()
                );
                self.mark_failed(path);
                false
            }
        }
    }

    /// Submit the built-in synthesised ping for `event`. Each event has a
    /// distinct tonal signature so users can tell them apart without
    /// configuring custom files:
    ///
    /// | Event              | Signature                                     |
    /// | ------------------ | --------------------------------------------- |
    /// | `Mention`          | two-tone bing-bong 880 Hz -> 660 Hz            |
    /// | `Whisper`          | soft single low ding at 520 Hz                |
    /// | `Subscribe`        | rising major-triad arpeggio C5 -> E5 -> G5      |
    /// | `Raid`             | urgent two-pulse 988 Hz repeat                |
    /// | `CustomHighlight`  | crisp double-tap at 784 Hz                    |
    ///
    /// Volumes are scaled by `0.4` on top of the caller's `volume` since
    /// raw sine waves at unit amplitude are far louder than a typical
    /// `.wav` ping file.
    fn play_default(&self, mixer: &Mixer, event: SoundEvent, volume: f32) {
        let gain = volume * 0.4;
        match event {
            SoundEvent::Mention => {
                // Classic "bing-bong": 880 Hz then 660 Hz, 80 ms each.
                self.push_tone(mixer, 880.0, 80, 0, gain);
                self.push_tone(mixer, 660.0, 80, 80, gain);
            }
            SoundEvent::Whisper => {
                // Softer, lower, and longer so an incoming DM feels
                // distinct from an @mention without being startling.
                self.push_tone(mixer, 520.0, 220, 0, gain * 0.85);
            }
            SoundEvent::Subscribe => {
                // Rising C5 / E5 / G5 arpeggio - celebratory fanfare.
                self.push_tone(mixer, 523.25, 90, 0, gain);
                self.push_tone(mixer, 659.25, 90, 90, gain);
                self.push_tone(mixer, 783.99, 140, 180, gain);
            }
            SoundEvent::Raid => {
                // Two urgent high-pitched pulses - reads as "alert".
                self.push_tone(mixer, 987.77, 90, 0, gain);
                self.push_tone(mixer, 987.77, 90, 140, gain);
                self.push_tone(mixer, 1318.51, 140, 280, gain);
            }
            SoundEvent::CustomHighlight => {
                // Crisp same-pitch double-tap at G5, distinct from the
                // descending Mention bing-bong.
                self.push_tone(mixer, 783.99, 60, 0, gain);
                self.push_tone(mixer, 783.99, 60, 100, gain);
            }
        }
    }

    /// Add a single sine-wave pulse to the mixer. `duration_ms` is the
    /// on-time, `delay_ms` offsets it into the future (0 = play now).
    /// Each pulse gets a tiny fade-in / fade-out so the abrupt start /
    /// stop of `take_duration` doesn't produce audible clicks.
    fn push_tone(&self, mixer: &Mixer, freq: f32, duration_ms: u64, delay_ms: u64, gain: f32) {
        let fade = Duration::from_millis((duration_ms / 4).max(4).min(15));
        let tone = SineWave::new(freq)
            .take_duration(Duration::from_millis(duration_ms))
            .fade_in(fade)
            .fade_out(fade)
            .amplify(gain)
            .delay(Duration::from_millis(delay_ms));
        mixer.add(tone);
    }

    fn load_file_bytes(&self, path: &Path) -> Option<Arc<[u8]>> {
        {
            let cache = self.cache.lock().ok()?;
            if cache.failed.contains_key(path) {
                return None;
            }
            if let Some(bytes) = cache.entries.get(path) {
                return Some(bytes.clone());
            }
        }

        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("Sound file unavailable ({}): {e}", path.display());
                self.mark_failed(path);
                return None;
            }
        };

        if metadata.len() > MAX_SOUND_FILE_BYTES {
            tracing::warn!(
                "Sound file {} exceeds {} byte limit; ignoring",
                path.display(),
                MAX_SOUND_FILE_BYTES
            );
            self.mark_failed(path);
            return None;
        }

        match fs::read(path) {
            Ok(bytes) => {
                let arc: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
                if let Ok(mut cache) = self.cache.lock() {
                    cache.entries.insert(path.to_path_buf(), arc.clone());
                }
                Some(arc)
            }
            Err(e) => {
                tracing::debug!("Failed to read sound file {}: {e}", path.display());
                self.mark_failed(path);
                None
            }
        }
    }

    fn mark_failed(&self, path: &Path) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.failed.insert(path.to_path_buf(), ());
            cache.entries.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_blocks_rapid_repeats() {
        let ctl = SoundController::new();
        assert!(ctl.allow_next_play(SoundEvent::Mention));
        // Immediately second call: rate-limited.
        assert!(!ctl.allow_next_play(SoundEvent::Mention));
        // Different event type: not rate-limited.
        assert!(ctl.allow_next_play(SoundEvent::Raid));
    }

    #[test]
    fn suppressed_flag_roundtrips() {
        let ctl = SoundController::new();
        assert!(!ctl.is_suppressed());
        ctl.set_suppressed(true);
        assert!(ctl.is_suppressed());
        ctl.set_suppressed(false);
        assert!(!ctl.is_suppressed());
    }

    #[test]
    fn missing_file_is_remembered_as_failed() {
        let ctl = SoundController::new();
        let path = Path::new("/definitely/not/a/real/sound/file.wav");
        assert!(ctl.load_file_bytes(path).is_none());
        // Second call should use the failed cache without hitting disk
        // again; we just verify it still returns None.
        assert!(ctl.load_file_bytes(path).is_none());
    }

    #[test]
    fn apply_settings_clears_cache() {
        let ctl = SoundController::new();
        {
            let mut cache = ctl.cache.lock().unwrap();
            cache.entries.insert(
                PathBuf::from("/tmp/stale.wav"),
                Arc::from(vec![0u8; 4].into_boxed_slice()),
            );
        }
        ctl.apply_settings(SoundSettings::with_defaults());
        let cache = ctl.cache.lock().unwrap();
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn disabled_event_is_noop() {
        // play_event_with should bail when `enabled = false` without
        // touching the audio device or the rate limiter.
        let ctl = SoundController::new();
        let setting = SoundEventSetting {
            enabled: false,
            path: String::new(),
            volume: 0.8,
        };
        ctl.play_event_with(SoundEvent::Mention, &setting, None);
        // Rate limiter untouched.
        assert!(ctl.allow_next_play(SoundEvent::Mention));
    }

    #[test]
    fn suppressed_event_is_noop() {
        let ctl = SoundController::new();
        ctl.set_suppressed(true);
        let setting = SoundEventSetting {
            enabled: true,
            path: String::new(),
            volume: 0.8,
        };
        ctl.play_event_with(SoundEvent::Mention, &setting, None);
        assert!(ctl.allow_next_play(SoundEvent::Mention));
    }
}
