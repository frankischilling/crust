//! Identifier ↔ Value/Type bindings for filter expressions evaluated against
//! Crust's [`ChatMessage`].
//!
//! The [`MESSAGE_TYPING_CONTEXT`] static mirrors Chatterino's
//! `MESSAGE_TYPING_CONTEXT` in `Filter.cpp`. Identifiers that Crust doesn't
//! currently track (for example `flags.automod`, `reward.*`) are still
//! declared here so expressions pass the static type check; at runtime they
//! resolve to sensible defaults (`false`, empty string, `-1`).

use std::collections::HashMap;

use once_cell::sync::Lazy;

use crate::filters::types::{Type, Value};
use crate::model::{ChatMessage, MsgKind, Span};

/// Declared types for every identifier supported by [`build_message_context`].
pub static MESSAGE_TYPING_CONTEXT: Lazy<HashMap<String, Type>> = Lazy::new(|| {
    let mut m = HashMap::new();
    // author.*
    m.insert("author.badges".into(), Type::StringList);
    m.insert("author.external_badges".into(), Type::StringList);
    m.insert("author.color".into(), Type::String);
    m.insert("author.name".into(), Type::String);
    m.insert("author.login".into(), Type::String);
    m.insert("author.user_id".into(), Type::String);
    m.insert("author.no_color".into(), Type::Bool);
    m.insert("author.subbed".into(), Type::Bool);
    m.insert("author.subscriber".into(), Type::Bool); // alias for author.subbed
    m.insert("author.sub_length".into(), Type::Int);
    // channel.*
    m.insert("channel.name".into(), Type::String);
    m.insert("channel.live".into(), Type::Bool);
    m.insert("channel.watching".into(), Type::Bool);
    // flags.*
    for k in [
        "flags.action",
        "flags.highlighted",
        "flags.points_redeemed",
        "flags.sub_message",
        "flags.system_message",
        "flags.reward_message",
        "flags.first_message",
        "flags.elevated_message",
        "flags.hype_chat",
        "flags.cheer_message",
        "flags.whisper",
        "flags.reply",
        "flags.automod",
        "flags.restricted",
        "flags.monitored",
        "flags.shared",
        "flags.similar",
        "flags.watch_streak",
        "flags.pinned",
        "flags.self",
        "flags.mention",
        "flags.history",
        "flags.deleted",
    ] {
        m.insert(k.into(), Type::Bool);
    }
    // message.*
    m.insert("message.content".into(), Type::String);
    m.insert("message.length".into(), Type::Int);
    // has.*
    m.insert("has.link".into(), Type::Bool);
    m.insert("has.emote".into(), Type::Bool);
    m.insert("has.mention".into(), Type::Bool);
    // reward.* - tracked lazily; identifiers always exist.
    m.insert("reward.title".into(), Type::String);
    m.insert("reward.cost".into(), Type::Int);
    m.insert("reward.id".into(), Type::String);
    m
});

