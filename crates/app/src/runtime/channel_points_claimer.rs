//! Channel-points balance poller.
//!
//! Polls the unofficial GQL endpoint for the viewer's channel-points context
//! on every joined Twitch channel and emits balance updates so the split
//! header can show the running total. The actual bonus-point redeem is
//! handled by the embedded webview (DOM click), so this task no longer
//! invokes the `claim_community_points` mutation.
//!
//! No dependency on EventSub - Twitch only exposes channel-points redemption
//! topics to broadcasters, so viewers have to poll. The unofficial GQL
//! endpoint is unsupported; failures are logged once per channel and skipped
//! until the next tick. Nothing else in the app depends on this task.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crust_core::{events::AppEvent, model::ChannelId};
use crust_twitch::gql::{
    fetch_channel_points_context, ChannelPointsContext, ChannelPointsMiss, GqlError,
};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, trace, warn};

/// Poll cadence per channel. 30s keeps the latency between the bonus
/// button appearing and the balance update within an acceptable window
/// without hammering the unofficial GQL endpoint.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum ChannelPointsClaimerCommand {
    /// Replace the set of Twitch channels to poll. New entries are picked up
    /// on the next tick; removed entries are dropped silently.
    SetChannels(Vec<(ChannelId, String)>),
    /// Update the OAuth token (chat token, with or without `oauth:` prefix).
    /// Empty string means signed-out - polling continues but nothing useful
    /// will be returned (we just clear cached state).
    SetAuth(String),
}

pub fn spawn_on(
    handle: &tokio::runtime::Handle,
    http: reqwest::Client,
    initial_token: String,
    cmd_rx: mpsc::Receiver<ChannelPointsClaimerCommand>,
    evt_tx: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(run(http, initial_token, cmd_rx, evt_tx))
}

#[derive(Default)]
struct ChannelEntry {
    login: String,
    last_balance: Option<u64>,
    /// True after we've logged a hard failure for this channel; subsequent
    /// failures stay at trace level.
    logged_failure: bool,
}

async fn run(
    http: reqwest::Client,
    mut token: String,
    mut cmd_rx: mpsc::Receiver<ChannelPointsClaimerCommand>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let mut tracked: HashMap<ChannelId, ChannelEntry> = HashMap::new();
    let mut tick = interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // One-shot warn flag for 401: surface exactly once per token so users
    // know to set their session cookie. Reset on token change.
    let mut auth_warned: bool = false;
    // Dedupe other recurring failures (e.g. GQL schema rename) by error
    // signature so a single broken call doesn't spam N warnings per tick.
    let mut seen_error_signatures: HashSet<String> = HashSet::new();
    // True once we've done at least one real poll. The interval's first
    // tick fires at t=0 when `tracked` is still empty, so without this
    // flag we'd wait a full POLL_INTERVAL (60s) before the first fetch
    // even though channels and tokens arrive seconds after startup.
    let mut has_polled_once: bool = false;

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                let mut should_prime = false;
                match cmd {
                    ChannelPointsClaimerCommand::SetChannels(channels) => {
                        let was_empty = tracked.is_empty();
                        apply_channel_update(&mut tracked, channels);
                        // If this is the first time we've been given
                        // channels (e.g. right after auto-join finishes),
                        // prime a poll on the next loop turn rather than
                        // waiting for the 60s interval boundary.
                        if was_empty && !tracked.is_empty() && !has_polled_once {
                            should_prime = true;
                        }
                    }
                    ChannelPointsClaimerCommand::SetAuth(new_token) => {
                        let was_empty = token.trim().is_empty();
                        let changed = new_token != token;
                        token = new_token;
                        if changed {
                            tracked.values_mut().for_each(|e| {
                                e.last_balance = None;
                                e.logged_failure = false;
                            });
                            auth_warned = false;
                            seen_error_signatures.clear();
                        }
                        // Token transitioned from empty -> set: prime an
                        // immediate poll so the pill appears within
                        // seconds of the user pasting their cookie.
                        if was_empty && !token.trim().is_empty() {
                            should_prime = true;
                        }
                    }
                }
                if should_prime && !tracked.is_empty() && !token.trim().is_empty() {
                    run_poll(
                        &http,
                        &token,
                        &mut tracked,
                        &mut auth_warned,
                        &mut seen_error_signatures,
                        &mut has_polled_once,
                        &evt_tx,
                    )
                    .await;
                }
            }
            _ = tick.tick() => {
                if tracked.is_empty() {
                    continue;
                }
                if token.trim().is_empty() {
                    if !auth_warned {
                        warn!(
                            "channel-points: session token is empty - paste your \
                             Twitch `auth-token` cookie into Settings -> External \
                             Tools -> Twitch session token. The chat OAuth token \
                             does NOT work here."
                        );
                        auth_warned = true;
                    }
                    continue;
                }
                run_poll(
                    &http,
                    &token,
                    &mut tracked,
                    &mut auth_warned,
                    &mut seen_error_signatures,
                    &mut has_polled_once,
                    &evt_tx,
                )
                .await;
            }
            else => break,
        }
    }
}

