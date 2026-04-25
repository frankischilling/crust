//! Reconnect-backfill runner.
//!
//! Called by the main select loop when the EventSub websocket session
//! emits `EventSubEvent::Backfill(plan)`. Fans out Helix refreshes for
//! every state-bearing topic covered by acceptance criterion "no events >
//! 1 minute old lost":
//!
//! - Stream status (via `fetch_twitch_user_profile`)
//! - Pending unban queue (moderated channels only)
//! - Active poll / prediction / hype train - converted into synthetic
//!   `EventSubNotice`s and routed back through the session event channel
//!   so they pick up the existing 45s `should_drop_duplicate_eventsub_notice`
//!   window in `main.rs`. If the websocket later replays the same event id
//!   we drop the duplicate cleanly.
//!
//! The runner is deliberately conservative: HTTP failures are swallowed
//! (logged and dropped) so a partial backfill never blocks chat flow.

use std::collections::HashMap;
use std::sync::Arc;

use crust_core::events::AppEvent;
use crust_core::model::ChannelId;
use crust_twitch::eventsub::backfill::BackfillPlan;
use crust_twitch::eventsub::{EventSubEvent, EventSubNotice, EventSubNoticeKind};
use crust_twitch::helix::{HelixApi, HelixHypeTrain, HelixPoll, HelixPrediction};
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
    pub helix: Arc<dyn HelixApi>,
    pub oauth_token: String,
    pub helix_client_id: Option<String>,
    pub auth_user_id: Option<String>,
    pub channel_room_ids: HashMap<ChannelId, String>,
    pub channel_mod_status: HashMap<ChannelId, bool>,
}

