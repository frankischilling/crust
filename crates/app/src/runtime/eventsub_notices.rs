use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use crust_core::model::MsgKind;
use crust_twitch::eventsub::EventSubNoticeKind;

const NOTICE_DEDUP_WINDOW: Duration = Duration::from_secs(45);
const NOTICE_DEDUP_MAX_ENTRIES: usize = 4096;

pub(crate) fn format_eventsub_notice_text(kind: &EventSubNoticeKind) -> String {
    match kind {
        EventSubNoticeKind::Follow { user_login } => {
            format!("{user_login} followed the channel.")
        }
        EventSubNoticeKind::Subscribe {
            user_login,
            tier,
            is_gift,
        } => {
            if *is_gift {
                format!("{user_login} subscribed with a gifted {tier} sub.")
            } else {
                format!("{user_login} subscribed ({tier}).")
            }
        }
        EventSubNoticeKind::SubscriptionGift {
            gifter_login,
            tier,
            total,
        } => {
            let from = gifter_login
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("An anonymous gifter");
            if let Some(total) = total {
                format!("{from} gifted {total} {tier} subscriptions.")
            } else {
                format!("{from} gifted a {tier} subscription.")
            }
        }
        EventSubNoticeKind::Raid {
            from_login,
            viewers,
        } => {
            format!("Incoming raid from {from_login} with {viewers} viewers.")
        }
        EventSubNoticeKind::ChannelChatUserMessageHold { text, .. } => {
            if text.trim().is_empty() {
                "AutoMod is checking your message.".to_owned()
            } else {
                "AutoMod: Hey! Your message is being checked by mods and has not been sent."
                    .to_owned()
            }
        }
        EventSubNoticeKind::ChannelChatUserMessageUpdate { status, .. } => {
            match status.trim().to_ascii_lowercase().as_str() {
                "approved" => "AutoMod: Mods have accepted your message.".to_owned(),
                "denied" => "AutoMod: Mods have denied your message.".to_owned(),
                "invalid" => "AutoMod: Your message was lost in the void.".to_owned(),
                other if !other.is_empty() => {
                    format!("AutoMod: Message update resolved as {other}.")
                }
                _ => "AutoMod: Message update resolved.".to_owned(),
            }
        }
        EventSubNoticeKind::ChannelPointsRedemption {
            user_login,
            reward_title,
            cost,
            user_input,
            status,
            is_update,
            ..
        } => {
            let mut out = if *is_update {
                if let Some(status) = status.as_deref().filter(|s| !s.trim().is_empty()) {
                    format!("Redemption '{reward_title}' from {user_login} is now {status}.")
                } else {
                    format!("Redemption '{reward_title}' from {user_login} was updated.")
                }
            } else {
                format!("{user_login} redeemed '{reward_title}' ({} points)", cost)
            };

            if !*is_update {
                if let Some(input) = user_input.as_deref().filter(|s| !s.trim().is_empty()) {
                    out.push_str(&format!(": {input}"));
                }
                if let Some(status) = status.as_deref().filter(|s| !s.trim().is_empty()) {
                    out.push_str(&format!(" [{status}]"));
                }
            }
            out
        }
        EventSubNoticeKind::PollLifecycle {
            title,
            phase,
            status,
            details,
        } => {
            let mut out = if let Some(status) = status.as_deref().filter(|s| !s.is_empty()) {
                format!("Poll {phase}: {title} ({status})")
            } else {
                format!("Poll {phase}: {title}")
            };
            if let Some(details) = details.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(" - {details}"));
            }
            out
        }
        EventSubNoticeKind::PredictionLifecycle {
            title,
            phase,
            status,
            details,
        } => {
            let mut out = if let Some(status) = status.as_deref().filter(|s| !s.is_empty()) {
                format!("Prediction {phase}: {title} ({status})")
            } else {
                format!("Prediction {phase}: {title}")
            };
            if let Some(details) = details.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(" - {details}"));
            }
            out
        }
        EventSubNoticeKind::AutoModMessageHold {
            sender_login,
            text,
            reason,
            ..
        } => {
            let mut out = format!("AutoMod held a message from {sender_login}");
            if !text.trim().is_empty() {
                out.push_str(&format!(": {text}"));
            }
            if let Some(reason) = reason.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(" ({reason})"));
            }
            out
        }
        EventSubNoticeKind::AutoModMessageUpdate { message_id, status } => {
            if message_id.trim().is_empty() {
                format!("AutoMod message was resolved as {status}.")
            } else {
                format!("AutoMod message {message_id} was resolved as {status}.")
            }
        }
        EventSubNoticeKind::UnbanRequestCreate {
            user_login, text, ..
        } => {
            if let Some(text) = text.as_deref().filter(|s| !s.trim().is_empty()) {
                format!("Unban request from {user_login}: {text}")
            } else {
                format!("Unban request from {user_login}.")
            }
        }
        EventSubNoticeKind::UnbanRequestResolve { request_id, status } => {
            if request_id.trim().is_empty() {
                format!("Unban request resolved as {status}.")
            } else {
                format!("Unban request {request_id} resolved as {status}.")
            }
        }
        EventSubNoticeKind::ChannelBan {
            user_login,
            reason,
            ends_at,
        } => {
            let mut out = if let Some(ends_at) = ends_at.as_deref().filter(|s| !s.trim().is_empty())
            {
                format!("{user_login} was timed out until {ends_at}.")
            } else {
                format!("{user_login} was banned.")
            };
            if let Some(reason) = reason.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(" Reason: {reason}"));
            }
            out
        }
        EventSubNoticeKind::ChannelUnban { user_login } => {
            format!("{user_login} was unbanned.")
        }
        EventSubNoticeKind::SuspiciousUserMessage {
            low_trust_status,
            ban_evasion_evaluation,
            shared_ban_channel_ids,
            types,
            ..
        } => {
            if low_trust_status.trim().eq_ignore_ascii_case("none")
                || low_trust_status.trim().eq_ignore_ascii_case("monitored")
            {
                String::new()
            } else {
                let mut out = String::from("Suspicious User: Restricted");
                let mut details = Vec::new();
                if types
                    .iter()
                    .any(|ty| ty.eq_ignore_ascii_case("ban_evader_detector"))
                {
                    let evader = match ban_evasion_evaluation
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "likely" => "likely",
                        _ => "possible",
                    };
                    details.push(format!("Detected as {evader} ban evader"));
                }
                if !shared_ban_channel_ids.is_empty() {
                    details.push(format!(
                        "Banned in {} shared channels",
                        shared_ban_channel_ids.len()
                    ));
                }
                if !details.is_empty() {
                    out.push_str(". ");
                    out.push_str(&details.join(". "));
                }
                out
            }
        }
        EventSubNoticeKind::SuspiciousUserUpdate {
            user_name,
            moderator_name,
            low_trust_status,
            ..
        } => match low_trust_status.trim().to_ascii_lowercase().as_str() {
            "restricted" => {
                format!("{moderator_name} added {user_name} as a restricted suspicious chatter.")
            }
            "monitored" => {
                format!("{moderator_name} added {user_name} as a monitored suspicious chatter.")
            }
            "none" => {
                format!("{moderator_name} removed {user_name} from the suspicious user list.")
            }
            other => {
                format!(
                    "{moderator_name} updated suspicious user status for {user_name} to {other}."
                )
            }
        },
        EventSubNoticeKind::ModerationAction {
            moderator_login,
            action,
            target_login,
            target_message_id: _,
            source_channel_login,
        } => {
            let action_label = action.replace('_', " ").replace('.', " ").trim().to_owned();
            let source_suffix = source_channel_login
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| format!(" in #{}", s.trim_start_matches('#')))
                .unwrap_or_default();
            if let Some(target) = target_login.as_deref().filter(|s| !s.trim().is_empty()) {
                format!("{moderator_login} performed {action_label} on {target}{source_suffix}.")
            } else {
                format!("{moderator_login} performed {action_label}{source_suffix}.")
            }
        }
        EventSubNoticeKind::StreamOnline => "Stream is now live.".to_owned(),
        EventSubNoticeKind::StreamOffline => "Stream is now offline.".to_owned(),
        EventSubNoticeKind::UserWhisperMessage {
            from_user_name,
            to_user_name,
            text,
            ..
        } => {
            if text.trim().is_empty() {
                format!("Whisper from {from_user_name} to {to_user_name}.")
            } else {
                format!("Whisper from {from_user_name} to {to_user_name}: {text}")
            }
        }
    }
}

