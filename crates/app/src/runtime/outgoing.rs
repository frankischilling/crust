use chrono::Utc;
use crust_core::ignores::{CompiledIgnoredPhrases, CompiledIgnoredUsers};
use crust_core::model::{ChannelId, ChatMessage, Nickname, ReplyInfo, TwitchEmotePos};
use crust_core::AppEvent;
use futures_util::{future::Shared, FutureExt};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::log_writer::LogWriter;

const MAX_PENDING_WHISPER_SENDS: usize = 32;

#[derive(Default)]
pub(crate) struct WhisperSendTasks {
    generation: u64,
    tasks: tokio::task::JoinSet<(u64, AppEvent)>,
}

impl WhisperSendTasks {
    pub(crate) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.tasks.abort_all();
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub(crate) fn spawn(
        &mut self,
        send: impl std::future::Future<Output = AppEvent> + Send + 'static,
    ) -> bool {
        if self.tasks.len() >= MAX_PENDING_WHISPER_SENDS {
            return false;
        }
        let generation = self.generation;
        self.tasks.spawn(async move { (generation, send.await) });
        true
    }

    pub(crate) async fn complete_next(&mut self) -> Option<AppEvent> {
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok((generation, event)) if generation == self.generation => return Some(event),
                Err(error) if !error.is_cancelled() => warn!("whisper-send: task failed: {error}"),
                _ => {}
            }
        }
        None
    }
}

pub(crate) struct ChatSendPublisher {
    tails: std::collections::HashMap<ChannelId, oneshot::Receiver<()>>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    pending_publications:
        std::collections::HashMap<ChannelId, std::collections::VecDeque<PendingChatPublication>>,
}

struct PendingChatPublication {
    receipt: Shared<oneshot::Receiver<Result<(), String>>>,
    published: oneshot::Receiver<()>,
}

impl Default for ChatSendPublisher {
    fn default() -> Self {
        Self {
            tails: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(512)),
            pending_publications: Default::default(),
        }
    }
}