/// Dispatch the backfill plan across per-broadcaster tokio tasks. Returns
/// immediately after spawning; individual tasks finish asynchronously.
pub(crate) fn run_backfill(plan: BackfillPlan, ctx: BackfillContext) {
    debug!(
        "EventSub backfill: {} watched channels, disconnect {}s (resumed={})",
        plan.watched.len(),
        plan.disconnect.as_secs(),
        plan.resumed
    );

    // Stream-status backfill via IVR profile refresh - reuses the existing
    // `fetch_twitch_user_profile` path that already fans StreamStatusChanged
    // into the UI. Mirrors the old `BackfillRequested` arm's behaviour.
    for (channel, _room_id) in ctx
        .channel_room_ids
        .iter()
        .filter(|(ch, _)| ch.is_twitch())
    {
        let login = channel.display_name().to_owned();
        let etx = ctx.evt_tx.clone();
        let token = ctx.oauth_token.clone();
        let client_id = ctx.helix_client_id.clone();
        tokio::spawn(async move {
            fetch_twitch_user_profile(&login, Some(token.as_str()), client_id.as_deref(), etx)
                .await;
        });
    }

    // Unban-queue backfill for moderated Twitch channels. Broadcasters
    // count as moderators of their own channel, mirroring main.rs.
    for (channel, broadcaster_id) in ctx
        .channel_room_ids
        .iter()
        .filter(|(ch, _)| ch.is_twitch())
    {
        let is_mod = ctx.channel_mod_status.get(channel).copied().unwrap_or(false)
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
        tokio::spawn(async move {
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

    // Poll / prediction / hype-train synthetic-notice backfill. These
    // three Helix endpoints are per-broadcaster and gated on the current
    // login's scopes; `AuthedHelix` surfaces a `MissingScope` error that
    // we silently log and drop. The plan.watched list only contains
    // broadcasters the session actually subscribed to, so we can spawn
    // unconditionally.
    for snapshot in plan.watched {
        let bid = snapshot.broadcaster_id.clone();
        let helix = Arc::clone(&ctx.helix);
        let esub_tx = ctx.eventsub_evt_tx.clone();
        let broadcaster_login = ctx
            .channel_room_ids
            .iter()
            .find(|(_, room_id)| *room_id == &bid)
            .map(|(ch, _)| ch.display_name().to_owned());
        tokio::spawn(async move {
            backfill_stateful_topics(helix, bid, broadcaster_login, esub_tx).await;
        });
    }
}

async fn backfill_stateful_topics(
    helix: Arc<dyn HelixApi>,
    broadcaster_id: String,
    broadcaster_login: Option<String>,
    esub_tx: mpsc::Sender<EventSubEvent>,
) {
    match helix.get_active_poll(&broadcaster_id).await {
        Ok(Some(poll)) => {
            if let Some(notice) = poll_to_notice(&broadcaster_id, &broadcaster_login, &poll) {
                let _ = esub_tx.send(EventSubEvent::Notice(notice)).await;
            }
        }
        Ok(None) => {}
        Err(err) => debug!(
            "EventSub backfill: active poll fetch failed for {broadcaster_id}: {err}"
        ),
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
        Err(err) => debug!(
            "EventSub backfill: active prediction fetch failed for {broadcaster_id}: {err}"
        ),
    }

    match helix.get_latest_hype_train(&broadcaster_id).await {
        Ok(Some(train)) => {
            if let Some(notice) = hype_train_to_notice(&broadcaster_id, &broadcaster_login, &train)
            {
                let _ = esub_tx.send(EventSubEvent::Notice(notice)).await;
            }
        }
        Ok(None) => {}
        Err(err) => warn!(
            "EventSub backfill: hype train fetch failed for {broadcaster_id}: {err}"
        ),
    }
}

/// Build a synthetic poll notice from a Helix `HelixPoll`. The event_id is
/// the poll id so the 45s dedup window correctly collapses with any real
/// `channel.poll.progress` websocket replay.
pub(crate) fn poll_to_notice(
    broadcaster_id: &str,
    broadcaster_login: &Option<String>,
    poll: &HelixPoll,
) -> Option<EventSubNotice> {
    if !poll.status.eq_ignore_ascii_case("ACTIVE") {
        return None;
    }
    Some(EventSubNotice {
        event_id: Some(format!("backfill:poll:{}", poll.id)),
        broadcaster_id: broadcaster_id.to_owned(),
        broadcaster_login: broadcaster_login.clone(),
        kind: EventSubNoticeKind::PollLifecycle {
            title: poll.title.clone(),
            phase: "progress".to_owned(),
            status: Some(poll.status.clone()),
            details: summarise_poll(poll),
        },
    })
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
    Some(EventSubNotice {
        event_id: Some(format!("backfill:prediction:{}", prediction.id)),
        broadcaster_id: broadcaster_id.to_owned(),
        broadcaster_login: broadcaster_login.clone(),
        kind: EventSubNoticeKind::PredictionLifecycle {
            title: prediction.title.clone(),
            phase: phase.to_owned(),
            status: Some(prediction.status.clone()),
            details: summarise_prediction(prediction),
        },
    })
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
    // Only surface hype trains whose cooldown window is still open - if
    // both `expires_at` and `cooldown_ends_at` are missing / in the past,
    // Helix is returning a historical event we shouldn't chat-spam about.
    let ends_at = train
        .expires_at
        .clone()
        .or_else(|| train.cooldown_ends_at.clone());
    if ends_at.is_none() {
        return None;
    }

    let top = train
        .top_contributions
        .iter()
        .max_by_key(|c| c.total);
    let top_contribution_login = top
        .and_then(|c| pick(c.user_login.as_deref(), c.user_name.as_deref()))
        .map(str::to_owned);
    let top_contribution_type = top.and_then(|c| c.contribution_type.clone());
    let top_contribution_total = top.map(|c| c.total);

    let last_contribution_login = train.last_contribution.as_ref().and_then(|c| {
        pick(c.user_login.as_deref(), c.user_name.as_deref()).map(str::to_owned)
    });

    Some(EventSubNotice {
        event_id: Some(format!("backfill:hype_train:{}", train.id)),
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
    })
}

fn pick<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.filter(|s| !s.trim().is_empty())
        .or_else(|| b.filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_twitch::helix::{HelixPollChoice, HelixPredictionOutcome};

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
        assert_eq!(notice.event_id.as_deref(), Some("backfill:poll:poll-1"));
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
        assert_eq!(
            notice.event_id.as_deref(),
            Some("backfill:prediction:pred-1")
        );
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
    fn hype_train_to_notice_requires_cooldown_or_expiry_timestamp() {
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

        let train_with_cooldown = HelixHypeTrain {
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
        let notice =
            hype_train_to_notice("123", &None, &train_with_cooldown).expect("notice emitted");
        assert_eq!(notice.event_id.as_deref(), Some("backfill:hype_train:ht-2"));
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