pub(crate) fn eventsub_notice_to_message(kind: &EventSubNoticeKind) -> (MsgKind, String) {
    match kind {
        EventSubNoticeKind::ChannelPointsRedemption {
            user_login,
            reward_title,
            cost,
            reward_id,
            redemption_id,
            user_input,
            status,
            is_update: _,
        } => {
            let text = format_eventsub_notice_text(kind);
            (
                MsgKind::ChannelPointsReward {
                    user_login: user_login.clone(),
                    reward_title: reward_title.clone(),
                    cost: *cost,
                    reward_id: reward_id.clone(),
                    redemption_id: redemption_id.clone(),
                    user_input: user_input.clone(),
                    status: status.clone(),
                },
                text,
            )
        }
        EventSubNoticeKind::ChannelBan {
            user_login,
            reason: _,
            ends_at,
        } => {
            let text = format_eventsub_notice_text(kind);
            if let Some(seconds) = parse_timeout_seconds(ends_at.as_deref()) {
                (
                    MsgKind::Timeout {
                        login: user_login.clone(),
                        seconds,
                    },
                    text,
                )
            } else if ends_at
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                (
                    MsgKind::Timeout {
                        login: user_login.clone(),
                        seconds: 0,
                    },
                    text,
                )
            } else {
                (
                    MsgKind::Ban {
                        login: user_login.clone(),
                    },
                    text,
                )
            }
        }
        _ => (MsgKind::SystemInfo, format_eventsub_notice_text(kind)),
    }
}

