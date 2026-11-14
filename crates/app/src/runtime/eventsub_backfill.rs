//! Refresh current channel state after an EventSub reconnect. The API can
//! recover active polls, predictions, and hype trains; it cannot replay
//! transient events that occurred while the connection was down.

use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use crust_core::events::AppEvent;
use crust_core::model::ChannelId;
use crust_twitch::eventsub::{EventSubEvent, EventSubNotice, EventSubNoticeKind};
use crust_twitch::helix::{AuthedHelix, HelixApi, HelixHypeTrain, HelixPoll, HelixPrediction};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::runtime::profiles::fetch_twitch_user_profile;

/// Everything `run_backfill` needs to dispatch Helix refresh tasks.
///
/// Cloned + moved into each spawned per-channel task, so every field must
/// be cheap to clone (Arc / mpsc::Sender / String).
pub(crate) struct BackfillContext {
    pub evt_tx: mpsc::Sender<AppEvent>,
    pub eventsub_evt_tx: mpsc::Sender<EventSubEvent>,
    pub helix: Arc<AuthedHelix>,
    pub oauth_token: String,
    pub helix_client_id: Option<String>,
    pub auth_user_id: Option<String>,
    pub channel_room_ids: HashMap<ChannelId, String>,
    pub channel_mod_status: HashMap<ChannelId, bool>,
}

/// Dispatch state refreshes across per-broadcaster tokio tasks. Returns
/// immediately after spawning; individual tasks finish asynchronously.
pub(crate) fn run_backfill(ctx: BackfillContext) {
    let auth_changes = ctx.helix.subscribe_auth_changes();
    debug!(
        "EventSub backfill: {} known channels",
        ctx.channel_room_ids.len()
    );

    // Refresh stream status through the existing profile path.
    for (channel, _room_id) in ctx.channel_room_ids.iter().filter(|(ch, _)| ch.is_twitch()) {
        let login = channel.display_name().to_owned();
        let etx = ctx.evt_tx.clone();
        let token = ctx.oauth_token.clone();
        let client_id = ctx.helix_client_id.clone();
        spawn_backfill_job(auth_changes.clone(), async move {
            fetch_twitch_user_profile(&login, Some(token.as_str()), client_id.as_deref(), etx)
                .await;
        });
    }

    // Unban-queue backfill for moderated Twitch channels. Broadcasters
    // count as moderators of their own channel, mirroring main.rs.
    for (channel, broadcaster_id) in ctx.channel_room_ids.iter().filter(|(ch, _)| ch.is_twitch()) {
        let is_mod = ctx
            .channel_mod_status
            .get(channel)
            .copied()
            .unwrap_or(false)
            || ctx.auth_user_id.as_deref() == Some(broadcaster_id.as_str());
        if !is_mod {
            continue;
        }
        let channel_clone = channel.clone();
        let broadcaster_id_clone = broadcaster_id.clone();
        let moderator_id = ctx.auth_user_id.clone();
        let token = ctx.oauth_token.clone();
        let client_id = ctx.helix_client_id.clone();
        let etx = ctx.evt_tx.clone();
        spawn_backfill_job(auth_changes.clone(), async move {
            crate::helix_fetch_unban_requests(
                &token,
                client_id.as_deref(),
                Some(broadcaster_id_clone.as_str()),
                moderator_id.as_deref(),
                &channel_clone,
                etx,
            )
            .await;
        });
    }

    // These endpoints require the broadcaster's own token. Moderator access
    // to another channel does not grant access to its polls or predictions.
    for (channel, broadcaster_id) in ctx.channel_room_ids.iter().filter(|(channel, id)| {
        channel.is_twitch() && ctx.auth_user_id.as_deref() == Some(id.as_str())
    }) {
        let bid = broadcaster_id.clone();
        let helix = Arc::clone(&ctx.helix);
        let esub_tx = ctx.eventsub_evt_tx.clone();
        let evt_tx = ctx.evt_tx.clone();
        let channel = channel.clone();
        let broadcaster_login = Some(channel.display_name().to_owned());
        spawn_backfill_job(auth_changes.clone(), async move {
            backfill_stateful_topics(helix, channel, bid, broadcaster_login, esub_tx, evt_tx).await;
        });
    }
}

fn spawn_backfill_job(
    mut auth_changes: tokio::sync::watch::Receiver<()>,
    job: impl std::future::Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = auth_changes.changed() => {}
            _ = job => {}
        }
    })
}