async fn run_poll(
    http: &reqwest::Client,
    token: &str,
    tracked: &mut HashMap<ChannelId, ChannelEntry>,
    auth_warned: &mut bool,
    seen_error_signatures: &mut HashSet<String>,
    has_polled_once: &mut bool,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    debug!(channels = tracked.len(), "channel-points: polling tick");
    *has_polled_once = true;
    poll_once(
        http,
        token,
        tracked,
        auth_warned,
        seen_error_signatures,
        evt_tx,
    )
    .await;
}

fn apply_channel_update(
    tracked: &mut HashMap<ChannelId, ChannelEntry>,
    channels: Vec<(ChannelId, String)>,
) {
    let desired: HashMap<ChannelId, String> = channels.into_iter().collect();
    tracked.retain(|id, _| desired.contains_key(id));
    for (id, login) in desired {
        tracked
            .entry(id)
            .and_modify(|e| e.login = login.clone())
            .or_insert_with(|| ChannelEntry {
                login,
                ..Default::default()
            });
    }
}

async fn poll_once(
    http: &reqwest::Client,
    token: &str,
    tracked: &mut HashMap<ChannelId, ChannelEntry>,
    auth_warned: &mut bool,
    seen_error_signatures: &mut HashSet<String>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let ids: Vec<ChannelId> = tracked.keys().cloned().collect();
    for id in ids {
        let login = match tracked.get(&id) {
            Some(e) => e.login.clone(),
            None => continue,
        };

        match fetch_channel_points_context(http, &login, token).await {
            Ok(Ok(ctx)) => {
                let prev_balance = tracked.get(&id).and_then(|e| e.last_balance);
                let delta = match prev_balance {
                    Some(prev) if ctx.balance >= prev => Some(ctx.balance - prev),
                    Some(prev) => Some(0u64.saturating_sub(prev - ctx.balance)),
                    None => None,
                };
                let first_time = prev_balance.is_none();
                if first_time || delta.map(|d| d != 0).unwrap_or(false)
                    || ctx.available_claim_id.is_some()
                {
                    info!(
                        channel = %id.0,
                        balance = ctx.balance,
                        delta = ?delta,
                        has_claim = ctx.available_claim_id.is_some(),
                        claim_id = ?ctx.available_claim_id,
                        "channel-points poll"
                    );
                } else {
                    debug!(
                        channel = %id.0,
                        balance = ctx.balance,
                        has_claim = false,
                        "channel-points poll (unchanged)"
                    );
                }
                handle_context(&id, ctx, tracked, evt_tx).await;
            }
            Ok(Err(miss)) => log_miss(
                &id,
                miss,
                auth_warned,
                seen_error_signatures,
            ),
            Err(err) => log_failure(
                tracked.get_mut(&id),
                &id,
                "context",
                err,
                auth_warned,
                seen_error_signatures,
            ),
        }
    }
}

fn log_miss(
    id: &ChannelId,
    miss: ChannelPointsMiss,
    auth_warned: &mut bool,
    seen: &mut HashSet<String>,
) {
    match miss {
        ChannelPointsMiss::NotAuthenticated => {
            if !*auth_warned {
                warn!(
                    "channel-points: session token is present but not authenticated \
                     for Twitch (user.channel.self was null). The token under \
                     Settings -> External Tools must be the `auth-token` COOKIE from \
                     twitch.tv (DevTools -> Application -> Cookies), not the chat OAuth \
                     token. Cookies expire - if yours is over a few weeks old, refresh \
                     the page on twitch.tv and copy the new value."
                );
                *auth_warned = true;
            } else {
                trace!(channel = %id.0, "channel-points: self=null (token not authenticated)");
            }
        }
        ChannelPointsMiss::UserNotFound => {
            let key = "user-not-found".to_owned();
            if seen.insert(key) {
                warn!(channel = %id.0, "channel-points: user(login:) returned null");
            }
        }
        ChannelPointsMiss::NoPointsProgram => {
            // Perfectly normal - not every channel has channel points. One
            // global info is enough so the user knows it's not a bug.
            let key = "no-points-program".to_owned();
            if seen.insert(key) {
                tracing::info!(channel = %id.0, "channel-points: channel has no points program");
            }
        }
        ChannelPointsMiss::UnexpectedShape(detail) => {
            let key = format!("unexpected:{detail}");
            if seen.insert(key) {
                warn!(channel = %id.0, "channel-points: unexpected response shape: {detail}");
            }
        }
    }
}

