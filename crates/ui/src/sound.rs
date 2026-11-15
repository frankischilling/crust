//! Notification sounds run on one lazily started worker.
//!
//! The UI submits to a bounded queue without waiting for device setup, file
//! reads, or decoder initialization. Settings and suppression changes cancel
//! stale requests, including work still being prepared by the worker.

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::mixer::Mixer;
use rodio::source::{SineWave, Source};
use rodio::stream::MixerDeviceSink;
use rodio::DeviceSinkBuilder;

use crust_core::sound::{SoundEvent, SoundEventSetting, SoundSettings};

const MAX_SOUND_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MIN_PLAYBACK_INTERVAL: Duration = Duration::from_millis(120);
const AUDIO_QUEUE_CAPACITY: usize = 8;
const DEVICE_RETRY_INTERVAL: Duration = Duration::from_secs(10);
const WORKER_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct SoundCache {
    entries: HashMap<PathBuf, Arc<[u8]>>,
    failed: HashMap<PathBuf, ()>,
}

#[derive(Default)]
struct PlaybackControls {
    suppressed: AtomicBool,
    settings_generation: AtomicU64,
    notification_generation: AtomicU64,
    stopped: AtomicBool,
}

impl PlaybackControls {
    fn permits(&self, request: &PlaybackRequest) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && request.settings_generation == self.settings_generation.load(Ordering::Acquire)
            && (request.force
                || (!self.suppressed.load(Ordering::Acquire)
                    && request.notification_generation
                        == self.notification_generation.load(Ordering::Acquire)))
    }
}

struct PlaybackRequest {
    event: SoundEvent,
    setting: SoundEventSetting,
    override_path: Option<PathBuf>,
    force: bool,
    settings_generation: u64,
    notification_generation: u64,
}

type WorkerTask = Arc<dyn Fn(Receiver<PlaybackRequest>, Arc<PlaybackControls>) + Send + Sync>;

struct ControllerState {
    settings: SoundSettings,
    sender: Option<SyncSender<PlaybackRequest>>,
    next_worker_attempt: Option<Instant>,
    last_submitted: HashMap<SoundEvent, Instant>,
}

/// Keeps sound configuration on the caller and playback resources on a worker.
///
/// The worker never accesses the controller mutex. Dropping this controller
/// cancels pending playback and closes its queue without joining a busy worker.
pub struct SoundController {
    state: Mutex<ControllerState>,
    controls: Arc<PlaybackControls>,
    worker_task: WorkerTask,
}

impl Default for SoundController {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundController {
    pub fn new() -> Self {
        Self::with_worker(Arc::new(|receiver, controls| {
            run_worker(receiver, controls, NativeAudio::default());
        }))
    }

    fn with_worker(worker_task: WorkerTask) -> Self {
        Self {
            state: Mutex::new(ControllerState {
                settings: SoundSettings::with_defaults(),
                sender: None,
                next_worker_attempt: None,
                last_submitted: HashMap::new(),
            }),
            controls: Arc::new(PlaybackControls::default()),
            worker_task,
        }
    }

    /// Re-picking a sound invalidates cached file bytes before the next play.
    /// Requests prepared using the previous settings are discarded.
    pub fn apply_settings(&self, settings: SoundSettings) {
        let normalised = settings.normalised();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.settings = normalised;
        self.controls
            .settings_generation
            .fetch_add(1, Ordering::AcqRel);
    }

