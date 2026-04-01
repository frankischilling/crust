use std::collections::{HashMap, HashSet};

use crust_core::{
    events::AppEvent,
    model::{ChannelId, ChatMessage, MessageId},
};
use crust_emotes::{cache::EmoteCache, providers::EmoteInfo};
use crust_storage::LogStore;
use crust_twitch::{parse_line, parse_privmsg_irc};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::seventv::SevenTvCosmeticUpdate;

use super::badges::{resolve_badge_url, BadgeMap};
use super::emote_loading::prefetch_emote_images;

const WHISPER_HISTORY_CHANNEL_PREFIX: &str = "whisper:";

/// Load locally persisted chat history from SQLite and replay it as
/// `AppEvent::HistoryLoaded` for the channel.
pub(crate) async fn load_local_recent_messages(
    channel: ChannelId,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let channel_for_query = channel.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        log_store.recent_messages(&channel_for_query, LogStore::default_recent_limit())
    })
    .await;

    let mut messages = match loaded {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            warn!(
                "chat-history: local SQLite load failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
        Err(e) => {
            warn!(
                "chat-history: local SQLite task failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
    };

    if messages.is_empty() {
        return;
    }

    for msg in &mut messages {
        msg.id = MessageId(
            crate::HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed),
        );
        msg.flags.is_history = true;
        msg.channel = channel.clone();
    }

    info!(
        "chat-history: loaded {} local SQLite messages for #{}",
        messages.len(),
        channel.display_name()
    );
    let _ = evt_tx
        .send(AppEvent::HistoryLoaded { channel, messages })
        .await;
}

/// Load older locally persisted chat history rows before `before_ts_ms` and
/// replay them as `AppEvent::HistoryLoaded` for incremental backfill.
pub(crate) async fn load_local_older_messages(
    channel: ChannelId,
    before_ts_ms: i64,
    limit: usize,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let channel_for_query = channel.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        log_store.older_messages(&channel_for_query, before_ts_ms, limit)
    })
    .await;

    let mut messages = match loaded {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            warn!(
                "chat-history: local older SQLite load failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
        Err(e) => {
            warn!(
                "chat-history: local older SQLite task failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
    };

    if messages.is_empty() {
        return;
    }

    for msg in &mut messages {
        msg.id = MessageId(
            crate::HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed),
        );
        msg.flags.is_history = true;
        msg.channel = channel.clone();
    }

    info!(
        "chat-history: loaded {} older local SQLite messages for #{}",
        messages.len(),
        channel.display_name()
    );
    let _ = evt_tx
        .send(AppEvent::HistoryLoaded { channel, messages })
        .await;
}

/// Load locally persisted whispers from SQLite and replay them as
/// `AppEvent::WhisperReceived` with `is_history=true`.
pub(crate) async fn load_local_recent_whispers(
    log_store: LogStore,
    self_login: Option<String>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let channels = tokio::task::spawn_blocking({
        let store = log_store.clone();
        move || store.recent_channels_with_prefix(WHISPER_HISTORY_CHANNEL_PREFIX, 250)
    })
    .await;

    let whisper_channels = match channels {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            warn!("whisper-history: failed to list whisper channels: {e}");
            return;
        }
        Err(e) => {
            warn!("whisper-history: channel-list task failed: {e}");
            return;
        }
    };

    if whisper_channels.is_empty() {
        return;
    }

    let self_login = self_login
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_default();

    for channel in whisper_channels {
        let Some(partner_login) = channel
            .strip_prefix(WHISPER_HISTORY_CHANNEL_PREFIX)
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };

        let rows = tokio::task::spawn_blocking({
            let store = log_store.clone();
            let channel_id = ChannelId(channel.clone());
            move || store.recent_messages(&channel_id, 250)
        })
        .await;

        let messages = match rows {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                warn!(
                    "whisper-history: failed loading channel {}: {e}",
                    channel
                );
                continue;
            }
            Err(e) => {
                warn!(
                    "whisper-history: load task failed for channel {}: {e}",
                    channel
                );
                continue;
            }
        };

        for msg in messages {
            let from_login = msg.sender.login.trim().to_ascii_lowercase();
            if from_login.is_empty() {
                continue;
            }
            let target_login = if msg.flags.is_self {
                partner_login.clone()
            } else {
                self_login.clone()
            };
            let _ = evt_tx
                .send(AppEvent::WhisperReceived {
                    from_login,
                    from_display_name: msg.sender.display_name,
                    target_login,
                    text: msg.raw_text,
                    twitch_emotes: msg.twitch_emotes,
                    is_self: msg.flags.is_self,
                    timestamp: msg.timestamp,
                    is_history: true,
                })
                .await;
        }
    }
}