async fn backfill_stateful_topics(
    helix: Arc<dyn HelixApi>,
    channel: ChannelId,
    broadcaster_id: String,
    broadcaster_login: Option<String>,
    esub_tx: mpsc::Sender<EventSubEvent>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    match helix.get_active_poll(&broadcaster_id).await {
        Ok(Some(poll)) => {
            if let Some(notice) = poll_to_notice(&broadcaster_id, &broadcaster_login, &poll) {
                let _ = esub_tx.send(EventSubEvent::Notice(notice)).await;
            }
        }
        Ok(None) => {}
        Err(err) => {
            debug!("EventSub backfill: active poll fetch failed for {broadcaster_id}: {err}")
        }
    }

    match helix.get_active_prediction(&broadcaster_id).await {
        Ok(Some(prediction)) => {
            if let Some(notice) =
                prediction_to_notice(&broadcaster_id, &broadcaster_login, &prediction)
            {
                let _ = esub_tx.send(EventSubEvent::Notice(notice)).await;
            }
        }
        Ok(None) => {}
        Err(err) => {
            debug!("EventSub backfill: active prediction fetch failed for {broadcaster_id}: {err}")
        }
    }

    let checked_at = Instant::now();
    match helix.get_active_hype_train(&broadcaster_id).await {
        Ok(Some(train)) => {
            if let Some(notice) = hype_train_to_notice(&broadcaster_id, &broadcaster_login, &train)
            {
                let _ = esub_tx.send(EventSubEvent::Notice(notice)).await;
            } else if train
                .expires_at
                .as_deref()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|expires| expires <= chrono::Utc::now())
            {
                let _ = evt_tx
                    .send(AppEvent::HypeTrainCleared {
                        channel,
                        checked_at,
                    })
                    .await;
            }
        }
        Ok(None) => {
            let _ = evt_tx
                .send(AppEvent::HypeTrainCleared {
                    channel,
                    checked_at,
                })
                .await;
        }
        Err(err) => warn!("EventSub backfill: hype train fetch failed for {broadcaster_id}: {err}"),
    }
}

/// Build a synthetic poll notice. Its ID deduplicates unchanged backfills;
/// WebSocket notifications have independent event IDs.
pub(crate) fn poll_to_notice(
    broadcaster_id: &str,
    broadcaster_login: &Option<String>,
    poll: &HelixPoll,
) -> Option<EventSubNotice> {
    if !poll.status.eq_ignore_ascii_case("ACTIVE") {
        return None;
    }
    Some(with_snapshot_id(
        "poll",
        &poll.id,
        EventSubNotice {
            event_id: None,
            broadcaster_id: broadcaster_id.to_owned(),
            broadcaster_login: broadcaster_login.clone(),
            kind: EventSubNoticeKind::PollLifecycle {
                title: poll.title.clone(),
                phase: "progress".to_owned(),
                status: Some(poll.status.clone()),
                details: summarise_poll(poll),
            },
        },
    ))
}