    pub fn set_suppressed(&self, suppressed: bool) {
        if self.controls.suppressed.swap(suppressed, Ordering::AcqRel) != suppressed {
            // A mute/unmute cycle must not release notifications queued before it.
            self.controls
                .notification_generation
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn is_suppressed(&self) -> bool {
        self.controls.suppressed.load(Ordering::Acquire)
    }

    pub fn play_event(&self, event: SoundEvent) {
        self.submit(event, None, false);
    }

    pub fn play_highlight_override(&self, override_path: Option<&str>) {
        self.submit(SoundEvent::CustomHighlight, override_path, false);
    }

    /// Settings previews bypass event enablement, suppression, and rate limits.
    pub fn preview_event(&self, event: SoundEvent) {
        self.submit(event, None, true);
    }

    fn submit(&self, event: SoundEvent, override_path: Option<&str>, force: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let setting = state.settings.get(event);
        if !force && (!setting.enabled || self.is_suppressed()) {
            return false;
        }

        let now = Instant::now();
        if !force && !interval_elapsed(state.last_submitted.get(&event), now) {
            return false;
        }
        if state.sender.is_none() {
            if state.next_worker_attempt.is_some_and(|next| now < next) {
                return false;
            }
            let (sender, receiver) = mpsc::sync_channel(AUDIO_QUEUE_CAPACITY);
            let task = self.worker_task.clone();
            let controls = self.controls.clone();
            match std::thread::Builder::new()
                .name("crust-audio".to_owned())
                .spawn(move || task(receiver, controls))
            {
                Ok(_) => {
                    state.sender = Some(sender);
                    state.next_worker_attempt = None;
                }
                Err(_) => {
                    state.next_worker_attempt = Some(now + WORKER_RETRY_INTERVAL);
                    tracing::debug!("Audio worker unavailable; will retry later");
                    return false;
                }
            }
        }

        let request = PlaybackRequest {
            event,
            setting,
            override_path: override_path
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            force,
            settings_generation: self.controls.settings_generation.load(Ordering::Acquire),
            notification_generation: self
                .controls
                .notification_generation
                .load(Ordering::Acquire),
        };
        match state.sender.as_ref().unwrap().try_send(request) {
            Ok(()) => {
                if !force {
                    state.last_submitted.insert(event, now);
                }
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                state.sender = None;
                state.next_worker_attempt = Some(now + WORKER_RETRY_INTERVAL);
                false
            }
        }
    }
}

impl Drop for SoundController {
    fn drop(&mut self) {
        self.controls.stopped.store(true, Ordering::Release);
    }
}

fn interval_elapsed(previous: Option<&Instant>, now: Instant) -> bool {
    previous
        .is_none_or(|previous| now.saturating_duration_since(*previous) >= MIN_PLAYBACK_INTERVAL)
}

trait AudioBackend {
    type Prepared;

    fn clear_cache(&mut self);
    fn prepare(&mut self, request: &PlaybackRequest) -> Option<Self::Prepared>;
    fn play(&mut self, prepared: Self::Prepared);
}

fn run_worker<B: AudioBackend>(
    receiver: Receiver<PlaybackRequest>,
    controls: Arc<PlaybackControls>,
    mut backend: B,
) {
    let mut cache_generation = None;
    let mut last_played = HashMap::new();
    while let Ok(request) = receiver.recv() {
        if controls.stopped.load(Ordering::Acquire) {
            break;
        }
        if !controls.permits(&request) {
            continue;
        }
        if cache_generation != Some(request.settings_generation) {
            backend.clear_cache();
            cache_generation = Some(request.settings_generation);
        }
        if !request.force && !interval_elapsed(last_played.get(&request.event), Instant::now()) {
            continue;
        }

        let started = Instant::now();
        let prepared = backend.prepare(&request);
        tracing::debug!(
            target: "crust_ui::sound",
            operation = "prepare",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Audio worker operation"
        );
        let Some(prepared) = prepared else { continue };
        // Device opening or file reads may have taken time. Re-check after that
        // work so a settings change or mute cannot release an obsolete sound.
        if !controls.permits(&request) {
            continue;
        }
        if !request.force {
            last_played.insert(request.event, Instant::now());
        }
        backend.play(prepared);
    }
}

struct AudioHandle {
    _sink: MixerDeviceSink,
    mixer: Mixer,
}

#[derive(Default)]
struct NativeAudio {
    handle: Option<AudioHandle>,
    next_device_attempt: Option<Instant>,
    cache: SoundCache,
}

enum PreparedSound {
    File {
        mixer: Mixer,
        decoder: rodio::Decoder<Cursor<Arc<[u8]>>>,
        volume: f32,
    },
    Default {
        mixer: Mixer,
        event: SoundEvent,
        volume: f32,
    },
}

impl AudioBackend for NativeAudio {
    type Prepared = PreparedSound;

    fn clear_cache(&mut self) {
        self.cache.entries.clear();
        self.cache.failed.clear();
    }

    fn prepare(&mut self, request: &PlaybackRequest) -> Option<Self::Prepared> {
        let mixer = self.ensure_mixer_with(Instant::now(), || {
            DeviceSinkBuilder::open_default_sink().ok().map(|sink| {
                let mixer = sink.mixer().clone();
                AudioHandle { _sink: sink, mixer }
            })
        })?;
        let volume = request.setting.clamped_volume();
        let path = request.override_path.clone().or_else(|| {
            let path = request.setting.path.trim();
            (!path.is_empty()).then(|| PathBuf::from(path))
        });
        if let Some(path) = path {
            if let Some(bytes) = self.load_file_bytes(&path) {
                match rodio::Decoder::try_from(Cursor::new(bytes)) {
                    Ok(decoder) => {
                        return Some(PreparedSound::File {
                            mixer,
                            decoder,
                            volume,
                        })
                    }
                    Err(_) => {
                        tracing::debug!(
                            "Could not decode configured sound; using the default tone"
                        );
                        self.mark_failed(&path);
                    }
                }
            }
        }
        Some(PreparedSound::Default {
            mixer,
            event: request.event,
            volume,
        })
    }

    fn play(&mut self, prepared: Self::Prepared) {
        match prepared {
            PreparedSound::File {
                mixer,
                decoder,
                volume,
            } => mixer.add(decoder.amplify(volume)),
            PreparedSound::Default {
                mixer,
                event,
                volume,
            } => self.play_default(&mixer, event, volume),
        }
    }
}

impl NativeAudio {
    fn ensure_mixer_with(
        &mut self,
        now: Instant,
        open: impl FnOnce() -> Option<AudioHandle>,
    ) -> Option<Mixer> {
        if self.handle.is_none() {
            if self.next_device_attempt.is_some_and(|next| now < next) {
                return None;
            }
            let started = Instant::now();
            self.handle = open();
            tracing::debug!(
                target: "crust_ui::sound",
                operation = "open_device",
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                "Audio worker operation"
            );
            if self.handle.is_none() {
                self.next_device_attempt = Some(Instant::now() + DEVICE_RETRY_INTERVAL);
                tracing::debug!("Audio output unavailable; will retry later");
                return None;
            }
            self.next_device_attempt = None;
        }
        self.handle.as_ref().map(|handle| handle.mixer.clone())
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

    fn load_file_bytes(&mut self, path: &Path) -> Option<Arc<[u8]>> {
        if self.cache.failed.contains_key(path) {
            return None;
        }
        if let Some(bytes) = self.cache.entries.get(path) {
            return Some(bytes.clone());
        }
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => {
                tracing::debug!("Configured sound file unavailable; using the default tone");
                self.mark_failed(path);
                return None;
            }
        };
        if metadata.len() > MAX_SOUND_FILE_BYTES {
            tracing::warn!("Configured sound exceeds the file size limit; using the default tone");
            self.mark_failed(path);
            return None;
        }
        match fs::read(path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_SOUND_FILE_BYTES => {
                let bytes: Arc<[u8]> = bytes.into();
                self.cache.entries.insert(path.to_path_buf(), bytes.clone());
                Some(bytes)
            }
            _ => {
                tracing::debug!("Could not read configured sound; using the default tone");
                self.mark_failed(path);
                None
            }
        }
    }

    fn mark_failed(&mut self, path: &Path) {
        self.cache.failed.insert(path.to_path_buf(), ());
        self.cache.entries.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug, Clone, PartialEq)]
    struct PreparedProbe {
        event: SoundEvent,
        setting: SoundEventSetting,
        override_path: Option<PathBuf>,
        force: bool,
    }

    struct FakeAudio {
        entered: mpsc::Sender<PreparedProbe>,
        played: mpsc::Sender<PreparedProbe>,
        release_first: Option<Receiver<()>>,
        cache_clears: Arc<AtomicUsize>,
    }

    impl AudioBackend for FakeAudio {
        type Prepared = PreparedProbe;

        fn clear_cache(&mut self) {
            self.cache_clears.fetch_add(1, Ordering::Relaxed);
        }

        fn prepare(&mut self, request: &PlaybackRequest) -> Option<PreparedProbe> {
            let prepared = PreparedProbe {
                event: request.event,
                setting: request.setting.clone(),
                override_path: request.override_path.clone(),
                force: request.force,
            };
            let _ = self.entered.send(prepared.clone());
            if let Some(release) = self.release_first.take() {
                let _ = release.recv();
            }
            Some(prepared)
        }

        fn play(&mut self, prepared: PreparedProbe) {
            let _ = self.played.send(prepared);
        }
    }

    struct WorkerProbe {
        entered: Receiver<PreparedProbe>,
        played: Receiver<PreparedProbe>,
        release: Option<mpsc::Sender<()>>,
        cache_clears: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
    }

    impl WorkerProbe {
        fn release(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
        }

        fn entered(&self) -> PreparedProbe {
            self.entered
                .recv_timeout(Duration::from_secs(2))
                .expect("audio worker did not prepare")
        }

        fn played(&self) -> PreparedProbe {
            self.played
                .recv_timeout(Duration::from_secs(2))
                .expect("audio worker did not play")
        }
    }

    impl Drop for WorkerProbe {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn fake_controller(block_first: bool) -> (Arc<SoundController>, WorkerProbe) {
        let (entered_tx, entered) = mpsc::channel();
        let (played_tx, played) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_first = Arc::new(Mutex::new(block_first.then_some(release_rx)));
        let cache_clears = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let worker_clears = cache_clears.clone();
        let worker_starts = starts.clone();
        let controller = SoundController::with_worker(Arc::new(move |receiver, controls| {
            worker_starts.fetch_add(1, Ordering::Relaxed);
            let release_first = release_first.lock().unwrap().take();
            run_worker(
                receiver,
                controls,
                FakeAudio {
                    entered: entered_tx.clone(),
                    played: played_tx.clone(),
                    release_first,
                    cache_clears: worker_clears.clone(),
                },
            );
        }));
        (
            Arc::new(controller),
            WorkerProbe {
                entered,
                played,
                release: Some(release_tx),
                cache_clears,
                starts,
            },
        )
    }

    #[test]
    fn blocked_audio_keeps_submission_and_settings_responsive_with_a_bounded_queue() {
        let (controller, mut probe) = fake_controller(true);
        assert!(controller.submit(SoundEvent::Mention, None, false));
        assert_eq!(probe.entered().event, SoundEvent::Mention);

        let ui = controller.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let submitter = std::thread::spawn(move || {
            let mut settings = SoundSettings::with_defaults();
            settings.set(
                SoundEvent::Whisper,
                SoundEventSetting {
                    enabled: false,
                    path: "new-preview.wav".into(),
                    volume: 0.2,
                },
            );
            ui.apply_settings(settings);
            ui.set_suppressed(true);
            let accepted = (0..AUDIO_QUEUE_CAPACITY + 32)
                .filter(|_| ui.submit(SoundEvent::Whisper, None, true))
                .count();
            let _ = done_tx.send((accepted, ui.is_suppressed()));
        });
        let result = done_rx.recv_timeout(Duration::from_secs(1));
        probe.release();
        submitter.join().unwrap();
        assert_eq!(
            result.expect("UI waited for audio preparation"),
            (AUDIO_QUEUE_CAPACITY, true)
        );
        for _ in 0..AUDIO_QUEUE_CAPACITY {
            let played = probe.played();
            assert_eq!(played.event, SoundEvent::Whisper);
            assert_eq!(played.setting.path, "new-preview.wav");
            assert_eq!(played.setting.volume, 0.2);
            assert!(played.force);
        }
        assert!(probe.played.try_recv().is_err());
        assert_eq!(probe.starts.load(Ordering::Relaxed), 1);
        assert_eq!(probe.cache_clears.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn settings_and_mute_changes_cancel_prepared_and_queued_notifications() {
        for change_settings in [false, true] {
            let (controller, mut probe) = fake_controller(true);
            assert!(controller.submit(SoundEvent::Mention, None, false));
            probe.entered();
            assert!(controller.submit(SoundEvent::Whisper, None, false));

            if change_settings {
                let mut settings = SoundSettings::with_defaults();
                settings.set(
                    SoundEvent::Raid,
                    SoundEventSetting {
                        enabled: true,
                        path: "latest.wav".into(),
                        volume: 0.4,
                    },
                );
                controller.apply_settings(settings);
            } else {
                controller.set_suppressed(true);
                controller.set_suppressed(false);
            }
            assert!(controller.submit(SoundEvent::Raid, None, false));
            probe.release();
            let played = probe.played();
            assert_eq!(played.event, SoundEvent::Raid);
            if change_settings {
                assert_eq!(played.setting.path, "latest.wav");
                assert_eq!(played.setting.volume, 0.4);
            }
            assert_eq!(probe.entered().event, SoundEvent::Raid);
            assert!(probe.entered.try_recv().is_err());
            assert!(probe.played.try_recv().is_err());
        }
    }

    #[test]
    fn normal_events_respect_enablement_suppression_and_per_event_rate_limits() {
        let (controller, probe) = fake_controller(false);
        let mut settings = SoundSettings::with_defaults();
        settings.set(
            SoundEvent::Mention,
            SoundEventSetting {
                enabled: false,
                ..Default::default()
            },
        );
        controller.apply_settings(settings);
        assert!(!controller.submit(SoundEvent::Mention, None, false));
        controller.set_suppressed(true);
        assert!(!controller.submit(SoundEvent::Whisper, None, false));
        assert!(controller.state.lock().unwrap().sender.is_none());

        controller.set_suppressed(false);
        assert!(controller.submit(SoundEvent::Whisper, None, false));
        assert!(!controller.submit(SoundEvent::Whisper, None, false));
        assert_eq!(probe.played().event, SoundEvent::Whisper);
        assert!(controller.submit(SoundEvent::Raid, None, false));
        assert_eq!(probe.played().event, SoundEvent::Raid);
        assert!(controller.submit(SoundEvent::CustomHighlight, Some("  override.wav "), false));
        assert_eq!(
            probe.played().override_path,
            Some(PathBuf::from("override.wav"))
        );
    }

    #[test]
    fn dropping_controller_cancels_audio_still_being_prepared() {
        let (controller, mut probe) = fake_controller(true);
        assert!(controller.submit(SoundEvent::Mention, None, false));
        probe.entered();
        drop(controller);
        probe.release();
        assert!(matches!(
            probe.played.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn absent_audio_device_retries_after_a_cooldown() {
        let mut audio = NativeAudio::default();
        let attempts = AtomicUsize::new(0);
        let fail = || {
            attempts.fetch_add(1, Ordering::Relaxed);
            None
        };
        assert!(audio.ensure_mixer_with(Instant::now(), fail).is_none());
        let retry_at = audio.next_device_attempt.unwrap();
        for _ in 0..100 {
            assert!(audio
                .ensure_mixer_with(retry_at - Duration::from_millis(1), fail)
                .is_none());
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(audio.ensure_mixer_with(retry_at, fail).is_none());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn worker_remembers_missing_files_and_clears_the_cache() {
        let mut audio = NativeAudio::default();
        let path = Path::new("/definitely/not/a/real/sound/file.wav");
        assert!(audio.load_file_bytes(path).is_none());
        assert!(audio.cache.failed.contains_key(path));
        assert!(audio.load_file_bytes(path).is_none());
        audio.cache.entries.insert(
            PathBuf::from("cached.wav"),
            Arc::from(vec![0u8; 4].into_boxed_slice()),
        );
        audio.clear_cache();
        assert!(audio.cache.entries.is_empty());
        assert!(audio.cache.failed.is_empty());
    }
}
