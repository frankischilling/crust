use chrono::{DateTime, Utc};
use crust_core::model::{ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};

/// Extract echo info from an IRC `/msg` or `/privmsg` command that targets a
/// channel (e.g. `/msg ##chat hello`). Returns `(target_channel_id, body_text)`
/// so the caller can emit a local echo. Returns `None` for non-channel targets
/// (e.g. NickServ) and non-msg commands.
pub(crate) fn extract_irc_msg_echo(
    text: &str,
    source_channel: &ChannelId,
) -> Option<(ChannelId, String)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let cmd_line = trimmed.trim_start_matches('/').trim_start();
    let (cmd, rest) = cmd_line
        .split_once(char::is_whitespace)
        .map(|(c, r)| (c, r.trim_start()))
        .unwrap_or((cmd_line, ""));
    if !matches!(cmd.to_ascii_lowercase().as_str(), "msg" | "privmsg") {
        return None;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let target = parts.next()?.trim();
    let body = parts.next()?.trim_start();
    // Strip optional leading ':' (IRC protocol format).
    let body = body.strip_prefix(':').unwrap_or(body);
    // Only echo for channel targets (starting with #).
    if !target.starts_with('#') || body.is_empty() {
        return None;
    }
    let irc_target = source_channel.irc_target()?;
    // Strip first '#' for internal ChannelId form (##chat -> #chat).
    let ch_name = target
        .strip_prefix('#')
        .unwrap_or(target)
        .to_ascii_lowercase();
    let echo_ch = ChannelId::irc(&irc_target.host, irc_target.port, irc_target.tls, &ch_name);
    Some((echo_ch, body.to_owned()))
}

/// Construct a system (non-chat) ChatMessage for inline display in a channel.
pub(crate) fn make_system_message(
    id: u64,
    channel: ChannelId,
    text: String,
    timestamp: DateTime<Utc>,
    kind: MsgKind,
) -> ChatMessage {
    make_custom_message(
        id,
        channel,
        text,
        timestamp,
        Sender {
            user_id: UserId(String::new()),
            login: String::new(),
            display_name: String::new(),
            color: None,
            name_paint: None,
            badges: Vec::new(),
        },
        MessageFlags {
            is_action: false,
            is_highlighted: false,
            is_deleted: false,
            is_first_msg: false,
            is_pinned: false,
            is_self: false,
            is_mention: false,
            custom_reward_id: None,
            is_history: false,
        },
        kind,
    )
}

pub(crate) fn make_custom_message(
    id: u64,
    channel: ChannelId,
    text: String,
    timestamp: DateTime<Utc>,
    sender: Sender,
    flags: MessageFlags,
    kind: MsgKind,
) -> ChatMessage {
    use smallvec::smallvec;

    let spans = smallvec![crust_core::model::Span::Text {
        text: text.clone(),
        is_action: false
    }];
    ChatMessage {
        id: MessageId(id),
        server_id: None,
        timestamp,
        channel,
        sender,
        raw_text: text,
        spans,
        twitch_emotes: Vec::new(),
        flags,
        reply: None,
        msg_kind: kind,
    }
}

/// Format a timeout notice for display in chat.
pub(crate) fn format_timeout_text(login: &str, seconds: u32) -> String {
    if seconds < 60 {
        format!("{login} was timed out for {seconds}s.")
    } else if seconds < 3600 {
        format!("{login} was timed out for {}m.", seconds / 60)
    } else {
        format!(
            "{login} was timed out for {}h {}m.",
            seconds / 3600,
            (seconds % 3600) / 60
        )
    }
}

/// Best-effort detection for Twitch pinned-card notices delivered as system text.
/// Twitch can surface current pinned cards as notice text instead of PRIVMSG tags.
pub(crate) fn is_twitch_pinned_notice(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("unpinned") {
        return false;
    }

    lower.contains("pinned by ")
        || lower.contains("pinned a message")
        || (lower.contains("pinned") && lower.contains(" sent at "))
}

/// Build a human-readable sub alert text.
pub(crate) fn build_sub_text(display_name: &str, months: u32, plan: &str, is_gift: bool) -> String {
    if is_gift {
        format!("{display_name} received a gifted {plan} subscription! ({months} months total)")
    } else if months <= 1 {
        format!("{display_name} subscribed with {plan}!")
    } else {
        format!("{display_name} resubscribed with {plan}! ({months} months)")
    }
}
