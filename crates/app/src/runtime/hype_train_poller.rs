//! Unofficial hype-train poller.
//!
//! Twitch's `channel.hype_train.*` EventSub topics are gated to the
//! broadcaster's own token (see `crates/twitch/src/eventsub/registry.rs`),
//! so the existing subscription path only delivers hype-train banners when
//! the logged-in user is also the channel they're watching.  This task
//! fills the viewer gap: for any Twitch channel the user has joined it
//! polls the unofficial GQL endpoint every [`POLL_INTERVAL`] seconds and
//! emits [`AppEvent::HypeTrainUpdated`] whenever the server-reported train
//! changes.  Banner rendering reuses the same reducer path as the EventSub
//! topic - see `AppState::apply_hype_train_update`.
//!
//! The endpoint is not a supported public API.  Failures are logged and
//! the poller skips the channel until the next tick; the UI simply shows
//! nothing when the call fails.  Nothing else in the app depends on this
//! task, so if Twitch ever blocks the query the rest of the session keeps
//! working untouched.

use std::collections::HashMap;
use std::time::Duration;

use crust_core::{
    events::AppEvent,
    model::ChannelId,
};
use crust_twitch::gql::{fetch_hype_train, GqlHypeTrain};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, trace, warn};

/// How often the poller re-checks each joined Twitch channel.  20s keeps
/// the banner progress visibly animating while staying well below anything
/// Twitch is likely to throttle.
pub const POLL_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum HypeTrainPollerCommand {
    /// Replace the set of channel logins that should be polled.  Logins
    /// added since the last `SetChannels` are polled on the next tick;
    /// logins removed have any still-visible "end" event emitted so the
    /// banner expires cleanly.
    SetChannels(Vec<(ChannelId, String)>),
}

/// Spawn the poller on an explicit tokio runtime handle.  Mirrors the
/// spawn-on helper used by the live-feed task so the caller doesn't have
/// to be inside an async context.
pub fn spawn_on(
    handle: &tokio::runtime::Handle,
    http: reqwest::Client,
    cmd_rx: mpsc::Receiver<HypeTrainPollerCommand>,
    evt_tx: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(run(http, cmd_rx, evt_tx))
}

async fn run(
    http: reqwest::Client,
    mut cmd_rx: mpsc::Receiver<HypeTrainPollerCommand>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let mut tracked: HashMap<ChannelId, ChannelEntry> = HashMap::new();
    let mut tick = interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    HypeTrainPollerCommand::SetChannels(channels) => {
                        apply_channel_update(&mut tracked, channels, &evt_tx).await;
                    }
                }
            }
            _ = tick.tick() => {
                if tracked.is_empty() {
                    continue;
                }
                poll_once(&http, &mut tracked, &evt_tx).await;
            }
            else => break,
        }
    }
}

struct ChannelEntry {
    login: String,
    last: Option<GqlHypeTrain>,
    /// `true` once we've emitted an "end" for the current train so we don't
    /// re-emit every tick while the train is gone but the cooldown banner
    /// is still visible.
    end_emitted: bool,
    /// Set after the first failing GQL response so we only log the error
    /// once per channel (otherwise the logs would fill up with identical
    /// "persisted query not found" lines every 20s).
    logged_failure: bool,
}

async fn apply_channel_update(
    tracked: &mut HashMap<ChannelId, ChannelEntry>,
    channels: Vec<(ChannelId, String)>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let desired: HashMap<ChannelId, String> = channels.into_iter().collect();

    // Drop removed channels.  If we had an active train, emit a synthetic
    // "end" so the UI banner expires instead of sticking forever.
    let removed: Vec<ChannelId> = tracked
        .keys()
        .filter(|id| !desired.contains_key(*id))
        .cloned()
        .collect();
    for id in removed {
        if let Some(entry) = tracked.remove(&id) {
            if let Some(train) = &entry.last {
                if !entry.end_emitted {
                    emit_end(evt_tx, &id, train).await;
                }
            }
        }
    }

    // Insert new channels; the next tick will fetch their state.
    for (id, login) in desired {
        tracked.entry(id).or_insert(ChannelEntry {
            login,
            last: None,
            end_emitted: false,
            logged_failure: false,
        });
    }
}