impl ChatSendPublisher {
    pub async fn submit(
        &mut self,
        receipt: oneshot::Receiver<Result<(), String>>,
        draft: OutgoingChat,
        echo: impl Into<Option<ChatMessage>>,
        log_writer: Option<LogWriter>,
        events: mpsc::Sender<AppEvent>,
    ) -> bool {
        let echo = echo.into();
        self.prune_publications();
        // Bound completions that are still waiting for storage or UI capacity.
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            let _ = events.send(draft.rejected("Message was not sent. Too many outgoing messages are waiting. Try again after they finish.".into())).await;
            return false;
        };
        let channel = draft.channel.clone();
        let receipt = receipt.shared();
        let (published, finished) = oneshot::channel();
        self.pending_publications
            .entry(channel.clone())
            .or_default()
            .push_back(PendingChatPublication {
                receipt: receipt.clone(),
                published: finished,
            });
        self.spawn_ordered(channel, slot, async move {
            publish_chat_result(receipt, draft, echo, log_writer, events).await;
            let _ = published.send(());
        });
        true
    }

    pub async fn submit_irc(
        &mut self,
        receipt: oneshot::Receiver<Result<String, String>>,
        draft: OutgoingChat,
        echo: Option<ChatMessage>,
        log_writer: Option<LogWriter>,
        events: mpsc::Sender<AppEvent>,
    ) -> bool {
        self.prune_publications();
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
                        if echo.channel.is_irc_query() {
                            let _ = events.send(AppEvent::ChannelJoined { channel: echo.channel.clone() }).await;
                        }
                        Some(saved_chat_echo(echo, log_writer).await)
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

    /// Publish confirmed self messages after local echoes whose socket writes
    /// have already completed. Later rate-limited sends never block incoming chat.
    pub async fn publish_incoming(
        &mut self,
        message: ChatMessage,
        log_writer: Option<&LogWriter>,
        events: mpsc::Sender<AppEvent>,
    ) {
        if message.flags.is_self {
            loop {
                let ready = self
                    .pending_publications
                    .get_mut(&message.channel)
                    .and_then(|pending| {
                        let _ = pending.front()?.receipt.clone().now_or_never()?;
                        pending.pop_front()
                    });
                let Some(ready) = ready else {
                    break;
                };
                // This receipt and every predecessor have resolved. The fence
                // depends only on the history writer and UI event consumer.
                let _ = ready.published.await;
            }
            self.prune_publications();
        }
        // Local writes reached this same writer before their publication
        // fences completed, so it can reconcile even if the UI closes now.
        if let Some(writer) = log_writer {
            if let Err(error) = writer.append(&message).await {
                warn!("chat-history: failed to queue incoming message: {error}");
            }
        }
        let _ = events
            .send(AppEvent::MessageReceived {
                channel: message.channel.clone(),
                message,
            })
            .await;
    }

    fn prune_publications(&mut self) {
        self.pending_publications.retain(|_, pending| {
            while pending.front_mut().is_some_and(|entry| {
                !matches!(
                    entry.published.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                )
            }) {
                pending.pop_front();
            }
            !pending.is_empty()
        });
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
    pub composer: Option<crust_core::ComposerTarget>,
}

impl OutgoingChat {
    pub fn rejected(self, reason: String) -> AppEvent {
        AppEvent::MessageSendRejected {
            channel: self.channel,
            text: self.text,
            reply_to_msg_id: self.reply_to_msg_id,
            reply: self.reply,
            reason,
            composer: self.composer,
        }
    }
}

/// Apply the same display rules to incoming messages and successful local echoes.
/// Native emote ranges must describe the original body before this runs.
pub(crate) fn prepare_chat_display(
    message: &mut ChatMessage,
    ignored_users: &CompiledIgnoredUsers,
    ignored_phrases: &CompiledIgnoredPhrases,
    local_login: Option<&str>,
    nicknames: &[Nickname],
) -> bool {
    if ignored_users.is_ignored(&message.sender.login) {
        return false;
    }
    let original_text = (message.flags.is_self
        && message.original_text.is_none()
        && !ignored_phrases.is_empty())
        .then(|| message.raw_text.clone());
    let outcome = ignored_phrases.apply_to_message(message);
    if outcome.replaced && message.original_text.is_none() {
        message.original_text = original_text;
    }
    if outcome.blocked {
        return false;
    }
    crust_core::model::apply_sender_nickname(
        nicknames,
        message.channel.as_str(),
        &mut message.sender,
    );
    if let Some(login) = local_login {
        message.flags.is_mention |=
            crust_core::highlight::contains_mention(&message.raw_text, login)
                || message.reply.as_ref().is_some_and(|reply| {
                    reply.parent_user_login.eq_ignore_ascii_case(login)
                })
                || crust_core::highlight::contains_nickname_mention(
                    &message.raw_text,
                    login,
                    message.channel.display_name(),
                    nicknames,
                );
    }
    true
}

/// Keep display context aligned with the parent actually addressed on the wire.
pub(crate) fn canonical_reply(
    reply_to_msg_id: Option<&str>,
    reply: Option<&ReplyInfo>,
) -> Option<ReplyInfo> {
    let parent_msg_id =
        reply_to_msg_id.or_else(|| reply.map(|reply| reply.parent_msg_id.as_str()))?;
    Some(match reply.filter(|reply| reply.parent_msg_id == parent_msg_id) {
        Some(reply) => reply.clone(),
        None => ReplyInfo {
            parent_msg_id: parent_msg_id.to_owned(),
            ..Default::default()
        },
    })
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
    receipt: impl std::future::Future<Output = Result<Result<(), String>, oneshot::error::RecvError>>,
    draft: OutgoingChat,
    echo: impl Into<Option<ChatMessage>>,
    log_writer: Option<LogWriter>,
    events: mpsc::Sender<AppEvent>,
) {
    let echo = echo.into();
    let event = match receipt.await {
        Ok(Ok(())) => match echo {
            Some(echo) => Some(saved_chat_echo(echo, log_writer).await),
            None => None,
        },
        Ok(Err(reason)) => Some(draft.rejected(format!("Message was not sent. {reason}"))),
        Err(_) => Some(draft.rejected(
            "The chat connection changed before the send completed. Check the channel before retrying.".into(),
        )),
    };
    if let Some(event) = event {
        let _ = events.send(event).await;
    }
}

async fn saved_chat_echo(mut echo: ChatMessage, log_writer: Option<LogWriter>) -> AppEvent {
    // The socket write completed. Server NOTICE errors can still follow.
    // A queued draft can predate a chat clear that happened before it was sent.
    // Use the successful publication time until the server supplies its timestamp.
    echo.timestamp = Utc::now();
    if let Some(writer) = log_writer {
        if let Err(error) = writer.append(&echo).await {
            warn!("chat-history: failed to queue sent message: {error}");
        }
    }
    AppEvent::MessageReceived {
        channel: echo.channel.clone(),
        message: echo,
    }
}

pub(crate) fn whisper_send_failure(
    owner_user_id: &str,
    target_login: &str,
    text: &str,
    message: impl Into<String>,
) -> AppEvent {
    AppEvent::WhisperSendFailed {
        owner_user_id: owner_user_id.to_owned(),
        target_login: target_login
            .trim()
            .trim_start_matches('@')
            .to_ascii_lowercase(),
        text: text.to_owned(),
        message: message.into(),
    }
}

pub(crate) async fn complete_whisper_send(
    status: reqwest::StatusCode,
    body: &str,
    from_user_id: &str,
    from_login: &str,
    target_login: &str,
    text: &str,
    twitch_emotes: Vec<TwitchEmotePos>,
    log_writer: Option<&LogWriter>,
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
        return whisper_send_failure(
            from_user_id,
            target_login,
            text,
            format!("Could not send whisper to {target_login}: {message}"),
        );
    }

    let timestamp = Utc::now();
    let event = AppEvent::WhisperReceived {
        owner_user_id: from_user_id.to_owned(),
        from_user_id: from_user_id.to_owned(),
        from_login: from_login.to_owned(),
        from_display_name: from_login.to_owned(),
        target_login: target_login.to_owned(),
        text: text.trim().to_owned(),
        twitch_emotes,
        is_self: true,
        timestamp,
        is_history: false,
    };
    if let Some(writer) = log_writer {
        if let Err(error) = writer
            .whisper(
                from_login.to_owned(),
                from_user_id.to_owned(),
                event.clone(),
            )
            .await
        {
            warn!("whisper-history: failed to queue sent whisper: {error}");
        }
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_storage::LogStore;

    #[tokio::test]
    async fn completed_whisper_outcomes_cannot_cross_account_lifetimes() {
        for old_succeeded in [false, true] {
            let mut sends = WhisperSendTasks::default();
            let (finished, ready) = oneshot::channel();
            assert!(sends.spawn(async move {
                let event = complete_whisper_send(
                    if old_succeeded {
                        reqwest::StatusCode::NO_CONTENT
                    } else {
                        reqwest::StatusCode::FORBIDDEN
                    },
                    "old result",
                    "10",
                    "alice",
                    "friend",
                    "same body",
                    Vec::new(),
                    None,
                )
                .await;
                let _ = finished.send(());
                event
            }));
            ready.await.unwrap();
            tokio::task::yield_now().await;

            // An already-completed result can remain queued while accounts change.
            sends.clear();
            sends.clear();
            let (finish_new, new_result) = oneshot::channel();
            assert!(sends.spawn(async move { new_result.await.unwrap() }));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), sends.complete_next(),)
                    .await
                    .is_err(),
                "an old outcome reached the replacement attempt"
            );

            finish_new
                .send(whisper_send_failure(
                    "10",
                    "friend",
                    "same body",
                    "new result",
                ))
                .unwrap();
            assert!(matches!(sends.complete_next().await,
                Some(AppEvent::WhisperSendFailed { owner_user_id, target_login, text, message })
                    if owner_user_id == "10" && target_login == "friend"
                        && text == "same body" && message == "new result"));
            assert!(!sends.has_pending());
        }
    }

    #[tokio::test]
    async fn whisper_send_work_stays_bounded_and_account_changes_cancel_held_requests() {
        let mut sends = WhisperSendTasks::default();
        for _ in 0..MAX_PENDING_WHISPER_SENDS {
            assert!(sends.spawn(std::future::pending()));
        }
        assert!(!sends.spawn(async { panic!("a rejected send must never start") }));
        sends.clear();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), sends.complete_next(),)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!sends.has_pending());
        assert!(sends.spawn(async { whisper_send_failure("20", "friend", "new", "rejected") }));
        assert!(matches!(sends.complete_next().await,
            Some(AppEvent::WhisperSendFailed { owner_user_id, .. }) if owner_user_id == "20"));
    }
    use crust_core::model::{MsgKind, UserId};

    fn pending() -> (OutgoingChat, ChatMessage) {
        let channel = ChannelId::new("room");
        let reply = ReplyInfo {
            parent_msg_id: "parent".into(),
            parent_user_login: "bob".into(),
            parent_display_name: "Bob".into(),
            parent_msg_body: "hello".into(),
            thread_parent_msg_id: Some("thread-root".into()),
            thread_parent_user_login: Some("author".into()),
        };
        let draft = OutgoingChat {
            channel: channel.clone(),
            text: "/me waves 🦀".into(),
            reply_to_msg_id: Some("parent".into()),
            reply: Some(reply.clone()),
            composer: Some(crust_core::ComposerTarget::Chat { account_id: Some("alice-id".into()), pane_id: 73 }),
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
    async fn rewritten_local_echo_and_changed_confirmation_keep_one_saved_row() {
        use crust_core::ignores::{IgnoredPhrase, IgnoredPhraseAction};

        let rules = |replacement: &str| {
            let mut rule = IgnoredPhrase::new("secret");
            rule.action = IgnoredPhraseAction::Replace;
            rule.replace_with = replacement.into();
            CompiledIgnoredPhrases::new(&[rule])
        };
        let ignored_users = CompiledIgnoredUsers::new(&[]);
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, mut received) = mpsc::channel(8);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(), store.clone(), events.clone(),
        );
        let (mut draft, mut local) = pending();
        draft.text = "/me secret Kappa".into();
        draft.reply_to_msg_id = Some("id-only-parent".into());
        draft.reply = None;
        local.raw_text = chat_echo_body(&draft.text).0.into();
        local.reply = canonical_reply(draft.reply_to_msg_id.as_deref(), draft.reply.as_ref());
        local.twitch_emotes = vec![TwitchEmotePos { id: "25".into(), start: 7, end: 11 }];
        let local_id = local.id;
        let mut server = local.clone();
        server.id.0 += 100;
        server.server_id = Some("confirmed-replaced".into());
        server.timestamp += chrono::Duration::seconds(1);
        server.reply.as_mut().unwrap().parent_user_login = "bob".into();
        server.reply.as_mut().unwrap().parent_msg_body = "server parent body".into();
        assert!(prepare_chat_display(
            &mut local, &ignored_users, &rules("masked"), Some("alice"), &[],
        ));
        assert!(prepare_chat_display(
            &mut server, &ignored_users, &rules("new display"), Some("alice"), &[],
        ));
        assert_eq!(draft.text, "/me secret Kappa");
        let mut publisher = ChatSendPublisher::default();
        let (receipt, ready) = oneshot::channel();
        assert!(publisher.submit(ready, draft, local, Some(writer.clone()), events.clone()).await);
        receipt.send(Ok(())).unwrap();
        publisher.publish_incoming(server, Some(&writer), events).await;

        let mut room = crust_core::model::ChannelState::new(ChannelId::new("room"));
        for _ in 0..2 {
            let AppEvent::MessageReceived { message, .. } = received.try_recv().unwrap() else {
                panic!("expected local echo and socket confirmation");
            };
            if room.absorb_own_echo(&message).is_none() {
                room.push_message(message);
            }
        }
        assert_eq!(room.messages.len(), 1);
        assert_eq!(room.messages[0].id, local_id);
        assert_eq!(room.messages[0].raw_text, "new display Kappa");
        assert_eq!(room.messages[0].confirmation_text(), "secret Kappa");
        assert_eq!(room.messages[0].reply.as_ref().unwrap().parent_user_login, "bob");
        writer.shutdown().await.unwrap();
        let rows = store.recent_messages(&room.id, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, local_id);
        assert_eq!(rows[0].server_id.as_deref(), Some("confirmed-replaced"));
        assert_eq!(rows[0].raw_text, "new display Kappa");
        assert_eq!(rows[0].confirmation_text(), "secret Kappa");
    }

    #[test]
    fn local_display_rules_keep_native_emotes_and_apply_aliases_and_mentions() {
        use crust_core::ignores::{IgnoredPhrase, IgnoredPhraseAction};

        let mut replacement = IgnoredPhrase::new("secret");
        replacement.action = IgnoredPhraseAction::Replace;
        replacement.replace_with = "safe".into();
        let mut highlight = IgnoredPhrase::new("safe");
        highlight.action = IgnoredPhraseAction::HighlightOnly;
        let rules = CompiledIgnoredPhrases::new(&[replacement, highlight]);
        let mut alias = Nickname::new("alice", "Alicia");
        alias.channel = Some("room".into());
        let (_, mut echo) = pending();
        echo.raw_text = "secret 🦀 Kappa @Alicia".into();
        echo.sender.display_name = "Alice".into();
        echo.twitch_emotes = vec![TwitchEmotePos { id: "25".into(), start: 9, end: 13 }];
        assert!(prepare_chat_display(
            &mut echo, &CompiledIgnoredUsers::new(&[]), &rules, Some("alice"), &[alias],
        ));
        assert_eq!(echo.raw_text, "safe 🦀 Kappa @Alicia");
        assert_eq!(echo.confirmation_text(), "secret 🦀 Kappa @Alicia");
        assert_eq!(echo.sender.display_name, "Alicia");
        assert_eq!(echo.sender.original_display_name.as_deref(), Some("Alice"));
        assert!(echo.flags.is_highlighted && echo.flags.is_mention);
        assert_eq!((echo.twitch_emotes[0].start, echo.twitch_emotes[0].end), (7, 11));
        let spans = crust_core::format::tokenize_with_flags(
            &echo.raw_text, echo.flags.is_action, &echo.twitch_emotes, &|_| None,
        );
        assert!(spans.iter().any(|span| matches!(span,
            crust_core::model::Span::Emote { id, code, .. } if id == "25" && code == "Kappa"
        )));
    }

    #[tokio::test]
    async fn blocked_local_echoes_stay_hidden_but_failed_sends_recover_the_draft() {
        use crust_core::ignores::{IgnoredPhrase, IgnoredUser};

        for ignored_user in [false, true] {
            for send_result in [Some(Ok(())), Some(Err("write failed".to_owned())), None] {
                let (mut draft, mut echo) = pending();
                draft.text = "/me secret 🦀".into();
                echo.raw_text = "secret 🦀".into();
                let users = CompiledIgnoredUsers::new(&if ignored_user {
                    vec![IgnoredUser::new("alice")]
                } else {
                    Vec::new()
                });
                let phrases = CompiledIgnoredPhrases::new(&if ignored_user {
                    Vec::new()
                } else {
                    vec![IgnoredPhrase::new("secret")]
                });
                assert!(!prepare_chat_display(&mut echo, &users, &phrases, Some("alice"), &[]));
                let store = LogStore::with_db_path(":memory:".into()).unwrap();
                let (events, mut received) = mpsc::channel(4);
                let writer = LogWriter::new(
                    &tokio::runtime::Handle::current(), store.clone(), events.clone(),
                );
                let (receipt, result) = oneshot::channel();
                let succeeded = matches!(&send_result, Some(Ok(())));
                if let Some(outcome) = send_result {
                    receipt.send(outcome).unwrap();
                } else {
                    drop(receipt);
                }
                publish_chat_result(result, draft, None::<ChatMessage>, Some(writer.clone()), events).await;
                if !succeeded {
                    let AppEvent::MessageSendRejected { text, reply_to_msg_id, composer, .. } =
                        received.try_recv().unwrap()
                    else {
                        panic!("a hidden echo must still recover a rejected send");
                    };
                    assert_eq!(text, "/me secret 🦀");
                    assert_eq!(reply_to_msg_id.as_deref(), Some("parent"));
                    assert_eq!(composer, Some(crust_core::ComposerTarget::Chat {
                        account_id: Some("alice-id".into()), pane_id: 73,
                    }));
                }
                assert!(received.try_recv().is_err());
                writer.shutdown().await.unwrap();
                assert!(store.recent_messages(&echo.channel, 10).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn reply_context_follows_the_wire_parent_without_copying_unrelated_metadata() {
        let (_, echo) = pending();
        let metadata = echo.reply.unwrap();
        assert_eq!(canonical_reply(None, Some(&metadata)), Some(metadata.clone()));
        assert_eq!(canonical_reply(Some("parent"), Some(&metadata)), Some(metadata.clone()));
        let expected = ReplyInfo { parent_msg_id: "different-parent".into(), ..Default::default() };
        assert_eq!(canonical_reply(Some("different-parent"), None), Some(expected.clone()));
        assert_eq!(canonical_reply(Some("different-parent"), Some(&metadata)), Some(expected));
        assert!(canonical_reply(None, None).is_none());
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
    async fn incoming_confirmation_waits_only_for_the_receipt_ready_prefix() {
        let mut publisher = ChatSendPublisher::default();
        let (events, mut received) = mpsc::channel(8);
        let (first_receipt, first_result) = oneshot::channel();
        let (draft, echo) = pending();
        let mut confirmation = echo.clone();
        confirmation.id.0 = 99;
        confirmation.server_id = Some("confirmed".into());
        assert!(
            publisher
                .submit(first_result, draft, echo, None, events.clone())
                .await
        );
        let (later_receipt, later_result) = oneshot::channel();
        let (draft, mut echo) = pending();
        echo.id.0 = 2;
        assert!(
            publisher
                .submit(later_result, draft, echo, None, events.clone())
                .await
        );
        first_receipt.send(Ok(())).unwrap();
        // The current-thread executor has not scheduled either publisher yet.
        assert!(received.try_recv().is_err());
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            publisher.publish_incoming(confirmation, None, events.clone()),
        )
        .await
        .unwrap();
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::MessageReceived { message, .. }
            if message.id.0 == 1 && message.server_id.is_none())
        );
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::MessageReceived { message, .. }
            if message.id.0 == 99 && message.server_id.as_deref() == Some("confirmed"))
        );
        assert!(
            received.try_recv().is_err(),
            "the later socket receipt is still pending"
        );
        later_receipt.send(Ok(())).unwrap();
        assert!(
            matches!(received.recv().await.unwrap(), AppEvent::MessageReceived { message, .. }
            if message.id.0 == 2)
        );
    }

    #[tokio::test]
    async fn incoming_rows_do_not_wait_for_unresolved_writes_or_send_capacity() {
        let mut publisher = ChatSendPublisher::default();
        publisher.slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (events, mut received) = mpsc::channel(8);
        let (receipt, result) = oneshot::channel();
        let (draft, echo) = pending();
        assert!(
            publisher
                .submit(result, draft, echo.clone(), None, events.clone())
                .await
        );
        for (id, is_self) in [(98, false), (99, true)] {
            let mut incoming = echo.clone();
            incoming.id.0 = id;
            incoming.server_id = Some(format!("incoming-{id}"));
            incoming.flags.is_self = is_self;
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                publisher.publish_incoming(incoming, None, events.clone()),
            )
            .await
            .unwrap();
            assert!(
                matches!(received.try_recv().unwrap(), AppEvent::MessageReceived { message, .. }
                if message.id.0 == id)
            );
        }
        drop(receipt);
        assert!(matches!(
            received.recv().await.unwrap(),
            AppEvent::MessageSendRejected { .. }
        ));
    }

    #[tokio::test]
    async fn repeated_same_text_sends_keep_fifo_confirmation_identities() {
        let mut publisher = ChatSendPublisher::default();
        let (events, mut received) = mpsc::channel(8);
        let mut receipts = Vec::new();
        let mut confirmations = Vec::new();
        for (id, server_id) in [(1, "first-confirmed"), (2, "second-confirmed")] {
            let (receipt, result) = oneshot::channel();
            let (draft, mut echo) = pending();
            echo.id.0 = id;
            let mut confirmed = echo.clone();
            confirmed.id.0 += 10;
            confirmed.server_id = Some(server_id.into());
            assert!(
                publisher
                    .submit(result, draft, echo, None, events.clone())
                    .await
            );
            receipts.push(receipt);
            confirmations.push(confirmed);
        }
        for receipt in receipts {
            receipt.send(Ok(())).unwrap();
        }
        for confirmed in confirmations {
            publisher
                .publish_incoming(confirmed, None, events.clone())
                .await;
        }
        let mut room = crust_core::model::ChannelState::new(ChannelId::new("room"));
        let (_, mut old_history) = pending();
        old_history.id.0 = 999;
        old_history.flags.is_history = true;
        room.push_message(old_history);
        for _ in 0..4 {
            let AppEvent::MessageReceived { message, .. } = received.try_recv().unwrap() else {
                panic!("expected a local or confirmed message");
            };
            if room.absorb_own_echo(&message).is_none() {
                room.push_message(message);
            }
        }
        assert_eq!(room.messages.len(), 3);
        assert!(room.messages[0].flags.is_history && room.messages[0].server_id.is_none());
        assert_eq!(room.messages[1].id.0, 1);
        assert_eq!(
            room.messages[1].server_id.as_deref(),
            Some("first-confirmed")
        );
        assert_eq!(room.messages[2].id.0, 2);
        assert_eq!(
            room.messages[2].server_id.as_deref(),
            Some("second-confirmed")
        );
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn confirmation_matching_keeps_actions_and_reply_contexts_separate() {
        let (_, mut action) = pending();
        action.id.0 = 1;
        let mut ordinary = action.clone();
        ordinary.id.0 = 2;
        ordinary.flags.is_action = false;
        ordinary.reply = None;
        let mut reply = ordinary.clone();
        reply.id.0 = 3;
        reply.reply = action.reply.clone();
        let mut room = crust_core::model::ChannelState::new(ChannelId::new("room"));
        for row in [&action, &ordinary, &reply] {
            room.push_message(row.clone());
        }
        for (server_id, row) in [("ordinary", ordinary), ("reply", reply), ("action", action)] {
            let expected_id = row.id;
            let expected_time = row.timestamp;
            let mut confirmed = row;
            confirmed.id.0 += 10;
            confirmed.server_id = Some(server_id.into());
            assert_eq!(
                room.absorb_own_echo(&confirmed),
                Some((expected_id, expected_time))
            );
        }
        assert_eq!(
            room.messages
                .iter()
                .map(|row| row.server_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["action", "ordinary", "reply"]
        );
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
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        assert!(
            publisher
                .submit_irc(receiver, draft, Some(echo), Some(writer.clone()), events)
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
        writer.flush().await.unwrap();
        let saved = store.recent_messages(&target, 10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].sender.login, "actual_1");
        writer.shutdown().await.unwrap();
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
                composer,
                ..
            } = received.recv().await.unwrap()
            else {
                panic!("failed writes must recover the submitted draft");
            };
            assert_eq!(text, "/me waves 🦀");
            assert_eq!(reply_to_msg_id.as_deref(), Some("parent"));
            assert_eq!(composer, Some(crust_core::ComposerTarget::Chat { account_id: Some("alice-id".into()), pane_id: 73 }));
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
        for origin in [
            None,
            Some(crust_core::ComposerTarget::Chat { account_id: Some("alice-id".into()), pane_id: 73 }),
            Some(crust_core::ComposerTarget::ReplyThread { account_id: Some("alice-id".into()), root_msg_id: "thread-root".into() }),
        ] {
            for rejected in [false, true] {
                let (receipt, receiver) = oneshot::channel();
                let (events, mut received) = mpsc::channel(4);
                let (mut draft, echo) = pending();
                draft.composer = origin.clone();
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
                        composer,
                    } => {
                        assert_eq!(channel, ChannelId::new("room"));
                        assert_eq!(text, "/me waves 🦀");
                        assert_eq!(reply_to_msg_id.as_deref(), Some("parent"));
                        assert_eq!(reply.unwrap().parent_msg_id, "parent");
                        assert_eq!(composer, origin);
                        assert!(!reason.is_empty());
                    }
                    event => panic!("unexpected send outcome: {event:?}"),
                }
                assert!(received.try_recv().is_err());
            }
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
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        let (draft, echo) = pending();
        let expected_id = echo.id;
        let submitted_at = echo.timestamp;
        let channel = echo.channel.clone();
        receipt.send(Ok(())).unwrap();
        publish_chat_result(receiver, draft, echo, Some(writer.clone()), events.clone()).await;
        let expected_timestamp = match received.try_recv().unwrap() {
            AppEvent::MessageReceived { message, .. } => {
                assert_eq!(message.id, expected_id);
                assert!(message.timestamp >= submitted_at);
                writer
                    .mark_highlighted(
                        channel.clone(),
                        None,
                        message.id,
                        message.timestamp.timestamp_millis(),
                    )
                    .await
                    .unwrap();
                message.timestamp
            }
            event => panic!("unexpected send outcome: {event:?}"),
        };
        let (receipt, receiver) = oneshot::channel();
        let (draft, mut echo) = pending();
        echo.id.0 += 1;
        receipt.send(Err("rejected".into())).unwrap();
        publish_chat_result(receiver, draft, echo, Some(writer.clone()), events).await;
        assert!(matches!(
            received.try_recv().unwrap(),
            AppEvent::MessageSendRejected { .. }
        ));
        writer.shutdown().await.unwrap();
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

    #[tokio::test]
    async fn closing_before_ui_confirmation_keeps_one_deleted_row_after_restart() {
        struct DatabaseFile(std::path::PathBuf);
        impl Drop for DatabaseFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let file = DatabaseFile(std::env::temp_dir().join(format!(
            "crust-close-confirmation-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        )));
        let store = LogStore::with_db_path(file.0.clone()).unwrap();
        let (events, received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        let mut publisher = ChatSendPublisher::default();
        let (receipt, result) = oneshot::channel();
        let (draft, mut local) = pending();
        local.flags.is_highlighted = true;
        let local_id = local.id;
        let channel = local.channel.clone();
        let mut server = local.clone();
        server.id.0 += 100;
        server.server_id = Some("confirmed-before-close".into());
        server.timestamp += chrono::Duration::seconds(1);
        let server_time = server.timestamp;
        assert!(
            publisher
                .submit(result, draft, local, Some(writer.clone()), events.clone())
                .await
        );
        super::super::log_writer::publish_moderation(
            AppEvent::MessageDeleted {
                channel: channel.clone(),
                server_id: "confirmed-before-close".into(),
            },
            Some(&writer),
            &events,
        )
        .await;
        receipt.send(Ok(())).unwrap();
        publisher
            .publish_incoming(server, Some(&writer), events)
            .await;
        assert_eq!(received.len(), 3);
        // Closing the window discards all queued UI events without applying any.
        drop(received);
        writer.shutdown().await.unwrap();
        drop(writer);
        drop(store);
        let reopened = LogStore::with_db_path(file.0.clone()).unwrap();
        let rows = reopened.recent_messages(&channel, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, local_id);
        assert_eq!(rows[0].server_id.as_deref(), Some("confirmed-before-close"));
        assert_eq!(rows[0].timestamp, server_time);
        assert!(rows[0].flags.is_deleted && rows[0].flags.is_highlighted);
    }

    async fn check_confirmed_echo_chronology(
        state: &mut crust_core::AppState,
        writer: &LogWriter,
        store: &LogStore,
        local: &ChatMessage,
        server_timestamp: chrono::DateTime<Utc>,
        deleted: bool,
    ) {
        let mut server = local.clone();
        server.id.0 += 100;
        server.server_id = Some("confirmed-chronology".into());
        server.timestamp = server_timestamp;
        server.flags.is_deleted = false;
        writer.append(&server).await.unwrap();
        state.apply_message_moderation(&mut server);
        let room = state.channels.get_mut(&local.channel).unwrap();
        assert_eq!(
            room.absorb_own_echo(&server),
            Some((local.id, local.timestamp)),
        );
        let confirmed = room.messages.back().unwrap().clone();
        assert_eq!(room.messages.len(), 1);
        assert_eq!(confirmed.flags.is_deleted, deleted);
        assert!(confirmed.flags.is_highlighted);
        assert_eq!(confirmed.timestamp, server_timestamp);
        state.sync_mention_echo(&confirmed);
        assert_eq!(state.mentions.len(), 1);
        assert_eq!(state.mentions_unread, 1);
        assert_eq!(state.mentions[0].flags.is_deleted, deleted);
        writer
            .update_sender()
            .send(crust_core::events::HistoryUpdate::Confirmation {
                message: Box::new(confirmed),
                original_timestamp_ms: local.timestamp.timestamp_millis(),
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let saved = store.recent_messages(&local.channel, 10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, local.id);
        assert_eq!(saved[0].server_id, server.server_id);
        assert_eq!(saved[0].timestamp, server_timestamp);
        assert_eq!(saved[0].flags.is_deleted, deleted);
        assert!(saved[0].flags.is_highlighted);
    }

    #[tokio::test]
    async fn a_clear_before_socket_success_does_not_delete_the_later_confirmed_send() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, mut received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        let (draft, mut echo) = pending();
        echo.timestamp = Utc::now() - chrono::Duration::minutes(2);
        echo.flags.is_highlighted = true;
        let cutoff = echo.timestamp + chrono::Duration::minutes(1);
        let clear =
            crust_core::model::ModerationAction::channel(echo.channel.clone(), Some(cutoff));
        let mut state = crust_core::AppState::default();
        state.join_channel(echo.channel.clone());
        state.apply_moderation(&clear);
        writer.moderate(clear).await.unwrap();
        let (receipt, result) = oneshot::channel();
        receipt.send(Ok(())).unwrap();
        publish_chat_result(result, draft, echo, Some(writer.clone()), events).await;
        let AppEvent::MessageReceived { message: local, .. } = received.recv().await.unwrap()
        else {
            panic!("expected successful local echo");
        };
        assert!(local.timestamp > cutoff);
        state
            .channels
            .get_mut(&local.channel)
            .unwrap()
            .push_message(local.clone());
        state.push_mention(local.clone(), true);
        check_confirmed_echo_chronology(
            &mut state,
            &writer,
            &store,
            &local,
            local.timestamp + chrono::Duration::milliseconds(1),
            false,
        )
        .await;
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_clear_after_socket_success_stays_deleted_through_confirmation() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, mut received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        let (draft, mut echo) = pending();
        echo.flags.is_highlighted = true;
        let mut state = crust_core::AppState::default();
        state.join_channel(echo.channel.clone());
        let (receipt, result) = oneshot::channel();
        receipt.send(Ok(())).unwrap();
        publish_chat_result(result, draft, echo, Some(writer.clone()), events).await;
        let AppEvent::MessageReceived { message: local, .. } = received.recv().await.unwrap()
        else {
            panic!("expected successful local echo");
        };
        state
            .channels
            .get_mut(&local.channel)
            .unwrap()
            .push_message(local.clone());
        state.push_mention(local.clone(), true);
        let clear = crust_core::model::ModerationAction::channel(
            local.channel.clone(),
            Some(local.timestamp + chrono::Duration::milliseconds(2)),
        );
        state.apply_moderation(&clear);
        writer.moderate(clear).await.unwrap();
        check_confirmed_echo_chronology(
            &mut state,
            &writer,
            &store,
            &local,
            local.timestamp + chrono::Duration::milliseconds(1),
            true,
        )
        .await;
        writer.shutdown().await.unwrap();
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

    #[tokio::test]
    async fn failed_whispers_never_enter_sent_history() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let channel = crate::whisper_thread_channel_id("123", "bob");
        let event = complete_whisper_send(
            reqwest::StatusCode::FORBIDDEN,
            "rejected",
            "123",
            "alice",
            "bob",
            "  hello 🦀  ",
            Vec::new(),
            Some(&writer),
        )
        .await;
        assert!(
            matches!(event, AppEvent::WhisperSendFailed { owner_user_id, target_login, text, .. }
            if owner_user_id == "123" && target_login == "bob" && text == "  hello 🦀  ")
        );
        writer.flush().await.unwrap();
        assert!(store.recent_messages(&channel, 10).unwrap().is_empty());
        let event = complete_whisper_send(
            reqwest::StatusCode::NO_CONTENT,
            "",
            "123",
            "alice",
            "bob",
            "  🦀 Kappa  ",
            vec![TwitchEmotePos {
                id: "25".into(),
                start: 2,
                end: 6,
            }],
            Some(&writer),
        )
        .await;
        assert!(matches!(
            event,
            AppEvent::WhisperReceived { is_self: true, owner_user_id, from_user_id, text, twitch_emotes, .. }
                if owner_user_id == "123" && from_user_id == "123" && text == "🦀 Kappa"
                    && twitch_emotes.len() == 1 && twitch_emotes[0].id == "25"
                    && twitch_emotes[0].start == 2 && twitch_emotes[0].end == 6
        ));
        writer.shutdown().await.unwrap();
        let messages = store.recent_messages(&channel, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].raw_text, "🦀 Kappa");
        assert_eq!(messages[0].sender.login, "alice");
        assert_eq!(messages[0].sender.user_id.0, "123");
    }

    #[tokio::test]
    async fn private_echo_opens_its_conversation_only_after_a_successful_write() {
        for succeeds in [false, true] {
            let mut publisher = ChatSendPublisher::default();
            let (events, mut received) = mpsc::channel(4);
            let (receipt, outcome) = oneshot::channel();
            let (draft, mut echo) = pending();
            let query = ChannelId::irc_query("irc.example", 6697, true, "bob");
            echo.channel = query.clone();
            assert!(
                publisher
                    .submit_irc(outcome, draft, Some(echo), None, events)
                    .await
            );
            receipt
                .send(if succeeds {
                    Ok("alice".into())
                } else {
                    Err("offline".into())
                })
                .unwrap();
            let first = tokio::time::timeout(std::time::Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap();
            if succeeds {
                assert!(matches!(first, AppEvent::ChannelJoined { channel } if channel == query));
                let echo = tokio::time::timeout(std::time::Duration::from_secs(3), received.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(echo, AppEvent::MessageReceived { channel, message } if channel == query && message.sender.login == "alice")
                );
            } else {
                assert!(matches!(first, AppEvent::MessageSendRejected { .. }));
                assert!(received.try_recv().is_err());
            }
        }
    }
}