/// Populate a runtime [`crate::filters::Context`] from a [`ChatMessage`] and
/// channel metadata. All [`MESSAGE_TYPING_CONTEXT`] identifiers are bound.
pub fn build_message_context(
    msg: &ChatMessage,
    channel_display_name: &str,
    channel_live: Option<bool>,
    watching: bool,
) -> crate::filters::types::Context {
    let mut ctx = crate::filters::types::Context::new();

    // -- author.*
    let badges: Vec<Value> = msg
        .sender
        .badges
        .iter()
        .map(|b| Value::Str(b.name.clone()))
        .collect();
    let sub_length = msg
        .sender
        .badges
        .iter()
        .find(|b| b.name == "subscriber" || b.name == "founder")
        .and_then(|b| b.version.parse::<i64>().ok())
        .unwrap_or(0);
    let subbed = msg
        .sender
        .badges
        .iter()
        .any(|b| b.name == "subscriber" || b.name == "founder");
    let color = msg.sender.color.clone().unwrap_or_default();
    let no_color = msg.sender.color.is_none();
    ctx.insert("author.badges".into(), Value::List(badges));
    ctx.insert("author.external_badges".into(), Value::List(Vec::new()));
    ctx.insert("author.color".into(), Value::Str(color));
    ctx.insert(
        "author.name".into(),
        Value::Str(msg.sender.display_name.clone()),
    );
    ctx.insert("author.login".into(), Value::Str(msg.sender.login.clone()));
    ctx.insert(
        "author.user_id".into(),
        Value::Str(msg.sender.user_id.0.clone()),
    );
    ctx.insert("author.no_color".into(), Value::Bool(no_color));
    ctx.insert("author.subbed".into(), Value::Bool(subbed));
    ctx.insert("author.subscriber".into(), Value::Bool(subbed));
    ctx.insert("author.sub_length".into(), Value::Int(sub_length));

    // -- channel.*
    ctx.insert(
        "channel.name".into(),
        Value::Str(channel_display_name.to_owned()),
    );
    ctx.insert(
        "channel.live".into(),
        Value::Bool(channel_live.unwrap_or(false)),
    );
    ctx.insert("channel.watching".into(), Value::Bool(watching));

    // -- flags.*
    let f = &msg.flags;
    let is_sub_msg = matches!(msg.msg_kind, MsgKind::Sub { .. });
    let is_system = matches!(
        msg.msg_kind,
        MsgKind::SystemInfo
            | MsgKind::Timeout { .. }
            | MsgKind::Ban { .. }
            | MsgKind::ChatCleared
            | MsgKind::Raid { .. }
    );
    let is_bits = matches!(msg.msg_kind, MsgKind::Bits { .. });
    let is_reward = matches!(msg.msg_kind, MsgKind::ChannelPointsReward { .. })
        || f.custom_reward_id.is_some();
    ctx.insert("flags.action".into(), Value::Bool(f.is_action));
    ctx.insert("flags.highlighted".into(), Value::Bool(f.is_highlighted));
    ctx.insert(
        "flags.points_redeemed".into(),
        Value::Bool(f.custom_reward_id.is_some()),
    );
    ctx.insert("flags.sub_message".into(), Value::Bool(is_sub_msg));
    ctx.insert("flags.system_message".into(), Value::Bool(is_system));
    ctx.insert("flags.reward_message".into(), Value::Bool(is_reward));
    ctx.insert("flags.first_message".into(), Value::Bool(f.is_first_msg));
    // Elevated / hype chat ≈ pinned paid chat in Crust.
    ctx.insert("flags.elevated_message".into(), Value::Bool(f.is_pinned));
    ctx.insert("flags.hype_chat".into(), Value::Bool(f.is_pinned));
    ctx.insert("flags.cheer_message".into(), Value::Bool(is_bits));
    // Whisper messages in Crust are a separate stream; default to false.
    ctx.insert("flags.whisper".into(), Value::Bool(false));
    ctx.insert("flags.reply".into(), Value::Bool(msg.reply.is_some()));
    // Not tracked yet in Crust.
    ctx.insert("flags.automod".into(), Value::Bool(false));
    ctx.insert("flags.restricted".into(), Value::Bool(false));
    ctx.insert("flags.monitored".into(), Value::Bool(false));
    ctx.insert("flags.shared".into(), Value::Bool(false));
    ctx.insert("flags.similar".into(), Value::Bool(false));
    ctx.insert("flags.watch_streak".into(), Value::Bool(false));
    ctx.insert("flags.pinned".into(), Value::Bool(f.is_pinned));
    ctx.insert("flags.self".into(), Value::Bool(f.is_self));
    ctx.insert("flags.mention".into(), Value::Bool(f.is_mention));
    ctx.insert("flags.history".into(), Value::Bool(f.is_history));
    ctx.insert("flags.deleted".into(), Value::Bool(f.is_deleted));

    // -- message.*
    ctx.insert(
        "message.content".into(),
        Value::Str(msg.raw_text.clone()),
    );
    ctx.insert(
        "message.length".into(),
        Value::Int(msg.raw_text.chars().count() as i64),
    );

    // -- has.* (scan spans)
    let mut has_link = false;
    let mut has_emote = false;
    let mut has_mention = false;
    for s in &msg.spans {
        match s {
            Span::Url { .. } => has_link = true,
            Span::Emote { .. } | Span::Emoji { .. } => has_emote = true,
            Span::Mention { .. } => has_mention = true,
            _ => {}
        }
    }
    ctx.insert("has.link".into(), Value::Bool(has_link));
    ctx.insert("has.emote".into(), Value::Bool(has_emote));
    ctx.insert("has.mention".into(), Value::Bool(has_mention));

    // -- reward.*
    if let MsgKind::ChannelPointsReward {
        reward_title,
        cost,
        reward_id,
        ..
    } = &msg.msg_kind
    {
        ctx.insert("reward.title".into(), Value::Str(reward_title.clone()));
        ctx.insert("reward.cost".into(), Value::Int(*cost as i64));
        ctx.insert(
            "reward.id".into(),
            Value::Str(reward_id.clone().unwrap_or_default()),
        );
    } else {
        ctx.insert("reward.title".into(), Value::Str(String::new()));
        ctx.insert("reward.cost".into(), Value::Int(-1));
        ctx.insert(
            "reward.id".into(),
            Value::Str(
                msg.flags
                    .custom_reward_id
                    .clone()
                    .unwrap_or_default(),
            ),
        );
    }

    ctx
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use smallvec::smallvec;

    use crate::filters::{evaluate, parse};
    use crate::model::{Badge, ChannelId, MessageId, MsgKind, Sender, Span, UserId};

    fn make_message(login: &str, text: &str, badges: Vec<Badge>) -> ChatMessage {
        ChatMessage {
            id: MessageId(1),
            server_id: None,
            timestamp: Utc::now(),
            channel: ChannelId("somech".into()),
            sender: Sender {
                user_id: UserId("42".into()),
                login: login.to_string(),
                display_name: login.to_string(),
                color: Some("#abc".into()),
                name_paint: None,
                badges,
            },
            raw_text: text.to_string(),
            spans: smallvec![Span::Text {
                text: text.to_string(),
                is_action: false
            }],
            twitch_emotes: Vec::new(),
            flags: Default::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn ticket_subbed_gg_acceptance() {
        let expr = parse("author.subscriber && message.content contains \"gg\"").unwrap();
        let msg = make_message(
            "alice",
            "gg ez",
            vec![Badge {
                name: "subscriber".into(),
                version: "3".into(),
                url: None,
            }],
        );
        let ctx = build_message_context(&msg, "somech", Some(true), false);
        assert_eq!(evaluate(&expr, &ctx), Value::Bool(true));
    }

    #[test]
    fn non_subscriber_does_not_match() {
        let expr = parse("author.subscriber && message.content contains \"gg\"").unwrap();
        let msg = make_message("alice", "gg ez", Vec::new());
        let ctx = build_message_context(&msg, "somech", Some(true), false);
        assert_eq!(evaluate(&expr, &ctx), Value::Bool(false));
    }

    #[test]
    fn badge_list_is_populated() {
        let msg = make_message(
            "alice",
            "hi",
            vec![
                Badge {
                    name: "moderator".into(),
                    version: "1".into(),
                    url: None,
                },
                Badge {
                    name: "subscriber".into(),
                    version: "12".into(),
                    url: None,
                },
            ],
        );
        let ctx = build_message_context(&msg, "chan", None, false);
        let expr = parse("author.badges contains \"moderator\"").unwrap();
        assert_eq!(evaluate(&expr, &ctx), Value::Bool(true));
        let expr2 = parse("author.sub_length >= 12").unwrap();
        assert_eq!(evaluate(&expr2, &ctx), Value::Bool(true));
    }

    #[test]
    fn channel_name_matches() {
        let msg = make_message("alice", "hi", Vec::new());
        let ctx = build_message_context(&msg, "forsen", None, false);
        let expr = parse("channel.name == \"forsen\"").unwrap();
        assert_eq!(evaluate(&expr, &ctx), Value::Bool(true));
    }

    #[test]
    fn has_link_detection() {
        let mut msg = make_message("alice", "see https://example.com", Vec::new());
        msg.spans = smallvec![
            Span::Text {
                text: "see ".into(),
                is_action: false
            },
            Span::Url {
                text: "https://example.com".into(),
                url: "https://example.com".into(),
            },
        ];
        let ctx = build_message_context(&msg, "chan", None, false);
        let expr = parse("has.link").unwrap();
        assert_eq!(evaluate(&expr, &ctx), Value::Bool(true));
    }
}