pub(crate) fn should_emit_eventsub_notice_message(kind: &EventSubNoticeKind) -> bool {
    !matches!(
        kind,
        // IRC moderation events already emit equivalent ban/unban lines.
        EventSubNoticeKind::ChannelBan { .. }
            | EventSubNoticeKind::ChannelUnban { .. }
            | EventSubNoticeKind::AutoModMessageHold { .. }
            | EventSubNoticeKind::AutoModMessageUpdate { .. }
            | EventSubNoticeKind::ChannelChatUserMessageHold { .. }
            | EventSubNoticeKind::ChannelChatUserMessageUpdate { .. }
            | EventSubNoticeKind::SuspiciousUserMessage { .. }
            | EventSubNoticeKind::SuspiciousUserUpdate { .. }
            | EventSubNoticeKind::UserWhisperMessage { .. }
    )
}

pub(crate) fn should_drop_duplicate_eventsub_notice(
    seen: &mut HashMap<String, Instant>,
    event_id: &str,
    now: Instant,
    gc_at: &mut Instant,
) -> bool {
    let id = event_id.trim();
    if id.is_empty() {
        return false;
    }

    // Trim expired entries periodically or whenever the map grows past the cap.
    if now.duration_since(*gc_at) >= Duration::from_secs(10)
        || seen.len() >= NOTICE_DEDUP_MAX_ENTRIES
    {
        seen.retain(|_, ts| now.duration_since(*ts) <= NOTICE_DEDUP_WINDOW);
        *gc_at = now;
    }

    if let Some(seen_at) = seen.get(id) {
        if now.duration_since(*seen_at) <= NOTICE_DEDUP_WINDOW {
            return true;
        }
    }

    seen.insert(id.to_owned(), now);
    false
}

pub(crate) fn stream_status_is_live_from_notice(kind: &EventSubNoticeKind) -> Option<bool> {
    match kind {
        EventSubNoticeKind::StreamOnline => Some(true),
        EventSubNoticeKind::StreamOffline => Some(false),
        _ => None,
    }
}

fn parse_timeout_seconds(ends_at: Option<&str>) -> Option<u32> {
    let ends_at = ends_at?.trim();
    if ends_at.is_empty() {
        return None;
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(ends_at).ok()?;
    let now = Utc::now();
    let end_utc = parsed.with_timezone(&Utc);
    let remaining = (end_utc - now).num_seconds().max(0);
    u32::try_from(remaining).ok()
}

pub(crate) fn room_state_update_from_moderation_action(
    action: &str,
) -> Option<(
    Option<bool>,
    Option<i32>,
    Option<u32>,
    Option<bool>,
    Option<bool>,
)> {
    let normalized = action.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }

    let mut emote_only = None;
    let mut followers_only = None;
    let mut slow = None;
    let mut subs_only = None;
    let mut r9k = None;

    match normalized.as_str() {
        "emoteonly" => emote_only = Some(true),
        "emoteonlyoff" => emote_only = Some(false),
        "subscribers" => subs_only = Some(true),
        "subscribersoff" => subs_only = Some(false),
        "followers" => followers_only = Some(0),
        "followersoff" => followers_only = Some(-1),
        "slow" => slow = Some(0),
        "slowoff" => slow = Some(0),
        "uniquechat" => r9k = Some(true),
        "uniquechatoff" => r9k = Some(false),
        _ => {
            if let Some(value) = normalized
                .strip_prefix("slow_")
                .and_then(|s| s.strip_suffix('s'))
                .and_then(|s| s.parse::<u32>().ok())
            {
                slow = Some(value);
            } else if let Some(value) = normalized
                .strip_prefix("followers_")
                .and_then(|s| s.strip_suffix('m'))
                .and_then(|s| s.parse::<i32>().ok())
            {
                followers_only = Some(value.max(0));
            }
        }
    }

    if emote_only.is_none()
        && followers_only.is_none()
        && slow.is_none()
        && subs_only.is_none()
        && r9k.is_none()
    {
        None
    } else {
        Some((emote_only, followers_only, slow, subs_only, r9k))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModerationActionEffect {
    ChannelMessagesCleared,
    UserMessagesCleared(String),
    MessageDeleted(String),
}

pub(crate) fn moderation_action_effect_from_notice(
    action: &str,
    target_login: Option<&str>,
    target_message_id: Option<&str>,
) -> Option<ModerationActionEffect> {
    let normalized = action.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }

    match normalized.as_str() {
        "clear" => Some(ModerationActionEffect::ChannelMessagesCleared),
        "delete" => target_message_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| ModerationActionEffect::MessageDeleted(s.to_owned())),
        "ban" | "timeout" => target_login
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| ModerationActionEffect::UserMessagesCleared(s.to_owned())),
        _ if normalized.starts_with("timeout_") => target_login
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| ModerationActionEffect::UserMessagesCleared(s.to_owned())),
        _ => None,
    }
}