fn summarise_poll(poll: &HelixPoll) -> Option<String> {
    if poll.choices.is_empty() {
        return None;
    }
    let mut rows: Vec<(String, u64)> = poll
        .choices
        .iter()
        .map(|c| {
            let votes = if c.votes > 0 {
                c.votes
            } else {
                c.channel_points_votes.saturating_add(c.bits_votes)
            };
            (c.title.clone(), votes)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total: u64 = rows.iter().map(|(_, v)| *v).sum();
    let summary = rows
        .into_iter()
        .take(3)
        .map(|(title, votes)| {
            if total > 0 {
                let pct = ((votes as f64 / total as f64) * 100.0).round() as u64;
                format!("{title} {pct}% ({votes})")
            } else {
                format!("{title} ({votes})")
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    (!summary.is_empty()).then(|| format!("Top: {summary}"))
}

pub(crate) fn prediction_to_notice(
    broadcaster_id: &str,
    broadcaster_login: &Option<String>,
    prediction: &HelixPrediction,
) -> Option<EventSubNotice> {
    let normalized = prediction.status.to_ascii_uppercase();
    if normalized != "ACTIVE" && normalized != "LOCKED" {
        return None;
    }
    let phase = if normalized == "LOCKED" {
        "lock"
    } else {
        "progress"
    };
    Some(with_snapshot_id(
        "prediction",
        &prediction.id,
        EventSubNotice {
            event_id: None,
            broadcaster_id: broadcaster_id.to_owned(),
            broadcaster_login: broadcaster_login.clone(),
            kind: EventSubNoticeKind::PredictionLifecycle {
                title: prediction.title.clone(),
                phase: phase.to_owned(),
                status: Some(prediction.status.clone()),
                details: summarise_prediction(prediction),
            },
        },
    ))
}

fn summarise_prediction(prediction: &HelixPrediction) -> Option<String> {
    if prediction.outcomes.is_empty() {
        return None;
    }
    let mut rows: Vec<(String, u64, u64, bool)> = prediction
        .outcomes
        .iter()
        .map(|o| {
            let is_winner = prediction
                .winning_outcome_id
                .as_deref()
                .map(|wid| wid == o.id)
                .unwrap_or(false);
            (o.title.clone(), o.channel_points, o.users, is_winner)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total: u64 = rows.iter().map(|(_, p, _, _)| *p).sum();
    let summary = rows
        .into_iter()
        .take(3)
        .map(|(title, points, users, is_winner)| {
            let winner = if is_winner { " [winner]" } else { "" };
            if total > 0 {
                let pct = ((points as f64 / total as f64) * 100.0).round() as u64;
                format!("{title} {pct}% ({points} pts, {users} users){winner}")
            } else {
                format!("{title} ({points} pts, {users} users){winner}")
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    (!summary.is_empty()).then(|| format!("Top: {summary}"))
}

pub(crate) fn hype_train_to_notice(
    broadcaster_id: &str,
    broadcaster_login: &Option<String>,
    train: &HelixHypeTrain,
) -> Option<EventSubNotice> {
    hype_train_to_notice_at(broadcaster_id, broadcaster_login, train, chrono::Utc::now())
}

fn hype_train_to_notice_at(
    broadcaster_id: &str,
    broadcaster_login: &Option<String>,
    train: &HelixHypeTrain,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<EventSubNotice> {
    let expires_at = train.expires_at.as_deref()?;
    let expiry = chrono::DateTime::parse_from_rfc3339(expires_at).ok()?;
    if expiry <= now {
        return None;
    }
    let ends_at = Some(expires_at.to_owned());

    let top = train.top_contributions.iter().max_by_key(|c| c.total);
    let top_contribution_login = top
        .and_then(|c| pick(c.user_login.as_deref(), c.user_name.as_deref()))
        .map(str::to_owned);
    let top_contribution_type = top.and_then(|c| c.contribution_type.clone());
    let top_contribution_total = top.map(|c| c.total);

    let last_contribution_login = train
        .last_contribution
        .as_ref()
        .and_then(|c| pick(c.user_login.as_deref(), c.user_name.as_deref()).map(str::to_owned));

    Some(with_snapshot_id(
        "hype_train",
        &train.id,
        EventSubNotice {
            event_id: None,
            broadcaster_id: broadcaster_id.to_owned(),
            broadcaster_login: broadcaster_login.clone(),
            kind: EventSubNoticeKind::HypeTrainLifecycle {
                train_id: train.id.clone(),
                phase: "progress".to_owned(),
                level: train.level,
                progress: train.progress,
                goal: train.goal,
                total: train.total,
                last_contribution_login,
                top_contribution_login,
                top_contribution_type,
                top_contribution_total,
                started_at: train.started_at.clone(),
                ends_at,
            },
        },
    ))
}

fn with_snapshot_id(topic: &str, object_id: &str, mut notice: EventSubNotice) -> EventSubNotice {
    // A reconnect can observe a new phase or vote total for the same object.
    // Hash the displayed state so only an unchanged snapshot is suppressed.
    let mut hash = DefaultHasher::new();
    format!("{:?}", notice.kind).hash(&mut hash);
    notice.event_id = Some(format!(
        "backfill:{topic}:{}:{object_id}:{:016x}",
        notice.broadcaster_id,
        hash.finish(),
    ));
    notice
}

fn pick<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.filter(|s| !s.trim().is_empty())
        .or_else(|| b.filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_twitch::helix::{
        FollowedChannel, HelixError, HelixPollChoice, HelixPredictionOutcome, HelixStream,
    };

    struct HypeResponse(Result<Option<HelixHypeTrain>, HelixError>);

    #[async_trait::async_trait]
    impl HelixApi for HypeResponse {
        async fn get_followed(&self, _: &str) -> Result<Vec<FollowedChannel>, HelixError> {
            unreachable!("state refresh does not load follows")
        }

        async fn get_streams(&self, _: &[String]) -> Result<Vec<HelixStream>, HelixError> {
            unreachable!("state refresh does not load streams")
        }

        async fn get_active_hype_train(
            &self,
            _: &str,
        ) -> Result<Option<HelixHypeTrain>, HelixError> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn absent_hype_state_clears_a_banner_but_failed_fetch_does_not() {
        for success in [true, false] {
            let result = if success {
                Ok(None)
            } else {
                Err(HelixError::RateLimited)
            };
            let (evt_tx, mut evt_rx) = mpsc::channel(4);
            let (esub_tx, _esub_rx) = mpsc::channel(4);
            let started = Instant::now();
            backfill_stateful_topics(
                Arc::new(HypeResponse(result)),
                ChannelId::new("alice"),
                "123".to_owned(),
                Some("alice".to_owned()),
                esub_tx,
                evt_tx,
            )
            .await;
            match evt_rx.try_recv() {
                Ok(AppEvent::HypeTrainCleared {
                    channel,
                    checked_at,
                }) => {
                    assert!(success);
                    assert_eq!(channel, ChannelId::new("alice"));
                    assert!(checked_at >= started && checked_at <= Instant::now());
                }
                Err(_) => assert!(!success),
                other => panic!("unexpected refresh event: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn account_changes_cancel_old_backfill_publication() {
        for switch_account in [false, true] {
            let helix = AuthedHelix::new();
            helix
                .set_auth("old-token".to_owned(), "client".to_owned())
                .await;
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let (evt_tx, mut evt_rx) = mpsc::channel(1);
            let job = spawn_backfill_job(helix.subscribe_auth_changes(), async move {
                started_tx.send(()).unwrap();
                let _ = release_rx.await;
                let _ = evt_tx
                    .send(AppEvent::UnbanRequestsLoaded {
                        channel: ChannelId::new("alice"),
                        requests: Vec::new(),
                    })
                    .await;
            });
            started_rx.await.unwrap();
            if switch_account {
                helix
                    .set_auth("new-token".to_owned(), "client".to_owned())
                    .await;
            } else {
                helix.clear_auth().await;
            }
            let _ = release_tx.send(());
            tokio::time::timeout(std::time::Duration::from_secs(1), job)
                .await
                .unwrap()
                .unwrap();
            assert!(
                evt_rx.try_recv().is_err(),
                "old account queue must not be published"
            );
        }
    }

    #[test]
    fn changed_snapshots_survive_deduplication_within_the_reconnect_window() {
        use crate::runtime::eventsub_notices::should_drop_duplicate_eventsub_notice;

        let mut seen = HashMap::new();
        let now = Instant::now();
        let mut gc_at = now;
        let mut dropped = |notice: EventSubNotice| {
            should_drop_duplicate_eventsub_notice(
                &mut seen,
                notice.event_id.as_deref().unwrap(),
                now,
                &mut gc_at,
            )
        };
        let mut poll = test_poll("ACTIVE");
        assert!(!dropped(poll_to_notice("123", &None, &poll).unwrap()));
        assert!(dropped(poll_to_notice("123", &None, &poll).unwrap()));
        poll.choices[0].votes += 1;
        assert!(!dropped(poll_to_notice("123", &None, &poll).unwrap()));

        let mut prediction = HelixPrediction {
            id: "same-prediction".into(),
            title: "Will the run finish?".into(),
            status: "ACTIVE".into(),
            outcomes: Vec::new(),
            winning_outcome_id: None,
        };
        assert!(!dropped(
            prediction_to_notice("123", &None, &prediction).unwrap()
        ));
        prediction.status = "LOCKED".into();
        assert!(!dropped(
            prediction_to_notice("123", &None, &prediction).unwrap()
        ));
        assert!(dropped(
            prediction_to_notice("123", &None, &prediction).unwrap()
        ));
    }

    fn test_poll(status: &str) -> HelixPoll {
        HelixPoll {
            id: "poll-1".into(),
            title: "Best snack?".into(),
            status: status.into(),
            choices: vec![
                HelixPollChoice {
                    title: "Pizza".into(),
                    votes: 120,
                    channel_points_votes: 0,
                    bits_votes: 0,
                },
                HelixPollChoice {
                    title: "Burgers".into(),
                    votes: 80,
                    channel_points_votes: 0,
                    bits_votes: 0,
                },
            ],
            started_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn poll_to_notice_carries_helix_id_as_event_id_for_dedup() {
        let notice = poll_to_notice("123", &None, &test_poll("ACTIVE")).expect("active poll");
        assert!(notice
            .event_id
            .as_deref()
            .unwrap()
            .starts_with("backfill:poll:123:poll-1:"));
        assert_eq!(notice.broadcaster_id, "123");
    }

    #[test]
    fn poll_to_notice_skips_non_active_polls() {
        assert!(poll_to_notice("123", &None, &test_poll("COMPLETED")).is_none());
        assert!(poll_to_notice("123", &None, &test_poll("TERMINATED")).is_none());
    }

    #[test]
    fn poll_to_notice_emits_progress_phase() {
        let notice = poll_to_notice("123", &None, &test_poll("ACTIVE")).expect("active poll");
        match notice.kind {
            EventSubNoticeKind::PollLifecycle { phase, .. } => assert_eq!(phase, "progress"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn prediction_to_notice_marks_locked_predictions_as_lock_phase() {
        let prediction = HelixPrediction {
            id: "pred-1".into(),
            title: "Will boss die?".into(),
            status: "LOCKED".into(),
            outcomes: vec![HelixPredictionOutcome {
                id: "o1".into(),
                title: "Yes".into(),
                channel_points: 500,
                users: 10,
            }],
            winning_outcome_id: None,
        };
        let notice = prediction_to_notice("123", &None, &prediction).expect("locked prediction");
        match notice.kind {
            EventSubNoticeKind::PredictionLifecycle { phase, .. } => assert_eq!(phase, "lock"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert!(notice
            .event_id
            .as_deref()
            .unwrap()
            .starts_with("backfill:prediction:123:pred-1:"));
    }

    #[test]
    fn prediction_to_notice_skips_resolved_and_canceled_states() {
        for status in ["RESOLVED", "CANCELED"] {
            let prediction = HelixPrediction {
                id: "pred-2".into(),
                title: "done".into(),
                status: status.into(),
                outcomes: Vec::new(),
                winning_outcome_id: None,
            };
            assert!(
                prediction_to_notice("123", &None, &prediction).is_none(),
                "status {status} should be skipped"
            );
        }
    }

    #[test]
    fn hype_train_to_notice_requires_an_active_expiry_timestamp() {
        let train_missing = HelixHypeTrain {
            id: "ht-1".into(),
            level: 3,
            total: 900,
            progress: 400,
            goal: 500,
            started_at: Some("2026-04-22T10:00:00Z".into()),
            expires_at: None,
            cooldown_ends_at: None,
            top_contributions: Vec::new(),
            last_contribution: None,
        };
        assert!(hype_train_to_notice("123", &None, &train_missing).is_none());

        let mut train_with_cooldown = HelixHypeTrain {
            id: "ht-2".into(),
            level: 4,
            total: 1500,
            progress: 500,
            goal: 1000,
            started_at: Some("2026-04-22T10:00:00Z".into()),
            expires_at: None,
            cooldown_ends_at: Some("2026-04-22T11:00:00Z".into()),
            top_contributions: Vec::new(),
            last_contribution: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-22T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(hype_train_to_notice_at("123", &None, &train_with_cooldown, now).is_none());
        train_with_cooldown.expires_at = Some("2026-04-22T10:00:00Z".to_owned());
        assert!(hype_train_to_notice_at("123", &None, &train_with_cooldown, now).is_none());
        train_with_cooldown.expires_at = Some("invalid".to_owned());
        assert!(hype_train_to_notice_at("123", &None, &train_with_cooldown, now).is_none());
        train_with_cooldown.expires_at = Some("2026-04-22T11:00:00Z".to_owned());
        let notice = hype_train_to_notice_at("123", &None, &train_with_cooldown, now)
            .expect("notice emitted");
        assert!(notice
            .event_id
            .as_deref()
            .unwrap()
            .starts_with("backfill:hype_train:123:ht-2:"));
        match notice.kind {
            EventSubNoticeKind::HypeTrainLifecycle {
                train_id,
                phase,
                level,
                ends_at,
                ..
            } => {
                assert_eq!(train_id, "ht-2");
                assert_eq!(phase, "progress");
                assert_eq!(level, 4);
                assert_eq!(ends_at.as_deref(), Some("2026-04-22T11:00:00Z"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }
}