async fn poll_once(
    http: &reqwest::Client,
    tracked: &mut HashMap<ChannelId, ChannelEntry>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    // Snapshot ids so we can mutate the map inside the loop.
    let ids: Vec<ChannelId> = tracked.keys().cloned().collect();
    for id in ids {
        let login = match tracked.get(&id) {
            Some(e) => e.login.clone(),
            None => continue,
        };

        match fetch_hype_train(http, &login).await {
            Ok(Some(train)) => {
                let phase = classify_phase(tracked.get(&id), &train);
                if let Some(entry) = tracked.get_mut(&id) {
                    let changed = entry
                        .last
                        .as_ref()
                        .map(|prev| prev != &train)
                        .unwrap_or(true);
                    entry.last = Some(train.clone());
                    entry.end_emitted = phase == "end";
                    if changed {
                        emit_update(evt_tx, &id, phase, &train).await;
                    } else {
                        trace!(channel = %id.0, "hype-train poll: unchanged");
                    }
                }
            }
            Ok(None) => {
                if let Some(entry) = tracked.get_mut(&id) {
                    if let Some(prev) = entry.last.clone() {
                        if !entry.end_emitted {
                            emit_end(evt_tx, &id, &prev).await;
                        }
                        entry.end_emitted = true;
                        entry.last = None;
                    }
                }
            }
            Err(err) => {
                // Surface the first failure at info level so users can see
                // exactly why the GQL endpoint rejected us (persisted-query
                // gating, field rename, rate limit).  Subsequent failures
                // stay at trace so we don't spam the log.
                if let Some(entry) = tracked.get_mut(&id) {
                    if !entry.logged_failure {
                        info!(channel = %id.0, "hype-train gql failed: {err}");
                        entry.logged_failure = true;
                    } else {
                        trace!(channel = %id.0, "hype-train gql failed: {err}");
                    }
                }
            }
        }
    }
}

fn classify_phase(prev: Option<&ChannelEntry>, train: &GqlHypeTrain) -> &'static str {
    if train.ended_at.is_some() {
        return "end";
    }
    match prev {
        Some(e) => match &e.last {
            Some(p) if p.train_id == train.train_id => "progress",
            _ => "begin",
        },
        None => "begin",
    }
}

async fn emit_update(
    evt_tx: &mpsc::Sender<AppEvent>,
    channel: &ChannelId,
    phase: &str,
    train: &GqlHypeTrain,
) {
    let payload = AppEvent::HypeTrainUpdated {
        channel: channel.clone(),
        phase: phase.to_owned(),
        level: train.level,
        progress: train.progress,
        goal: train.goal,
        total: train.total,
        top_contributor_login: train.top_contributor_login.clone(),
        top_contributor_type: train.top_contributor_type.clone(),
        top_contributor_total: train.top_contributor_total,
        ends_at: train.ended_at.clone().or_else(|| train.expires_at.clone()),
    };
    if let Err(e) = evt_tx.send(payload).await {
        warn!(channel = %channel.0, "hype-train: failed to send update: {e}");
    }
}

async fn emit_end(
    evt_tx: &mpsc::Sender<AppEvent>,
    channel: &ChannelId,
    train: &GqlHypeTrain,
) {
    emit_update(evt_tx, channel, "end", train).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_train(id: &str, level: u32, progress: u64) -> GqlHypeTrain {
        GqlHypeTrain {
            train_id: id.to_owned(),
            level,
            progress,
            goal: 500,
            total: progress,
            expires_at: None,
            ended_at: None,
            top_contributor_login: None,
            top_contributor_type: None,
            top_contributor_total: None,
        }
    }

    #[test]
    fn classify_phase_returns_begin_for_first_sighting() {
        let train = sample_train("t1", 1, 100);
        assert_eq!(classify_phase(None, &train), "begin");
    }

    #[test]
    fn classify_phase_returns_progress_for_same_train_id() {
        let prev_train = sample_train("t1", 1, 100);
        let entry = ChannelEntry {
            login: "x".into(),
            last: Some(prev_train),
            end_emitted: false,
            logged_failure: false,
        };
        let updated = sample_train("t1", 2, 250);
        assert_eq!(classify_phase(Some(&entry), &updated), "progress");
    }

    #[test]
    fn classify_phase_returns_begin_when_train_id_changes() {
        let prev_train = sample_train("t1", 5, 1500);
        let entry = ChannelEntry {
            login: "x".into(),
            last: Some(prev_train),
            end_emitted: true,
            logged_failure: false,
        };
        let next = sample_train("t2", 1, 50);
        assert_eq!(classify_phase(Some(&entry), &next), "begin");
    }

    #[test]
    fn classify_phase_returns_end_when_train_has_ended_at() {
        let mut train = sample_train("t1", 5, 1500);
        train.ended_at = Some("2026-04-23T10:30:00Z".into());
        assert_eq!(classify_phase(None, &train), "end");
    }

    #[tokio::test]
    async fn apply_channel_update_adds_and_removes_entries() {
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        let mut tracked: HashMap<ChannelId, ChannelEntry> = HashMap::new();
        let ch = ChannelId::new("moonmoon");

        apply_channel_update(
            &mut tracked,
            vec![(ch.clone(), "moonmoon".into())],
            &tx,
        )
        .await;
        assert!(tracked.contains_key(&ch));

        // Seed a fake last-known train, then remove the channel and expect
        // a synthetic "end" event on the channel output.
        if let Some(entry) = tracked.get_mut(&ch) {
            entry.last = Some(sample_train("t1", 3, 400));
        }
        apply_channel_update(&mut tracked, Vec::new(), &tx).await;
        assert!(tracked.is_empty());

        let evt = rx.recv().await.expect("end event");
        match evt {
            AppEvent::HypeTrainUpdated { phase, channel, .. } => {
                assert_eq!(phase, "end");
                assert_eq!(channel, ch);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
