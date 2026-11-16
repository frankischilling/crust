use chrono::{DateTime, Utc};
use crust_core::model::{ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};

/// Resolve the conversation for an IRC `/msg` or `/privmsg` echo.
/// Service commands stay out of saved chat history because they can contain credentials.
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
    if body.is_empty() || target.is_empty()
        || target.chars().any(|c| c.is_whitespace() || matches!(c, ':' | ',' | '\0' | '\r' | '\n'))
        || matches!(target.to_ascii_lowercase().as_str(), "nickserv" | "chanserv" | "memoserv" | "operserv" | "hostserv")
    {
        return None;
    }
    let irc_target = source_channel.irc_target()?;
    let echo_ch = if let Some(channel) = target.strip_prefix('#') {
        ChannelId::irc(&irc_target.host, irc_target.port, irc_target.tls, channel)
    } else {
        ChannelId::irc_query(&irc_target.host, irc_target.port, irc_target.tls, target)
    };
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
            original_display_name: None,
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
            suppress_notification: false,
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
        original_text: None,
        raw_text: text,
        spans,
        twitch_emotes: Vec::new(),
        flags,
        reply: None,
        msg_kind: kind,
        shared: None,
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

#[cfg(test)]
mod echo_tests {
    use super::*;

    #[test]
    fn private_echoes_use_queries_without_saving_service_credentials() {
        let source = ChannelId::irc("irc.example", 6697, true, "room");
        let (query, text) = extract_irc_msg_echo("/msg Alice hello", &source).unwrap();
        assert_eq!(query.irc_query_nick(), Some("alice"));
        assert_eq!(text, "hello");
        let (channel, _) = extract_irc_msg_echo("/privmsg ##room :hello", &source).unwrap();
        assert!(!channel.is_irc_query());
        assert_eq!(channel.irc_name(), "##room");
        for command in ["/msg NickServ IDENTIFY secret", "/msg ChanServ IDENTIFY secret", "/msg alice,bob hello", "/msg :alice hello"] {
            assert!(extract_irc_msg_echo(command, &source).is_none());
        }
    }
}

#[cfg(test)]
mod subscription_tests {
    use super::*;
    use crate::runtime::{log_writer::LogWriter, outgoing::ChatSendPublisher};
    use crust_core::{AppEvent, model::TwitchEmotePos};
    use crust_storage::LogStore;

    #[tokio::test]
    async fn subscription_chat_rows_keep_body_and_emotes_after_history_reload() {
        struct DatabaseFile(std::path::PathBuf);
        impl Drop for DatabaseFile {
            fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
        }
        let file = DatabaseFile(std::env::temp_dir().join(format!(
            "crust-subscription-{}-{}.sqlite3", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        )));
        let store = LogStore::with_db_path(file.0.clone()).unwrap();
        let (events, mut received) = tokio::sync::mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events.clone());
        let channel = ChannelId::new("room");
        let mut message = make_system_message(42, channel.clone(), "🦀 Kappa".into(), Utc::now(),
            MsgKind::Sub { display_name: "Alice".into(), months: 2, plan: "Tier 1".into(),
                is_gift: false, sub_msg: "🦀 Kappa".into() });
        message.server_id = Some("subscription-42".into());
        message.sender.login = "alice".into();
        message.sender.display_name = "Alice".into();
        message.sender.user_id = UserId("10".into());
        message.twitch_emotes.push(TwitchEmotePos { id: "25".into(), start: 2, end: 6 });
        message.spans = crust_core::format::tokenize_with_flags(
            &message.raw_text, false, &message.twitch_emotes, &|_| None,
        );
        ChatSendPublisher::default().publish_incoming(message, Some(&writer), events).await;
        assert!(matches!(received.try_recv().unwrap(), AppEvent::MessageReceived { message, .. }
            if matches!(message.msg_kind, MsgKind::Sub { .. }) && !message.flags.is_history));
        writer.shutdown().await.unwrap();
        drop(writer);
        drop(store);

        let reopened = LogStore::with_db_path(file.0.clone()).unwrap();
        let rows = reopened.recent_messages(&channel, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].server_id.as_deref(), Some("subscription-42"));
        assert_eq!(rows[0].sender.login, "alice");
        assert_eq!(rows[0].raw_text, "🦀 Kappa");
        assert_eq!(rows[0].plain_text(), "Alice resubscribed with Tier 1! (2 months)\n🦀 Kappa");
        assert_eq!(rows[0].twitch_emotes[0].start, 2);
        assert_eq!(rows[0].twitch_emotes[0].end, 6);
        assert!(rows[0].spans.iter().any(|span| matches!(span,
            crust_core::model::Span::Emote { id, code, .. } if id == "25" && code == "Kappa")));
        let date = rows[0].timestamp.format("%Y-%m-%d").to_string();
        let text = reopened.read(channel.as_str(), &date).await.unwrap();
        assert!(text.contains("Alice resubscribed with Tier 1! (2 months)\n🦀 Kappa"));
        assert_eq!(text.matches("🦀 Kappa").count(), 1);
    }
}
