use chrono::Utc;
use crust_core::model::{ChannelId, ChatMessage, ReplyInfo, TwitchEmotePos};
use crust_core::AppEvent;
use crust_storage::LogStore;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

pub(crate) struct ChatSendPublisher {
    tails: std::collections::HashMap<ChannelId, oneshot::Receiver<()>>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}

impl Default for ChatSendPublisher {
    fn default() -> Self {
        Self {
            tails: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(512)),
        }
    }
}

impl ChatSendPublisher {
    pub async fn submit(
        &mut self,
        receipt: oneshot::Receiver<Result<(), String>>,
        draft: OutgoingChat,
        echo: ChatMessage,
        log_store: Option<LogStore>,
        events: mpsc::Sender<AppEvent>,
    ) -> bool {
        // Bound completions that are still waiting for storage or UI capacity.
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            let _ = events.send(draft.rejected("Message was not sent. Too many outgoing messages are waiting. Try again after they finish.".into())).await;
            return false;
        };
        self.spawn_ordered(
            draft.channel.clone(),
            slot,
            publish_chat_result(receipt, draft, echo, log_store, events),
        );
        true
    }

    pub async fn submit_irc(
        &mut self,
        receipt: oneshot::Receiver<Result<String, String>>,
        draft: OutgoingChat,
        echo: Option<ChatMessage>,
        log_store: Option<LogStore>,
        events: mpsc::Sender<AppEvent>,
    ) -> bool {
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            let _ = events.send(draft.rejected("Message was not sent. Too many outgoing messages are waiting. Try again after they finish.".into())).await;
            return false;
        };
        let channel = echo
            .as_ref()
            .map(|message| message.channel.clone())
            .unwrap_or_else(|| draft.channel.clone());
        self.spawn_ordered(channel, slot, async move {
            let event = match receipt.await {
                Ok(Ok(nick)) => {
                    if let Some(mut echo) = echo {
                        echo.sender.user_id.0 = nick.clone();
                        echo.sender.login = nick.to_ascii_lowercase();
                        echo.sender.display_name = nick;
                        Some(saved_chat_echo(echo, log_store).await)
                    } else {
                        None
                    }
                }
                Ok(Err(reason)) => Some(draft.rejected(reason)),
                Err(_) => Some(draft.rejected("The IRC connection changed before the send completed. Check the channel before retrying.".into())),
            };
            if let Some(event) = event {
                let _ = events.send(event).await;
            }
        });
        true
    }

    fn spawn_ordered(
        &mut self,
        channel: ChannelId,
        slot: tokio::sync::OwnedSemaphorePermit,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.tails
            .retain(|_, tail| matches!(tail.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        let (finished, tail) = oneshot::channel();
        let previous = self.tails.insert(channel, tail);
        tokio::spawn(async move {
            let _slot = slot;
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            task.await;
            let _ = finished.send(());
        });
    }
}

/// Keep the submitted draft separate from its display text and wire encoding.
pub(crate) struct OutgoingChat {
    pub channel: ChannelId,
    pub text: String,
    pub reply_to_msg_id: Option<String>,
    pub reply: Option<ReplyInfo>,
}

impl OutgoingChat {
    pub fn rejected(self, reason: String) -> AppEvent {
        AppEvent::MessageSendRejected {
            channel: self.channel,
            text: self.text,
            reply_to_msg_id: self.reply_to_msg_id,
            reply: self.reply,
            reason,
        }
    }
}

pub(crate) fn twitch_message_error(text: &str) -> Option<String> {
    let count = text.chars().count();
    (count > crate::TWITCH_MAX_MESSAGE_CHARS).then(|| {
        format!(
            "Message was not sent. It has {count} characters; Twitch allows up to {}.",
            crate::TWITCH_MAX_MESSAGE_CHARS
        )
    })
}

pub(crate) fn chat_echo_body(text: &str) -> (&str, bool) {
    if let Some(body) = text
        .strip_prefix("\u{0001}ACTION ")
        .and_then(|body| body.strip_suffix('\u{0001}'))
    {
        return (body, true);
    }
    if let Some((command, body)) = text.split_once(char::is_whitespace) {
        if command.eq_ignore_ascii_case("/me") {
            return (body, true);
        }
    }
    (text, false)
}

pub(crate) async fn publish_chat_result(
    receipt: oneshot::Receiver<Result<(), String>>,
    draft: OutgoingChat,
    echo: ChatMessage,
    log_store: Option<LogStore>,
    events: mpsc::Sender<AppEvent>,
) {
    let event = match receipt.await {
        Ok(Ok(())) => saved_chat_echo(echo, log_store).await,
        Ok(Err(reason)) => draft.rejected(format!("Message was not sent. {reason}")),
        Err(_) => draft.rejected(
            "The chat connection changed before the send completed. Check the channel before retrying.".into(),
        ),
    };
    let _ = events.send(event).await;
}

async fn saved_chat_echo(echo: ChatMessage, log_store: Option<LogStore>) -> AppEvent {
    // The socket write completed. Server NOTICE errors can still follow.
    if let Some(store) = log_store {
        let saved_echo = echo.clone();
        let saved = tokio::task::spawn_blocking(move || store.append_message(&saved_echo)).await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!("chat-history: failed to persist sent message: {error}"),
            Err(error) => warn!("chat-history: sent-message writer failed: {error}"),
        }
    }
    AppEvent::MessageReceived {
        channel: echo.channel.clone(),
        message: echo,
    }
}