/// Fetch recent messages for a channel and send `AppEvent::HistoryLoaded`.
/// Primary source: recent-messages.robotty.de (covers all channels, correct
/// path uses a hyphen: /recent-messages/).  Fallback: logs.ivr.fi (large
/// channels only, returns objects with a "raw" IRC line field, newest-first).
pub(crate) async fn load_recent_messages(
    channel: &str,
    local_nick: Option<&str>,
    emote_index: &crate::EmoteIndex,
    badge_map: &BadgeMap,
    emote_cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    stv_update_tx: &mpsc::Sender<SevenTvCosmeticUpdate>,
) {
    let ch = channel.trim_start_matches('#');
    let channel_id = crust_core::model::ChannelId::new(ch);

    info!("chat-history: fetching recent messages for #{ch}...");

    // NOTE: the correct path is /recent-messages/ (hyphen), not /recent_messages/.
    let robotty_url =
        format!("https://recent-messages.robotty.de/api/v2/recent-messages/{ch}?limit=800");
    let ivr_url = format!("https://logs.ivr.fi/channel/{ch}?json=1&reverse=true&limit=800");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Try robotty first; it covers all channels (including small ones).
    // Fall back to IVR if robotty fails or returns nothing.
    #[allow(unused_assignments)]
    let mut robotty_err: Option<String> = None;
    let raw_lines: Vec<String> = 'fetch: {
        match client
            .get(&robotty_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.text().await {
                        Ok(text) => {
                            #[derive(serde::Deserialize)]
                            struct RobottyResponse {
                                messages: Vec<String>,
                            }
                            match serde_json::from_str::<RobottyResponse>(&text) {
                                Ok(p) if !p.messages.is_empty() => {
                                    info!(
                                        "chat-history: robotty returned {} raw lines for #{ch}",
                                        p.messages.len()
                                    );
                                    break 'fetch p.messages;
                                }
                                Ok(_) => {
                                    robotty_err = Some("robotty returned 0 messages".to_owned());
                                }
                                Err(e) => {
                                    robotty_err = Some(format!("robotty JSON parse failed: {e}"));
                                }
                            }
                        }
                        Err(e) => {
                            robotty_err = Some(format!("robotty body read failed: {e}"));
                        }
                    }
                } else {
                    robotty_err = Some(format!("robotty HTTP {status}"));
                }
            }
            Err(e) => {
                robotty_err = Some(format!("robotty request failed: {e}"));
            }
        }

        if let Some(ref err) = robotty_err {
            info!("chat-history: {err}, trying IVR fallback for #{ch}");
        }

        // IVR fallback
        match client
            .get(&ivr_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => {
                    #[derive(serde::Deserialize)]
                    struct IvrMsg {
                        raw: String,
                    }
                    #[derive(serde::Deserialize)]
                    struct IvrResp {
                        messages: Vec<IvrMsg>,
                    }
                    match serde_json::from_str::<IvrResp>(&text) {
                        Ok(mut p) if !p.messages.is_empty() => {
                            p.messages.reverse(); // IVR is newest-first
                            info!(
                                "chat-history: IVR returned {} raw lines for #{ch}",
                                p.messages.len()
                            );
                            break 'fetch p.messages.into_iter().map(|m| m.raw).collect();
                        }
                        Ok(_) => {
                            warn!("chat-history: both sources returned 0 messages for #{ch}");
                        }
                        Err(e) => {
                            warn!("chat-history: IVR JSON parse failed for #{ch}: {e}");
                        }
                    }
                    Vec::new()
                }
                Err(e) => {
                    warn!("chat-history: IVR body read failed for #{ch}: {e}");
                    Vec::new()
                }
            },
            Ok(resp) => {
                warn!(
                    "chat-history: both sources failed for #{ch} (IVR HTTP {})",
                    resp.status()
                );
                Vec::new()
            }
            Err(e) => {
                warn!("chat-history: both sources failed for #{ch}: {e}");
                Vec::new()
            }
        }
    };

    if raw_lines.is_empty() {
        info!("chat-history: loaded 0 historical messages for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages: Vec::new(),
            })
            .await;
        return;
    }

    let raw_line_count = raw_lines.len();

    // Snapshot shared state once before the parse loop.
    let emote_snapshot: HashMap<String, EmoteInfo> = {
        let guard = emote_index.read().unwrap();
        guard.clone()
    };
    let badge_snapshot: HashMap<(String, String, String), String> = {
        let bm = badge_map.read().unwrap();
        bm.clone()
    };
    let local_nick_owned = local_nick.map(str::to_owned);
    let channel_scope = ch.to_owned();

    // Move parse + tokenize CPU work off async executors.
    let (messages, image_urls) = tokio::task::spawn_blocking(move || {
        let mut messages: Vec<ChatMessage> = Vec::with_capacity(raw_lines.len());
        // Deduplicate image fetch URLs across all history messages.
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut image_urls: Vec<String> = Vec::new();

        for line in &raw_lines {
            let irc_msg = match parse_line(line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if irc_msg.command != "PRIVMSG" {
                continue;
            }

            let id = crate::HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            let mut msg = match parse_privmsg_irc(&irc_msg, local_nick_owned.as_deref(), id) {
                Some(m) => m,
                None => continue,
            };

            // Tokenize spans
            msg.spans = crust_core::format::tokenize(
                &msg.raw_text,
                msg.flags.is_action,
                &msg.twitch_emotes,
                &|code| {
                    crate::resolve_emote(&emote_snapshot, code).map(|info| {
                        (
                            info.id.clone(),
                            info.code.clone(),
                            info.url_1x.clone(),
                            info.provider.clone(),
                            info.url_4x.clone().or_else(|| info.url_2x.clone()),
                        )
                    })
                },
            );

            // Resolve badge URLs from the snapshot (no lock needed)
            for badge in &mut msg.sender.badges {
                badge.url =
                    resolve_badge_url(&badge_snapshot, &channel_scope, &badge.name, &badge.version);
            }

            // Mention detection
            if let Some(ref nick) = local_nick_owned {
                let nick_lower = nick.to_lowercase();
                let text_lower = msg.raw_text.to_lowercase();
                // @mention or bare username as a whole word
                let has_mention = text_lower.contains(&format!("@{nick_lower}"))
                    || text_lower
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|w| w == nick_lower);
                let is_reply_to_me = msg
                    .reply
                    .as_ref()
                    .map(|r| r.parent_user_login.to_lowercase() == nick_lower)
                    .unwrap_or(false);
                msg.flags.is_mention = has_mention || is_reply_to_me;
            }

            // Collect unique image URLs (emotes, emoji, badges) for batch prefetch
            for span in &msg.spans {
                let url = match span {
                    crust_core::Span::Emote { url, .. } => Some(url.clone()),
                    crust_core::Span::Emoji { url, .. } => Some(url.clone()),
                    _ => None,
                };
                if let Some(u) = url {
                    if seen_urls.insert(u.clone()) {
                        image_urls.push(u);
                    }
                }
            }
            for badge in &msg.sender.badges {
                if let Some(ref u) = badge.url {
                    if seen_urls.insert(u.clone()) {
                        image_urls.push(u.clone());
                    }
                }
            }

            messages.push(msg);
        }
        (messages, image_urls)
    })
    .await
    .unwrap_or_default();

    if messages.is_empty() {
        info!("chat-history: parsed 0 PRIVMSG lines from {raw_line_count} raw lines for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages,
            })
            .await;
        return;
    }

    info!("chat-history: loaded {} messages for #{ch}", messages.len());

    // Collect unique Twitch user-ids from history so 7TV can resolve
    // their cosmetics (paints/badges) retroactively.
    {
        let mut history_user_ids: Vec<String> = messages
            .iter()
            .map(|m| m.sender.user_id.0.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        history_user_ids.sort();
        if !history_user_ids.is_empty() {
            let _ = stv_update_tx
                .send(SevenTvCosmeticUpdate::BatchUserLookup {
                    user_ids: history_user_ids,
                })
                .await;
        }
    }

    // Batch-prefetch all unique image URLs.
    if !image_urls.is_empty() {
        prefetch_emote_images(image_urls, emote_cache, evt_tx);
    }

    let _ = evt_tx
        .send(AppEvent::HistoryLoaded {
            channel: channel_id,
            messages,
        })
        .await;
}