async fn handle_context(
    id: &ChannelId,
    ctx: ChannelPointsContext,
    tracked: &mut HashMap<ChannelId, ChannelEntry>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let entry = match tracked.get_mut(id) {
        Some(e) => e,
        None => return,
    };

    let balance_changed = entry.last_balance != Some(ctx.balance);
    if balance_changed {
        entry.last_balance = Some(ctx.balance);
        let _ = evt_tx
            .send(AppEvent::ChannelPointsBalanceUpdated {
                channel: id.clone(),
                balance: ctx.balance,
            })
            .await;
    }
}

fn log_failure(
    entry: Option<&mut ChannelEntry>,
    id: &ChannelId,
    kind: &str,
    err: GqlError,
    auth_warned: &mut bool,
    seen_error_signatures: &mut HashSet<String>,
) {
    let msg = err.to_string();
    let is_auth = msg.contains("401") || msg.to_ascii_lowercase().contains("unauthorized");
    if is_auth {
        // 401 hits every channel at once (token-wide problem). Surface it
        // exactly once with an actionable hint, then stay quiet until the
        // user updates their session token.
        if !*auth_warned {
            warn!(
                "channel-points gql 401 unauthorized; set the Twitch session token \
                 (auth-token cookie) under Settings -> External Tools. The chat IRC \
                 OAuth token does not authorize gql.twitch.tv."
            );
            *auth_warned = true;
        } else {
            trace!(channel = %id.0, kind, "channel-points gql failed: {msg}");
        }
        if let Some(entry) = entry {
            entry.logged_failure = true;
        }
        return;
    }
    // Other GQL errors (schema rename, integrity-check, rate-limit) repeat
    // identically across every channel each tick. Dedupe by message so a
    // single broken call surfaces exactly one warn, not N.
    let signature = format!("{kind}|{msg}");
    if seen_error_signatures.insert(signature) {
        warn!(channel = %id.0, kind, "channel-points gql failed: {msg}");
    } else {
        trace!(channel = %id.0, kind, "channel-points gql failed: {msg}");
    }
    if let Some(entry) = entry {
        entry.logged_failure = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_channel_update_adds_and_drops_entries() {
        let mut tracked: HashMap<ChannelId, ChannelEntry> = HashMap::new();
        let a = ChannelId::new("aaa");
        let b = ChannelId::new("bbb");

        apply_channel_update(
            &mut tracked,
            vec![(a.clone(), "aaa".into()), (b.clone(), "bbb".into())],
        );
        assert_eq!(tracked.len(), 2);

        apply_channel_update(&mut tracked, vec![(a.clone(), "aaa".into())]);
        assert!(tracked.contains_key(&a));
        assert!(!tracked.contains_key(&b));
    }

    #[tokio::test]
    async fn handle_context_emits_balance_change_only_on_diff() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut tracked: HashMap<ChannelId, ChannelEntry> = HashMap::new();
        let ch = ChannelId::new("ch");
        tracked.insert(
            ch.clone(),
            ChannelEntry {
                login: "ch".into(),
                ..Default::default()
            },
        );

        let ctx = ChannelPointsContext {
            channel_id: "1".into(),
            balance: 100,
            available_claim_id: None,
        };
        handle_context(&ch, ctx.clone(), &mut tracked, &tx).await;
        let evt = rx.recv().await.expect("balance event");
        match evt {
            AppEvent::ChannelPointsBalanceUpdated { balance, .. } => assert_eq!(balance, 100),
            other => panic!("unexpected: {other:?}"),
        }

        // Same balance again: no event.
        handle_context(&ch, ctx, &mut tracked, &tx).await;
        assert!(rx.try_recv().is_err());
    }
}
