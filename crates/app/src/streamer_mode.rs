//! Broadcasting-software detection for streamer mode.
//!
//! Mirrors Chatterino's `StreamerMode` singleton: when the user picks `auto`
//! we periodically check whether OBS / Streamlabs / similar processes are
//! running and toggle the effective active flag accordingly.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crust_core::events::AppEvent;
use tokio::sync::{mpsc, watch};
#[cfg(unix)]
use tracing::debug;
use tracing::warn;

/// Process names recognised as broadcasting software (case-insensitive match).
#[cfg(windows)]
const BROADCASTING_BINARIES: &[&str] = &[
    "obs.exe",
    "obs64.exe",
    "PRISMLiveStudio.exe",
    "XSplit.Core.exe",
    "TwitchStudio.exe",
    "vMix64.exe",
    "Streamlabs OBS.exe",
];

#[cfg(not(windows))]
const BROADCASTING_BINARIES: &[&str] = &["obs", "Twitch Studio", "Streamlabs Desktop"];

/// Poll interval matches Chatterino (~20 seconds).
const POLL_INTERVAL: Duration = Duration::from_secs(20);

/// Streamer mode setting selected by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamerModeSetting {
    Off,
    Auto,
    On,
}

impl StreamerModeSetting {
    pub fn from_str(s: &str) -> Self {
        match s {
            "auto" => Self::Auto,
            "on" => Self::On,
            _ => Self::Off,
        }
    }
}

/// Spawn the background detector. The returned `setting_tx` allows the
/// runtime to update the user's chosen setting at any time. The detector
/// emits `StreamerModeActiveChanged` events whenever the effective state
/// changes.
pub fn spawn_detector(
    initial_setting: StreamerModeSetting,
    evt_tx: mpsc::Sender<AppEvent>,
) -> watch::Sender<StreamerModeSetting> {
    let (setting_tx, mut setting_rx) = watch::channel(initial_setting);
    let active_state = Arc::new(AtomicBool::new(false));

    tokio::spawn(async move {
        let mut current = *setting_rx.borrow();
        emit_initial(&active_state, current, &evt_tx).await;

        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick (we already evaluated above).
        interval.tick().await;

        loop {
            tokio::select! {
                changed = setting_rx.changed() => {
                    if changed.is_err() {
                        break; // channel closed → app shutting down
                    }
                    current = *setting_rx.borrow();
                    apply_setting(&active_state, current, &evt_tx).await;
                }
                _ = interval.tick(), if matches!(current, StreamerModeSetting::Auto) => {
                    let detected = is_broadcaster_software_active();
                    update_active(&active_state, detected, &evt_tx).await;
                }
            }
        }
    });

    setting_tx
}

async fn emit_initial(
    active: &AtomicBool,
    setting: StreamerModeSetting,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    apply_setting(active, setting, evt_tx).await;
}

async fn apply_setting(
    active: &AtomicBool,
    setting: StreamerModeSetting,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let next = match setting {
        StreamerModeSetting::Off => false,
        StreamerModeSetting::On => true,
        StreamerModeSetting::Auto => is_broadcaster_software_active(),
    };
    update_active(active, next, evt_tx).await;
}

async fn update_active(active: &AtomicBool, next: bool, evt_tx: &mpsc::Sender<AppEvent>) {
    let prev = active.swap(next, Ordering::SeqCst);
    if prev != next {
        let _ = evt_tx
            .send(AppEvent::StreamerModeActiveChanged { active: next })
            .await;
    }
}

/// Returns true when any broadcasting binary is currently running.
#[cfg(windows)]
fn is_broadcaster_software_active() -> bool {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW prevents a console flash when tasklist runs.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = Command::new("tasklist.exe")
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            warn!("Streamer mode: tasklist failed: {e}");
            return false;
        }
    };
    if !output.status.success() {
        warn!("Streamer mode: tasklist exited {}", output.status);
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    matches_any_binary(&stdout)
}

#[cfg(unix)]
fn is_broadcaster_software_active() -> bool {
    let pattern = BROADCASTING_BINARIES.join("|");
    let output = Command::new("pgrep").args(["-xi", &pattern]).output();
    match output {
        Ok(o) => o.status.success(),
        Err(e) => {
            debug!("Streamer mode: pgrep unavailable ({e})");
            false
        }
    }
}

#[cfg(not(any(windows, unix)))]
fn is_broadcaster_software_active() -> bool {
    false
}

/// Returns true if the tasklist CSV output contains any broadcasting binary.
/// Each CSV row's first field is the image name in quotes.
fn matches_any_binary(csv_stdout: &str) -> bool {
    for line in csv_stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('"') {
            continue;
        }
        let after = &trimmed[1..];
        let Some(end) = after.find('"') else {
            continue;
        };
        let name = &after[..end];
        for bin in BROADCASTING_BINARIES {
            if name.eq_ignore_ascii_case(bin) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_obs_in_tasklist_csv() {
        let csv = "\"explorer.exe\",\"1234\",\"Console\",\"1\",\"100,000 K\"\n\
                   \"obs64.exe\",\"5678\",\"Console\",\"1\",\"200,000 K\"\n";
        assert!(matches_any_binary(csv));
    }

    #[test]
    fn ignores_unrelated_processes() {
        let csv = "\"explorer.exe\",\"1234\",\"Console\",\"1\",\"100,000 K\"\n";
        assert!(!matches_any_binary(csv));
    }

    #[test]
    fn case_insensitive_match() {
        let csv = "\"OBS64.EXE\",\"1\",\"Console\",\"1\",\"1 K\"\n";
        assert!(matches_any_binary(csv));
    }

    #[test]
    fn setting_from_str_handles_known_values() {
        assert_eq!(
            StreamerModeSetting::from_str("off"),
            StreamerModeSetting::Off
        );
        assert_eq!(
            StreamerModeSetting::from_str("auto"),
            StreamerModeSetting::Auto
        );
        assert_eq!(StreamerModeSetting::from_str("on"), StreamerModeSetting::On);
        assert_eq!(
            StreamerModeSetting::from_str("garbage"),
            StreamerModeSetting::Off
        );
    }
}