pub(crate) fn complete_whisper_send(
    status: reqwest::StatusCode,
    body: &str,
    from_login: &str,
    target_login: &str,
    text: &str,
    twitch_emotes: Vec<TwitchEmotePos>,
    log_store: Option<&LogStore>,
) -> AppEvent {
    if !status.is_success() {
        let mut message = crate::helix_error_message(status, body);
        if body.contains("MISSING_REQUIRED_SCOPE")
            || message
                .to_ascii_lowercase()
                .contains("missing required scope")
        {
            message.push_str(" Re-login with a token that includes user:manage:whispers.");
        }
        return AppEvent::Error {
            context: "Whisper".into(),
            message: format!("Could not send whisper to {target_login}: {message}"),
        };
    }

    let timestamp = Utc::now();
    if let Some(store) = log_store {
        if let Err(error) = crate::persist_whisper_message(
            store,
            Some(from_login),
            from_login,
            from_login,
            target_login,
            text,
            &twitch_emotes,
            true,
            timestamp,
        ) {
            warn!("whisper-history: failed to persist sent whisper: {error}");
        }
    }
    AppEvent::WhisperReceived {
        from_login: from_login.to_owned(),
        from_display_name: from_login.to_owned(),
        target_login: target_login.to_owned(),
        text: text.to_owned(),
        twitch_emotes,
        is_self: true,
        timestamp,
        is_history: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::model::{MsgKind, UserId};

    fn pending() -> (OutgoingChat, ChatMessage) {
        let channel = ChannelId::new("room");
        let reply = ReplyInfo {
            parent_msg_id: "parent".into(),
            parent_user_login: "bob".into(),
            parent_display_name: "Bob".into(),
            parent_msg_body: "hello".into(),
        };
        let draft = OutgoingChat {
            channel: channel.clone(),
            text: "/me waves 🦀".into(),
            reply_to_msg_id: Some("parent".into()),
            reply: Some(reply.clone()),
        };
        let mut echo = super::super::system_messages::make_system_message(
            1,
            channel,
            "waves 🦀".into(),
            Utc::now(),
            MsgKind::Chat,
        );
        echo.sender.login = "alice".into();
        echo.sender.user_id = UserId("alice-id".into());
        echo.flags.is_action = true;
        echo.flags.is_self = true;
        echo.reply = Some(reply);
        (draft, echo)
    }

    #[tokio::test]
    async fn send_outcomes_keep_channel_order_without_blocking_other_channels() {
        let mut publisher = ChatSendPublisher::default();
        let (events, mut received) = mpsc::channel(8);
        let mut first_receipt = None;
        for (index, name) in ["room", "room", "other"].into_iter().enumerate() {
            let (receipt, receiver) = oneshot::channel();
            let (mut draft, mut echo) = pending();
            draft.channel = ChannelId::new(name);
            echo.channel = draft.channel.clone();
            echo.id.0 = index as u64;
            publisher
                .submit(receiver, draft, echo, None, events.clone())
                .await;
            if index == 0 {
                first_receipt = Some(receipt);
            } else {
                receipt.send(Ok(())).unwrap();
            }
        }
        let event = received.recv().await.unwrap();
        assert!(
            matches!(event, AppEvent::MessageReceived { channel, message } if channel == ChannelId::new("other") && message.id.0 == 2)
        );
        assert!(received.try_recv().is_err());
        first_receipt.unwrap().send(Ok(())).unwrap();
        for expected_id in [0, 1] {
            assert!(
                matches!(received.recv().await.unwrap(), AppEvent::MessageReceived { channel, message } if channel == ChannelId::new("room") && message.id.0 == expected_id)
            );
        }
    }

    #[tokio::test]
    async fn irc_echo_uses_the_confirmed_nick_target_and_action_after_writing() {
        let mut publisher = ChatSendPublisher::default();
        let (events, mut received) = mpsc::channel(4);
        let (receipt, receiver) = oneshot::channel();
        let (mut draft, mut echo) = pending();
        draft.channel = ChannelId::irc("example.com", 6697, true, "__server__");
        echo.channel = ChannelId::irc("example.com", 6697, true, "target");
        let target = echo.channel.clone();
        let store = LogStore::with_db_path(std::path::PathBuf::from(":memory:")).unwrap();
        assert!(
            publisher
                .submit_irc(receiver, draft, Some(echo), Some(store.clone()), events)
                .await
        );
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        receipt.send(Ok("actual_1".into())).unwrap();
        let AppEvent::MessageReceived { channel, message } = received.recv().await.unwrap() else {
            panic!("expected the completed IRC echo");
        };
        assert_eq!(channel, target);
        assert_eq!(message.channel, target);
        assert_eq!(message.sender.login, "actual_1");
        assert_eq!(message.sender.display_name, "actual_1");
        assert!(message.flags.is_self && message.flags.is_action);
        assert_eq!(message.raw_text, "waves 🦀");
        let saved = store.recent_messages(&target, 10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].sender.login, "actual_1");
    }

    #[tokio::test]
    async fn irc_write_failure_and_cancellation_recover_drafts_without_echoes() {
        let mut publisher = ChatSendPublisher::default();
        let (events, mut received) = mpsc::channel(4);
        for explicit_error in [true, false] {
            let (receipt, receiver) = oneshot::channel();
            let (draft, echo) = pending();
            assert!(
                publisher
                    .submit_irc(receiver, draft, Some(echo), None, events.clone())
                    .await
            );
            if explicit_error {
                receipt.send(Err("write failed".into())).unwrap();
            } else {
                drop(receipt);
            }
            let AppEvent::MessageSendRejected {
                text,
                reply_to_msg_id,
                ..
            } = received.recv().await.unwrap()
            else {
                panic!("failed writes must recover the submitted draft");
            };
            assert_eq!(text, "/me waves 🦀");
            assert_eq!(reply_to_msg_id.as_deref(), Some("parent"));
            assert!(received.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn chat_echo_waits_for_the_write_and_keeps_the_captured_sender() {
        let (receipt, receiver) = oneshot::channel();
        let (events, mut received) = mpsc::channel(4);
        let (draft, echo) = pending();
        let task = tokio::spawn(publish_chat_result(receiver, draft, echo, None, events));
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        receipt.send(Ok(())).unwrap();
        task.await.unwrap();
        match received.try_recv().unwrap() {
            AppEvent::MessageReceived { channel, message } => {
                assert_eq!(channel, ChannelId::new("room"));
                assert_eq!(message.sender.login, "alice");
                assert_eq!(message.sender.user_id.0, "alice-id");
                assert_eq!(message.raw_text, "waves 🦀");
                assert!(message.flags.is_action);
                assert_eq!(message.reply.unwrap().parent_msg_id, "parent");
            }
            event => panic!("unexpected send outcome: {event:?}"),
        }
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn chat_rejection_and_disconnect_restore_the_original_text_and_reply() {
        for rejected in [false, true] {
            let (receipt, receiver) = oneshot::channel();
            let (events, mut received) = mpsc::channel(4);
            let (draft, echo) = pending();
            if rejected {
                receipt.send(Err("queue full".into())).unwrap();
            } else {
                drop(receipt);
            }
            publish_chat_result(receiver, draft, echo, None, events).await;
            match received.try_recv().unwrap() {
                AppEvent::MessageSendRejected {
                    channel,
                    text,
                    reply_to_msg_id,
                    reply,
                    reason,
                } => {
                    assert_eq!(channel, ChannelId::new("room"));
                    assert_eq!(text, "/me waves 🦀");
                    assert_eq!(reply_to_msg_id.as_deref(), Some("parent"));
                    assert_eq!(reply.unwrap().parent_msg_id, "parent");
                    assert!(!reason.is_empty());
                }
                event => panic!("unexpected send outcome: {event:?}"),
            }
            assert!(received.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn successful_own_messages_are_saved_before_ui_highlights_and_survive_restart() {
        struct DatabaseFile(std::path::PathBuf);
        impl Drop for DatabaseFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let file = DatabaseFile(std::env::temp_dir().join(format!(
            "crust-outgoing-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        )));
        let store = LogStore::with_db_path(file.0.clone()).unwrap();
        let (receipt, receiver) = oneshot::channel();
        let (events, mut received) = mpsc::channel(4);
        let (draft, echo) = pending();
        let expected_id = echo.id;
        let expected_timestamp = echo.timestamp;
        let channel = echo.channel.clone();
        receipt.send(Ok(())).unwrap();
        publish_chat_result(receiver, draft, echo, Some(store.clone()), events.clone()).await;
        match received.try_recv().unwrap() {
            AppEvent::MessageReceived { message, .. } => {
                assert_eq!(message.id, expected_id);
                assert_eq!(message.timestamp, expected_timestamp);
                store
                    .mark_highlighted(
                        &channel,
                        None,
                        message.id,
                        message.timestamp.timestamp_millis(),
                    )
                    .unwrap();
            }
            event => panic!("unexpected send outcome: {event:?}"),
        }
        let (receipt, receiver) = oneshot::channel();
        let (draft, mut echo) = pending();
        echo.id.0 += 1;
        receipt.send(Err("rejected".into())).unwrap();
        publish_chat_result(receiver, draft, echo, Some(store.clone()), events).await;
        assert!(matches!(
            received.try_recv().unwrap(),
            AppEvent::MessageSendRejected { .. }
        ));
        drop(store);

        let reopened = LogStore::with_db_path(file.0.clone()).unwrap();
        let messages = reopened.recent_messages(&channel, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, expected_id);
        assert_eq!(messages[0].timestamp, expected_timestamp);
        assert_eq!(messages[0].raw_text, "waves 🦀");
        assert!(
            messages[0].flags.is_self
                && messages[0].flags.is_action
                && messages[0].flags.is_highlighted
        );
        assert_eq!(messages[0].reply.as_ref().unwrap().parent_msg_id, "parent");
    }

    #[test]
    fn action_echoes_use_the_body_without_changing_the_submitted_text() {
        for text in [
            "/me waves 🦀",
            "/ME waves 🦀",
            "\u{0001}ACTION waves 🦀\u{0001}",
        ] {
            assert_eq!(chat_echo_body(text), ("waves 🦀", true));
        }
        for text in ["hello 🦀", "/media clip", "I use /me sometimes"] {
            assert_eq!(chat_echo_body(text), (text, false));
        }
        assert!(twitch_message_error(&"🦀".repeat(500)).is_none());
        assert!(twitch_message_error(&"🦀".repeat(501))
            .unwrap()
            .contains("501"));
    }

    #[test]
    fn failed_whispers_never_enter_sent_history() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let channel = crate::whisper_thread_channel_id("bob");
        let event = complete_whisper_send(
            reqwest::StatusCode::FORBIDDEN,
            "rejected",
            "alice",
            "bob",
            "hello",
            Vec::new(),
            Some(&store),
        );
        assert!(matches!(event, AppEvent::Error { .. }));
        assert!(store.recent_messages(&channel, 10).unwrap().is_empty());
        let event = complete_whisper_send(
            reqwest::StatusCode::NO_CONTENT,
            "",
            "alice",
            "bob",
            "hello",
            Vec::new(),
            Some(&store),
        );
        assert!(matches!(
            event,
            AppEvent::WhisperReceived { is_self: true, .. }
        ));
        let messages = store.recent_messages(&channel, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].raw_text, "hello");
        assert_eq!(messages[0].sender.login, "alice");
    }
}
