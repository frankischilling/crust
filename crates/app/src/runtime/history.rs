use std::collections::{HashMap, HashSet};

use crust_core::{
    events::AppEvent,
    ignores::{CompiledIgnoredPhrases, IgnoredPhrase},
    model::{
        ChannelId, ChatMessage, HistoryCursor, LocalHistoryMessageIdentity, LocalHistoryPageInfo,
        MessageId, ModerationAction, ModerationState, ModerationTarget, Nickname,
    },
};
use crust_storage::LogStore;
use crust_twitch::{parse_line, parse_privmsg_irc, parse_usernotice_irc};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::seventv::SevenTvCosmeticUpdate;

use super::badges::{resolve_badge_url, BadgeMap};
use super::log_writer::{flush_before_read, publish_history_moderation, LogWriter};

const WHISPER_HISTORY_CHANNEL_PREFIX: &str = "whisper:uid:";

/// Filter account-owned events before either plugins or the UI receive them.
#[derive(Default)]
pub(crate) struct WhisperAccount {
    owner: Option<String>,
    user_id: Option<String>,
}

impl WhisperAccount {
    pub(crate) fn accepts(&mut self, event: &AppEvent) -> bool {
        match event {
            AppEvent::Authenticated { username, user_id } => {
                self.owner = crate::normalize_whisper_login(username);
                self.user_id = crate::normalize_whisper_user_id(user_id).map(str::to_owned);
            }
            AppEvent::LoggedOut => {
                self.owner = None;
                self.user_id = None;
            }
            AppEvent::WhisperReceived {
                owner_user_id,
                from_user_id,
                from_login,
                target_login,
                is_self,
                ..
            } => {
                if self.user_id.as_deref() != crate::normalize_whisper_user_id(owner_user_id)
                    || self.user_id.is_none()
                    || (*is_self
                        && crate::normalize_whisper_user_id(from_user_id)
                            != self.user_id.as_deref())
                {
                    return false;
                }
                let owner = if *is_self { from_login } else { target_login };
                return self.owner.as_ref().is_some_and(|active| {
                    crate::normalize_whisper_login(owner).as_ref() == Some(active)
                });
            }
            AppEvent::WhisperSendFailed { owner_user_id, .. } => {
                return self.user_id.is_some()
                    && self.user_id.as_deref() == crate::normalize_whisper_user_id(owner_user_id);
            }
            AppEvent::SelfAvatarLoaded { account, .. } => {
                return self.owner.as_ref().is_some_and(|active| {
                    crate::normalize_whisper_login(account).as_ref() == Some(active)
                });
            }
            _ => {}
        }
        true
    }
}

/// Keep the sender's supplied identity when live whispers enter history and the UI.
pub(crate) async fn prepare_live_whisper(
    mut event: AppEvent,
    local_login: Option<&str>,
    local_user_id: Option<&str>,
    emote_index: &crate::EmoteIndex,
    ignored_phrases: &CompiledIgnoredPhrases,
    log_writer: Option<&LogWriter>,
) -> Option<AppEvent> {
    let AppEvent::WhisperReceived {
        owner_user_id,
        from_user_id,
        from_login,
        target_login,
        text,
        twitch_emotes,
        is_self,
        is_history,
        ..
    } = &mut event
    else {
        return None;
    };
    let local_login = local_login.and_then(crate::normalize_whisper_login)?;
    let local_user_id = local_user_id.and_then(crate::normalize_whisper_user_id)?;
    if crate::normalize_whisper_user_id(owner_user_id) != Some(local_user_id)
        || (*is_self && crate::normalize_whisper_user_id(from_user_id) != Some(local_user_id))
    {
        return None;
    }
    let owner = crate::normalize_whisper_login(if *is_self { from_login } else { target_login });
    if owner.as_deref() != Some(local_login.as_str()) {
        return None;
    }
    if !*is_self
        && !*is_history
        && ignored_phrases
            .apply_with_emotes(text, twitch_emotes)
            .blocked
    {
        return None;
    }
    emote_index
        .write()
        .unwrap()
        .queue_whisper_sender(local_user_id, from_user_id);
    if !*is_self {
        if let Some(writer) = log_writer {
            if let Err(error) = writer
                .whisper(local_login.clone(), local_user_id.to_owned(), event.clone())
                .await
            {
                warn!("whisper-history: failed to queue whisper: {error}");
            }
        }
    }
    Some(event)
}

#[derive(Default)]
pub(crate) struct WhisperHistory {
    owner: Option<(String, String)>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl WhisperHistory {
    pub(crate) fn load(
        &mut self,
        log_store: LogStore,
        owner: &str,
        owner_user_id: &str,
        emote_index: &crate::EmoteIndex,
        evt_tx: mpsc::Sender<AppEvent>,
        log_writer: Option<LogWriter>,
    ) {
        let Some(owner) = crate::normalize_whisper_login(owner) else {
            return;
        };
        let Some(owner_user_id) =
            crate::normalize_whisper_user_id(owner_user_id).map(str::to_owned)
        else {
            return;
        };
        let identity = (owner_user_id.clone(), owner.clone());
        if self.owner.as_ref() == Some(&identity) {
            return;
        }
        self.clear();
        self.owner = Some(identity);
        let emotes = emote_index.clone();
        self.task = Some(tokio::spawn(async move {
            flush_before_read(log_writer.as_ref()).await;
            load_local_recent_whispers(
                log_store,
                Some(owner),
                Some(owner_user_id),
                evt_tx,
                Some(emotes),
            )
            .await;
        }));
    }

    pub(crate) fn clear(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.owner = None;
    }
}

impl Drop for WhisperHistory {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod whisper_tests {
    use super::*;

    fn whisper(owner_user_id: &str, owner: &str, is_self: bool) -> AppEvent {
        AppEvent::WhisperReceived {
            owner_user_id: owner_user_id.to_owned(),
            from_user_id: if is_self {
                owner_user_id.to_owned()
            } else {
                String::new()
            },
            from_login: if is_self { owner } else { "bob" }.into(),
            from_display_name: "Bob".into(),
            target_login: if is_self { "bob" } else { owner }.into(),
            text: "private message".into(),
            twitch_emotes: Vec::new(),
            is_self,
            timestamp: chrono::Utc::now(),
            is_history: false,
        }
    }

    fn save(store: &LogStore, owner_user_id: &str, owner: &str, text: &str) {
        let mut event = whisper(owner_user_id, owner, false);
        if let AppEvent::WhisperReceived { text: body, .. } = &mut event {
            *body = text.to_owned();
        }
        crate::persist_whisper_message(store, Some(owner), Some(owner_user_id), &event).unwrap();
    }

    #[test]
    fn account_filter_rejects_old_callbacks_before_plugin_dispatch() {
        let mut account = WhisperAccount::default();
        assert!(!account.accepts(&whisper("1", "alice", false)));
        account.accepts(&AppEvent::Authenticated {
            username: "Alice".into(),
            user_id: "1".into(),
        });
        assert!(account.accepts(&whisper("1", "ALICE", false)));
        assert!(account.accepts(&whisper("1", "alice", true)));
        account.accepts(&AppEvent::LoggedOut);
        assert!(!account.accepts(&whisper("1", "alice", false)));
        account.accepts(&AppEvent::Authenticated {
            username: "carol".into(),
            user_id: "2".into(),
        });
        assert!(!account.accepts(&whisper("1", "alice", false)));
        assert!(!account.accepts(&whisper("1", "alice", true)));
        assert!(account.accepts(&whisper("2", "carol", false)));
        account.accepts(&AppEvent::Authenticated {
            username: "carol".into(),
            user_id: "3".into(),
        });
        assert!(!account.accepts(&whisper("2", "carol", false)));
        assert!(!account.accepts(&whisper("2", "carol", true)));
        assert!(account.accepts(&whisper("3", "carol", false)));
    }

    #[tokio::test]
    async fn whisper_failures_follow_owner_ids_before_plugin_dispatch() {
        let mut account = WhisperAccount::default();
        async fn failed(owner: &str) -> AppEvent {
            super::super::outgoing::complete_whisper_send(
                reqwest::StatusCode::FORBIDDEN,
                "unavailable",
                owner,
                "alice",
                "bob",
                "private message",
                Vec::new(),
                None,
            )
            .await
        }
        assert!(!account.accepts(&failed("1").await));
        account.accepts(&AppEvent::Authenticated {
            username: "alice".into(),
            user_id: "1".into(),
        });
        assert!(account.accepts(&failed("1").await));
        account.accepts(&AppEvent::LoggedOut);
        assert!(!account.accepts(&failed("1").await));
        account.accepts(&AppEvent::Authenticated {
            username: "alice".into(),
            user_id: "2".into(),
        });
        assert!(!account.accepts(&failed("1").await));
        assert!(!account.accepts(&failed("").await));
        assert!(account.accepts(&failed("2").await));
        account.accepts(&AppEvent::Authenticated {
            username: "renamed".into(),
            user_id: "2".into(),
        });
        assert!(account.accepts(&failed("2").await));
    }

    #[test]
    fn account_filter_rejects_stale_avatars_before_plugin_dispatch() {
        let mut account = WhisperAccount::default();
        let avatar = |owner: &str| AppEvent::SelfAvatarLoaded {
            account: owner.to_owned(),
            avatar_url: "https://example.test/avatar.png".to_owned(),
        };
        assert!(!account.accepts(&avatar("alice")));
        account.accepts(&AppEvent::Authenticated {
            username: "Alice".into(),
            user_id: "1".into(),
        });
        assert!(account.accepts(&avatar("ALICE")));
        assert!(!account.accepts(&avatar("")));
        account.accepts(&AppEvent::Authenticated {
            username: "carol".into(),
            user_id: "2".into(),
        });
        assert!(!account.accepts(&avatar("alice")));
        assert!(account.accepts(&avatar("carol")));
        account.accepts(&AppEvent::LoggedOut);
        assert!(!account.accepts(&avatar("carol")));
    }

    #[tokio::test]
    async fn history_reads_only_owned_threads_and_leaves_legacy_rows_unassigned() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        save(&store, "1", "reused", "first account");
        save(&store, "2", "reused", "second account");
        let mut legacy = super::super::system_messages::make_system_message(
            42,
            ChannelId("whisper:bob".into()),
            "unowned legacy".into(),
            chrono::Utc::now(),
            crust_core::model::MsgKind::Chat,
        );
        legacy.sender.login = "bob".into();
        store.append_message(&legacy).unwrap();
        legacy.channel = ChannelId("whisper:reused:bob".into());
        legacy.raw_text = "login-owned legacy".into();
        store.append_message(&legacy).unwrap();
        let (events, mut received) = mpsc::channel(8);
        for (owner, text) in [("1", "first account"), ("2", "second account")] {
            load_local_recent_whispers(
                store.clone(),
                Some("reused".into()),
                Some(owner.into()),
                events.clone(),
                None,
            )
            .await;
            let AppEvent::WhisperReceived {
                owner_user_id,
                target_login,
                text: actual,
                from_login,
                ..
            } = received.try_recv().unwrap()
            else {
                panic!("expected account whisper history");
            };
            assert_eq!(owner_user_id, owner);
            assert_eq!(target_login, "reused");
            assert_eq!(actual, text);
            assert_eq!(from_login, "bob");
            assert!(received.try_recv().is_err());
        }
        load_local_recent_whispers(store.clone(), None, Some("1".into()), events.clone(), None)
            .await;
        load_local_recent_whispers(store.clone(), Some("reused".into()), None, events, None).await;
        assert!(received.try_recv().is_err());
        assert_eq!(
            store
                .recent_messages(&ChannelId("whisper:bob".into()), 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .recent_messages(&ChannelId("whisper:reused:bob".into()), 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn renamed_accounts_keep_history_and_replay_self_rows_by_sender_id() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        save(&store, "10", "before", "incoming before rename");
        let mut sent = whisper("10", "before", true);
        if let AppEvent::WhisperReceived { text, .. } = &mut sent {
            *text = "sent before rename".into();
        }
        crate::persist_whisper_message(&store, Some("before"), Some("10"), &sent).unwrap();
        let channel = crate::whisper_thread_channel_id("10", "bob");
        let mut bad_self = store
            .recent_messages(&channel, 10)
            .unwrap()
            .into_iter()
            .find(|message| message.flags.is_self)
            .unwrap();
        bad_self.sender.user_id.0 = "99".into();
        bad_self.raw_text = "wrong sender ID".into();
        store.append_message(&bad_self).unwrap();
        bad_self.sender.user_id.0.clear();
        bad_self.raw_text = "unknown sender ID".into();
        store.append_message(&bad_self).unwrap();
        let (events, mut received) = mpsc::channel(8);
        load_local_recent_whispers(store, Some("after".into()), Some("10".into()), events, None)
            .await;
        let mut count = 0;
        while let Ok(event) = received.try_recv() {
            let AppEvent::WhisperReceived {
                owner_user_id,
                from_user_id,
                from_login,
                target_login,
                text,
                is_self,
                ..
            } = event
            else {
                panic!("expected renamed account history");
            };
            assert_eq!(owner_user_id, "10");
            if is_self {
                assert_eq!(from_user_id, "10");
                assert_eq!(from_login, "after");
                assert_eq!(target_login, "bob");
                assert_eq!(text, "sent before rename");
            } else {
                assert_eq!(from_login, "bob");
                assert_eq!(target_login, "after");
                assert_eq!(text, "incoming before rename");
            }
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn stale_whispers_cannot_be_saved_under_a_different_account() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        for is_self in [false, true] {
            let event = whisper("1", "alice", is_self);
            for (local, local_id) in [
                (Some("carol"), Some("1")),
                (Some("alice"), Some("2")),
                (Some("alice"), None),
                (None, Some("1")),
            ] {
                crate::persist_whisper_message(&store, local, local_id, &event).unwrap();
            }
        }
        assert!(store
            .recent_channels_with_prefix("whisper:", 20)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn incoming_whisper_blocking_precedes_storage_and_sender_loading() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (writer_events, _writer_received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            writer_events,
        );
        let index = crate::EmoteIndex::default();
        index.write().unwrap().set_account(Some("10"));
        let mut incoming = whisper("10", "alice", false);
        if let AppEvent::WhisperReceived {
            from_user_id, text, ..
        } = &mut incoming
        {
            *from_user_id = "20".into();
            *text = "SPAM".into();
        }
        let rules = CompiledIgnoredPhrases::new(&[IgnoredPhrase::new("spam")]);
        assert!(prepare_live_whisper(
            incoming,
            Some("alice"),
            Some("10"),
            &index,
            &rules,
            Some(&writer),
        )
        .await
        .is_none());
        writer.shutdown().await.unwrap();
        assert!(store
            .recent_channels_with_prefix("whisper:", 10)
            .unwrap()
            .is_empty());
        assert!(index
            .write()
            .unwrap()
            .next_deferred_personal_load()
            .is_none());
    }

    #[tokio::test]
    async fn incoming_whisper_replacement_preserves_emotes_and_replays_once() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (writer_events, _writer_received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            writer_events,
        );
        let index = crate::EmoteIndex::default();
        index.write().unwrap().set_account(Some("10"));
        let mut phrase = IgnoredPhrase::new("badword");
        phrase.action = crust_core::ignores::IgnoredPhraseAction::Replace;
        phrase.replace_with = "badwordbadword".into();
        let rules = CompiledIgnoredPhrases::new(&[phrase]);
        let mut incoming = whisper("10", "alice", false);
        if let AppEvent::WhisperReceived {
            from_user_id,
            text,
            twitch_emotes,
            ..
        } = &mut incoming
        {
            *from_user_id = "20".into();
            *text = "badword Native 🦀".into();
            twitch_emotes.push(crust_core::model::TwitchEmotePos {
                id: "unowned".into(),
                start: 8,
                end: 13,
            });
        }
        let forwarded = prepare_live_whisper(
            incoming,
            Some("alice"),
            Some("10"),
            &index,
            &rules,
            Some(&writer),
        )
        .await
        .unwrap();
        assert!(
            matches!(forwarded, AppEvent::WhisperReceived { text, twitch_emotes, is_history: false, .. }
            if text == "badwordbadword Native 🦀" && twitch_emotes.len() == 1
            && twitch_emotes[0].id == "unowned"
            && twitch_emotes[0].start == 15 && twitch_emotes[0].end == 20)
        );
        writer.shutdown().await.unwrap();

        let (events, mut received) = mpsc::channel(4);
        load_local_recent_whispers(store, Some("alice".into()), Some("10".into()), events, None)
            .await;
        let replay = received.try_recv().unwrap();
        assert!(
            matches!(&replay, AppEvent::WhisperReceived { text, twitch_emotes, is_history: true, .. }
            if text == "badwordbadword Native 🦀" && twitch_emotes.len() == 1
            && twitch_emotes[0].start == 15 && twitch_emotes[0].end == 20)
        );
        assert!(received.try_recv().is_err());
        let unchanged =
            prepare_live_whisper(replay, Some("alice"), Some("10"), &index, &rules, None)
                .await
                .unwrap();
        assert!(
            matches!(unchanged, AppEvent::WhisperReceived { text, is_history: true, .. }
            if text == "badwordbadword Native 🦀")
        );
    }

    #[tokio::test]
    async fn self_whisper_echoes_keep_the_sent_text_when_phrase_rules_match() {
        let index = crate::EmoteIndex::default();
        index.write().unwrap().set_account(Some("10"));
        let mut outgoing = whisper("10", "alice", true);
        if let AppEvent::WhisperReceived { text, .. } = &mut outgoing {
            *text = "spam".into();
        }
        let rules = CompiledIgnoredPhrases::new(&[IgnoredPhrase::new("spam")]);
        let forwarded =
            prepare_live_whisper(outgoing, Some("alice"), Some("10"), &index, &rules, None)
                .await
                .unwrap();
        assert!(
            matches!(forwarded, AppEvent::WhisperReceived { text, is_self: true, .. }
            if text == "spam")
        );
    }

    #[tokio::test]
    async fn live_whisper_sender_ids_survive_storage_and_replay_for_personal_emotes() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (writer_events, _writer_received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            writer_events,
        );
        let live_index = crate::EmoteIndex::default();
        live_index.write().unwrap().set_account(Some("10"));
        let mut live = whisper("10", "alice", false);
        if let AppEvent::WhisperReceived {
            from_user_id,
            text,
            is_history,
            ..
        } = &mut live
        {
            *from_user_id = "20".into();
            *text = "PersonalHello".into();
            *is_history = false;
        }
        let forwarded = prepare_live_whisper(
            live,
            Some("alice"),
            Some("10"),
            &live_index,
            &CompiledIgnoredPhrases::new(&[]),
            Some(&writer),
        )
        .await
        .unwrap();
        writer.shutdown().await.unwrap();
        assert!(
            matches!(forwarded, AppEvent::WhisperReceived { from_user_id, is_history: false, .. } if from_user_id == "20")
        );
        assert_eq!(
            live_index.write().unwrap().next_deferred_personal_load(),
            Some(("20".into(), None))
        );
        let rows = store
            .recent_messages(&crate::whisper_thread_channel_id("10", "bob"), 10)
            .unwrap();
        assert_eq!(rows[0].sender.user_id.0, "20");

        // Rows without IDs and the old login placeholders still display, but
        // neither can enroll a guessed sender in personal-emote lookups.
        save(&store, "10", "alice", "missing ID");
        let mut legacy = rows[0].clone();
        legacy.sender.user_id.0 = legacy.sender.login.clone();
        legacy.raw_text = "legacy placeholder".into();
        store.append_message(&legacy).unwrap();

        let replay_index = crate::EmoteIndex::default();
        replay_index.write().unwrap().set_account(Some("10"));
        let (events, mut received) = mpsc::channel(8);
        load_local_recent_whispers(
            store,
            Some("alice".into()),
            Some("10".into()),
            events,
            Some(replay_index.clone()),
        )
        .await;
        let mut identities = HashMap::new();
        while let Ok(event) = received.try_recv() {
            let AppEvent::WhisperReceived {
                from_user_id,
                text,
                target_login,
                is_history,
                ..
            } = event
            else {
                panic!("expected whisper replay");
            };
            assert!(is_history);
            assert_eq!(target_login, "alice");
            identities.insert(text, from_user_id);
        }
        assert_eq!(identities["PersonalHello"], "20");
        assert_eq!(identities["missing ID"], "");
        assert_eq!(identities["legacy placeholder"], "");

        let mut registry = replay_index.write().unwrap();
        assert_eq!(
            registry.next_deferred_personal_load(),
            Some(("20".into(), None))
        );
        assert!(registry.next_deferred_personal_load().is_none());
        let generation = registry.begin_personal_load("20", None).unwrap();
        registry
            .complete_personal_load(
                "20",
                generation,
                Some(vec![crust_emotes::EmoteInfo {
                    provider: "7tv".into(),
                    id: "personal".into(),
                    code: "PersonalHello".into(),
                    zero_width: false,
                    url_1x: "https://example.test/personal.png".into(),
                    url_2x: None,
                    url_4x: None,
                }]),
            )
            .unwrap();
        assert!(registry
            .resolve(&ChannelId::mentions(), "20", "PersonalHello")
            .is_some());
        assert!(registry
            .resolve(&ChannelId::mentions(), "10", "PersonalHello")
            .is_none());
        assert!(registry
            .resolve(&ChannelId::mentions(), "", "PersonalHello")
            .is_none());
    }

    #[tokio::test]
    async fn stale_live_whispers_do_not_schedule_sender_lookups_or_history() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (writer_events, _writer_received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            writer_events,
        );
        let index = crate::EmoteIndex::default();
        index.write().unwrap().set_account(Some("30"));
        let mut stale = whisper("10", "alice", false);
        if let AppEvent::WhisperReceived { from_user_id, .. } = &mut stale {
            *from_user_id = "20".into();
        }
        assert!(prepare_live_whisper(
            stale,
            Some("alice"),
            Some("30"),
            &index,
            &CompiledIgnoredPhrases::new(&[]),
            Some(&writer)
        )
        .await
        .is_none());
        writer.shutdown().await.unwrap();
        assert!(index
            .write()
            .unwrap()
            .next_deferred_personal_load()
            .is_none());
        assert!(store
            .recent_channels_with_prefix("whisper:", 20)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn clearing_history_aborts_blocked_replays_and_allows_the_next_account() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        save(&store, "1", "alice", "old account");
        save(&store, "2", "carol", "new account");
        let (events, mut received) = mpsc::channel(1);
        events.send(AppEvent::LoggedOut).await.unwrap();
        let mut replay = WhisperHistory::default();
        let emotes = crate::EmoteIndex::default();
        replay.load(store.clone(), "alice", "1", &emotes, events.clone(), None);
        let handle = replay.task.as_ref().unwrap().abort_handle();
        replay.clear();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(received.try_recv().unwrap(), AppEvent::LoggedOut));
        assert!(received.try_recv().is_err());
        replay.load(store, "carol", "2", &emotes, events, None);
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            replay.task.take().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::WhisperReceived { target_login, text, .. }
            if target_login == "carol" && text == "new account")
        );
        assert!(received.try_recv().is_err());
    }
}

/// Load locally persisted chat history from SQLite and replay it as
/// `AppEvent::HistoryLoaded` for the channel.
pub(crate) async fn load_local_recent_messages(
    channel: ChannelId,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    load_local_history_page(
        channel,
        None,
        LogStore::default_recent_limit(),
        log_store,
        evt_tx,
    )
    .await;
}

/// Load older locally persisted chat history rows before `before_ts_ms` and
/// replay them as `AppEvent::HistoryLoaded` for incremental backfill.
pub(crate) async fn load_local_older_messages(
    channel: ChannelId,
    before_ts_ms: i64,
    before_row_id: Option<i64>,
    limit: usize,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let before = HistoryCursor {
        timestamp_ms: before_ts_ms,
        row_id: before_row_id.unwrap_or(i64::MIN),
    };
    load_local_history_page(channel, Some(before), limit, log_store, evt_tx).await;
}

async fn load_local_history_page(
    channel: ChannelId,
    before: Option<HistoryCursor>,
    limit: usize,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let channel_for_query = channel.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let page = log_store.history_page(&channel_for_query, before, limit)?;
        let moderation = if before.is_none() {
            log_store.saved_moderation(&channel_for_query)?
        } else {
            Vec::new()
        };
        Ok::<_, crust_storage::StorageError>((page, moderation))
    })
    .await;

    let (page, moderation) = match loaded {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(e)) => {
            warn!(
                "chat-history: local older SQLite load failed for #{}: {e}",
                channel.display_name()
            );
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "History".into(),
                    message: format!(
                        "Could not load local history for #{}: {e}",
                        channel.display_name()
                    ),
                })
                .await;
            return;
        }
        Err(e) => {
            warn!(
                "chat-history: local older SQLite task failed for #{}: {e}",
                channel.display_name()
            );
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "History".into(),
                    message: format!(
                        "Could not load local history for #{}: {e}",
                        channel.display_name()
                    ),
                })
                .await;
            return;
        }
    };

    if !moderation.is_empty() {
        if evt_tx
            .send(AppEvent::HistoryModerationLoaded {
                channel: channel.clone(),
                actions: moderation,
            })
            .await
            .is_err()
        {
            return;
        }
    }
    let mut messages = Vec::with_capacity(page.messages.len());
    let mut identities = Vec::with_capacity(page.messages.len());
    for (mut msg, row_id) in page.messages.into_iter().zip(page.row_ids) {
        let Some(replay_id) = MessageId::from_local_history_row(row_id) else {
            continue;
        };
        identities.push(LocalHistoryMessageIdentity {
            replay_id,
            original_id: msg.id,
        });
        msg.id = replay_id;
        msg.flags.is_history = true;
        msg.channel = channel.clone();
        messages.push(msg);
    }

    info!(
        "chat-history: loaded {} older local SQLite messages for #{}",
        messages.len(),
        channel.display_name()
    );
    let _ = evt_tx
        .send(AppEvent::HistoryLoaded {
            channel,
            messages,
            local: Some(LocalHistoryPageInfo {
                before,
                next: page.next,
                has_more: page.has_more,
                identities,
            }),
        })
        .await;
}

#[cfg(test)]
mod local_history_identity_tests {
    use super::*;
    use crust_core::model::{ChannelState, MsgKind};

    #[tokio::test]
    async fn local_restore_sends_one_internal_moderation_batch_before_messages() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let channel = ChannelId::new("room");
        let mut message = super::super::system_messages::make_system_message(
            1,
            channel.clone(),
            "original text".into(),
            chrono::Utc::now(),
            MsgKind::Chat,
        );
        message.server_id = Some("old-message".into());
        store.append_message(&message).unwrap();
        store
            .apply_moderation(&ModerationAction::message(
                channel.clone(),
                "old-message".into(),
            ))
            .unwrap();
        let (events, mut received) = mpsc::channel(4);
        load_local_recent_messages(channel, store, events).await;
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::HistoryModerationLoaded { actions, .. } if actions.len() == 1)
        );
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::HistoryLoaded { messages, .. }
            if messages.len() == 1 && messages[0].flags.is_deleted && messages[0].raw_text == "original text")
        );
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_replays_keep_storage_ids_and_match_live_rows_after_reload() {
        let store = LogStore::with_db_path(std::path::PathBuf::from(":memory:")).unwrap();
        let channel = ChannelId::new("room");
        let mut room = ChannelState::new(channel.clone());
        for id in [17, 18, 19] {
            let message = super::super::system_messages::make_system_message(
                id,
                channel.clone(),
                "same text".into(),
                chrono::DateTime::from_timestamp_millis(1000).unwrap(),
                MsgKind::Chat,
            );
            store.append_message(&message).unwrap();
            if id == 19 {
                room.push_message(message);
            }
        }
        let (events, mut received) = mpsc::channel(4);
        let mut previous_ids = None;
        for _ in 0..3 {
            load_local_recent_messages(channel.clone(), store.clone(), events.clone()).await;
            let AppEvent::HistoryLoaded {
                messages,
                local: Some(page),
                ..
            } = received.recv().await.unwrap()
            else {
                panic!("expected a local history page");
            };
            let ids: Vec<_> = messages.iter().map(|message| message.id).collect();
            assert!(ids.iter().all(|id| id.is_local_history()));
            assert_eq!(
                page.identities
                    .iter()
                    .map(|identity| identity.original_id.0)
                    .collect::<Vec<_>>(),
                [17, 18, 19]
            );
            if let Some(previous) = &previous_ids {
                assert_eq!(&ids, previous);
            }
            previous_ids = Some(ids);
            room.prepend_local_history(messages, &page);
            assert_eq!(room.messages.len(), 3);
            assert_eq!(
                room.messages
                    .iter()
                    .filter(|message| message.id == MessageId(19))
                    .count(),
                1
            );
        }
    }
}

/// Load cross-channel mention-matching messages from SQLite and dispatch as
/// a single `AppEvent::MentionsLoaded` event. Intended to run once on
/// startup to backfill the Mentions pseudo-tab.
pub(crate) async fn load_local_recent_mentions(
    log_store: LogStore,
    limit: usize,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let loaded = tokio::task::spawn_blocking(move || log_store.recent_mentions(limit)).await;

    let mut messages = match loaded {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            warn!("mentions-history: local SQLite load failed: {e}");
            return;
        }
        Err(e) => {
            warn!("mentions-history: local SQLite task failed: {e}");
            return;
        }
    };

    if messages.is_empty() {
        return;
    }

    for msg in &mut messages {
        msg.id =
            MessageId(crate::HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed));
        msg.flags.is_history = true;
    }

    info!(
        "mentions-history: loaded {} cross-channel mention rows",
        messages.len()
    );
    let _ = evt_tx.send(AppEvent::MentionsLoaded { messages }).await;
}

/// Load locally persisted whispers from SQLite and replay them as
/// `AppEvent::WhisperReceived` with `is_history=true`.
pub(crate) async fn load_local_recent_whispers(
    log_store: LogStore,
    self_login: Option<String>,
    self_user_id: Option<String>,
    evt_tx: mpsc::Sender<AppEvent>,
    emotes: Option<crate::EmoteIndex>,
) {
    let Some(self_login) = self_login
        .as_deref()
        .and_then(crate::normalize_whisper_login)
    else {
        return;
    };
    let Some(self_user_id) = self_user_id
        .as_deref()
        .and_then(crate::normalize_whisper_user_id)
    else {
        return;
    };
    let account_prefix = format!("{WHISPER_HISTORY_CHANNEL_PREFIX}{self_user_id}:");
    let channels = tokio::task::spawn_blocking({
        let store = log_store.clone();
        let prefix = account_prefix.clone();
        move || store.recent_channels_with_prefix(&prefix, 250)
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

    for channel in whisper_channels {
        let Some(partner_login) = channel
            .strip_prefix(&account_prefix)
            .and_then(crate::normalize_whisper_login)
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
                warn!("whisper-history: failed loading channel {}: {e}", channel);
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
            let stored_login = msg.sender.login.trim().to_ascii_lowercase();
            let from_user_id = crate::normalize_whisper_user_id(&msg.sender.user_id.0)
                .unwrap_or_default()
                .to_owned();
            let (from_login, from_display_name) = if msg.flags.is_self {
                if from_user_id != self_user_id {
                    continue;
                }
                (self_login.clone(), self_login.clone())
            } else {
                if stored_login.is_empty() || stored_login != partner_login {
                    continue;
                }
                (stored_login, msg.sender.display_name)
            };
            let target_login = if msg.flags.is_self {
                partner_login.clone()
            } else {
                self_login.clone()
            };
            if let Some(index) = &emotes {
                index
                    .write()
                    .unwrap()
                    .queue_whisper_sender(self_user_id, &from_user_id);
            }
            if evt_tx
                .send(AppEvent::WhisperReceived {
                    owner_user_id: self_user_id.to_owned(),
                    from_user_id,
                    from_login,
                    from_display_name,
                    target_login,
                    text: msg.raw_text,
                    twitch_emotes: msg.twitch_emotes,
                    is_self: msg.flags.is_self,
                    timestamp: msg.timestamp,
                    is_history: true,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

fn parse_history_records(
    raw_lines: &[String],
    local_nick: Option<&str>,
    nicknames: &[Nickname],
    channel: &ChannelId,
    saved_actions: &[ModerationAction],
    ignored_phrases: &[IgnoredPhrase],
) -> (Vec<ChatMessage>, Vec<ModerationAction>) {
    let state = ModerationState::default();
    state.remember(saved_actions);
    let mut messages = Vec::with_capacity(raw_lines.len());
    let mut actions = Vec::new();
    for line in raw_lines {
        let Ok(record) = parse_line(line) else {
            continue;
        };
        if record
            .params
            .first()
            .map(|value| ChannelId::new(value))
            .as_ref()
            != Some(channel)
        {
            continue;
        }
        let action = match record.command.as_str() {
            "PRIVMSG" | "USERNOTICE" => {
                let id = crate::HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let message = if record.command == "PRIVMSG" {
                    parse_privmsg_irc(&record, local_nick, id)
                } else {
                    parse_usernotice_irc(&record, local_nick, id)
                };
                if let Some(mut message) = message {
                    state.apply(&mut message);
                    state.observe_deleted(&message);
                    messages.push(message);
                }
                continue;
            }
            "CLEARMSG" => {
                let Some(id) = record.tags.get("target-msg-id").filter(|id| !id.is_empty()) else {
                    continue;
                };
                ModerationAction::message(channel.clone(), id.to_owned())
            }
            "CLEARCHAT" => {
                let timestamp = record
                    .tags
                    .get("tmi-sent-ts")
                    .and_then(|value| value.parse::<i64>().ok())
                    .and_then(chrono::DateTime::from_timestamp_millis);
                if let Some(login) = record
                    .params
                    .get(1)
                    .filter(|login| !login.trim().is_empty())
                {
                    ModerationAction::user(
                        channel.clone(),
                        login.clone(),
                        record.tags.get("target-user-id").map(str::to_owned),
                        timestamp,
                    )
                } else {
                    ModerationAction::channel(channel.clone(), timestamp)
                }
            }
            _ => continue,
        };
        state.apply_to_retained(&action, &mut messages);
        // Untimed clears describe only the rows already seen in this history
        // response. Publishing the broad action could delete newer live chat.
        if action.timestamp.is_some() || matches!(action.target, ModerationTarget::Message(_)) {
            actions.push(action);
        }
    }
    let published = ModerationState::default();
    published.remember(saved_actions);
    for action in &actions {
        published.apply_to_retained(action, std::iter::empty());
    }
    for message in &mut messages {
        state.apply(message);
        if message.flags.is_deleted && !published.is_deleted(message) {
            if let Some(id) = message.server_id.as_ref().filter(|id| !id.is_empty()) {
                let action = ModerationAction::message(channel.clone(), id.clone());
                published.apply_to_retained(&action, std::iter::empty());
                actions.push(action);
            }
        }
    }
    let ignored_phrases = CompiledIgnoredPhrases::new(ignored_phrases);
    messages.retain_mut(|message| {
        if ignored_phrases.apply_to_message(message).blocked {
            return false;
        }
        if let Some(nick) = local_nick {
            message.flags.is_mention |=
                crust_core::highlight::contains_mention(&message.raw_text, nick)
                    || message
                        .reply
                        .as_ref()
                        .is_some_and(|reply| reply.parent_user_login.eq_ignore_ascii_case(nick))
                    || crust_core::highlight::contains_nickname_mention(
                        &message.raw_text,
                        nick,
                        channel.display_name(),
                        nicknames,
                    );
        }
        true
    });
    (messages, actions)
}

#[cfg(test)]
mod moderation_history_tests {
    use super::*;

    fn lines(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|line| (*line).to_owned()).collect()
    }

    fn phrase_rule(
        pattern: &str,
        action: crust_core::ignores::IgnoredPhraseAction,
        replacement: &str,
    ) -> IgnoredPhrase {
        let mut phrase = IgnoredPhrase::new(pattern);
        phrase.action = action;
        phrase.replace_with = replacement.into();
        phrase
    }

    #[test]
    fn remote_history_keeps_rich_announcements_and_subscriber_bodies_with_moderation() {
        use crust_core::{ignores::IgnoredPhraseAction, model::MsgKind};

        let rules = [
            phrase_rule("badword", IgnoredPhraseAction::Replace, "ok"),
            phrase_rule("spam", IgnoredPhraseAction::Block, ""),
        ];
        let (messages, actions) = parse_history_records(&lines(&[
            "@id=announcement;msg-id=announcement;msg-param-color=GREEN;login=alice;user-id=10;emotes=25:8-12;tmi-sent-ts=1000 :tmi.twitch.tv USERNOTICE #room :badword Kappa",
            "@id=subscription;msg-id=resub;login=alice;user-id=10;display-name=Alice;msg-param-cumulative-months=2;tmi-sent-ts=1001 :tmi.twitch.tv USERNOTICE #room :subscriber text",
            "@id=mirror;msg-id=sharedchatnotice;source-msg-id=announcement;room-id=1;source-room-id=2;source-id=original;login=alice;user-id=10;tmi-sent-ts=1002 :tmi.twitch.tv USERNOTICE #room :shared body",
            "@id=payment;msg-id=sharedchatnotice;source-msg-id=sub;room-id=1;source-room-id=2;login=alice;user-id=10;tmi-sent-ts=1002 :tmi.twitch.tv USERNOTICE #room :other room subscription",
            "@target-msg-id=announcement :tmi.twitch.tv CLEARMSG #room :badword Kappa",
            "@target-user-id=10;tmi-sent-ts=1003 :tmi.twitch.tv CLEARCHAT #room :alice",
            "@id=new;msg-id=announcement;login=alice;user-id=10;tmi-sent-ts=1004 :tmi.twitch.tv USERNOTICE #room :new announcement",
            "@id=blocked;msg-id=announcement;login=alice;user-id=10;tmi-sent-ts=1005 :tmi.twitch.tv USERNOTICE #room :spam",
        ]), None, &[], &ChannelId::new("room"), &[], &rules);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].raw_text, "ok Kappa");
        assert_eq!((messages[0].twitch_emotes[0].start, messages[0].twitch_emotes[0].end), (3, 7));
        assert!(matches!(&messages[0].msg_kind, MsgKind::Announcement { color } if color == "green"));
        assert!(matches!(&messages[1].msg_kind, MsgKind::Sub { sub_msg, .. } if sub_msg == "subscriber text"));
        assert_eq!(messages[2].shared.as_ref().unwrap().source_message_id.as_deref(), Some("original"));
        assert_eq!(messages[3].server_id.as_deref(), Some("new"));
        assert!(messages[..3].iter().all(|message| message.flags.is_deleted));
        assert!(!messages[3].flags.is_deleted);
        assert!(messages.iter().all(|message| message.flags.is_history));
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn remote_history_applies_all_phrase_actions_before_display() {
        use crust_core::ignores::IgnoredPhraseAction;

        let rules = [
            phrase_rule("spam", IgnoredPhraseAction::Block, ""),
            phrase_rule("urgent", IgnoredPhraseAction::MentionOnly, ""),
            phrase_rule("bright", IgnoredPhraseAction::HighlightOnly, ""),
            phrase_rule("badword", IgnoredPhraseAction::Replace, "é"),
        ];
        let (messages, _) = parse_history_records(&lines(&[
            "@id=blocked;tmi-sent-ts=1000 :bob!user@host PRIVMSG #room :SPAM",
            "@id=mention;tmi-sent-ts=1001 :bob!user@host PRIVMSG #room :urgent",
            "@id=highlight;tmi-sent-ts=1002 :bob!user@host PRIVMSG #room :bright",
            "@id=replaced;emotes=unowned:8-13;tmi-sent-ts=1003 :bob!user@host PRIVMSG #room :badword Native",
        ]), Some("alice"), &[], &ChannelId::new("room"), &[], &rules);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].server_id.as_deref(), Some("mention"));
        assert!(messages[0].flags.is_mention);
        assert!(messages[1].flags.is_highlighted);
        assert_eq!(messages[2].raw_text, "é Native");
        assert_eq!(messages[2].twitch_emotes.len(), 1);
        assert_eq!(messages[2].twitch_emotes[0].id, "unowned");
        assert_eq!(
            (
                messages[2].twitch_emotes[0].start,
                messages[2].twitch_emotes[0].end
            ),
            (2, 7)
        );
        assert!(messages.iter().all(|message| message.flags.is_history));
    }

    #[test]
    fn remote_history_detects_name_mentions_after_replacement() {
        use crust_core::ignores::IgnoredPhraseAction;

        let rules = [
            phrase_rule("@alice", IgnoredPhraseAction::Replace, "@other"),
            phrase_rule("welcome", IgnoredPhraseAction::Replace, "@alice"),
        ];
        let (messages, _) = parse_history_records(
            &lines(&[
                "@id=removed;tmi-sent-ts=1000 :bob!user@host PRIVMSG #room :hello @alice",
                "@id=created;tmi-sent-ts=1001 :bob!user@host PRIVMSG #room :welcome",
            ]),
            Some("alice"),
            &[],
            &ChannelId::new("room"),
            &[],
            &rules,
        );
        assert_eq!(messages[0].raw_text, "hello @other");
        assert!(!messages[0].flags.is_mention);
        assert_eq!(messages[1].raw_text, "@alice");
        assert!(messages[1].flags.is_mention);
    }

    #[test]
    fn filtered_history_still_publishes_its_deletion_records() {
        let channel = ChannelId::new("room");
        let (messages, actions) = parse_history_records(
            &lines(&[
                "@id=blocked;rm-deleted=1;tmi-sent-ts=1000 :bob!user@host PRIVMSG #room :spam",
            ]),
            None,
            &[],
            &channel,
            &[],
            &[IgnoredPhrase::new("spam")],
        );
        assert!(messages.is_empty());
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].channel, channel);
        assert_eq!(
            actions[0].target,
            crust_core::model::ModerationTarget::Message("blocked".into())
        );
        assert!(actions[0].timestamp.is_none());
    }

    #[test]
    fn stored_cutoffs_cover_deleted_flags_without_creating_more_tombstones() {
        let channel = ChannelId::new("room");
        let saved = vec![ModerationAction::channel(
            channel.clone(),
            chrono::DateTime::from_timestamp_millis(1000),
        )];
        let (messages, actions) = parse_history_records(
            &lines(&[
                "@id=old;rm-deleted=1;tmi-sent-ts=900 :alice!user@host PRIVMSG #room :old text",
            ]),
            None,
            &[],
            &channel,
            &saved,
            &[],
        );
        assert!(messages[0].flags.is_deleted);
        assert!(actions.is_empty());
    }

    #[test]
    fn history_clears_use_source_identity_channel_and_time() {
        let (messages, actions) = parse_history_records(&lines(&[
            "@id=old;user-id=1;tmi-sent-ts=900 :alice!user@host PRIVMSG #room :old text",
            "@id=new;user-id=1;tmi-sent-ts=1100 :alice!user@host PRIVMSG #room :new text",
            "@id=reused-login;user-id=2;tmi-sent-ts=900 :alice!user@host PRIVMSG #room :another user",
            "@target-user-id=1;tmi-sent-ts=1000 :tmi.twitch.tv CLEARCHAT #room :alice",
            "@id=late;user-id=1;tmi-sent-ts=950 :alice!user@host PRIVMSG #room :late old text",
            "@id=old;user-id=1;tmi-sent-ts=900 :alice!user@host PRIVMSG #elsewhere :another channel",
            "@target-msg-id=unrelated :tmi.twitch.tv CLEARMSG #elsewhere :ignored",
        ]), None, &[], &ChannelId::new("room"), &[], &[]);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.flags.is_deleted)
                .collect::<Vec<_>>(),
            [true, false, false, true]
        );
        assert_eq!(messages[0].raw_text, "old text");
        assert_eq!(
            actions.len(),
            1,
            "the cutoff must not turn into redundant per-row tombstones"
        );
        assert!(matches!(actions[0].target, ModerationTarget::User { .. }));
    }

    #[test]
    fn untimed_history_clears_publish_only_preceding_message_identities() {
        let (messages, actions) = parse_history_records(&lines(&[
            "@id=old;user-id=1;tmi-sent-ts=900 :alice!user@host PRIVMSG #room :old text",
            "@tmi-sent-ts=invalid :tmi.twitch.tv CLEARCHAT #room :alice",
            "@id=new;user-id=1;tmi-sent-ts=800 :alice!user@host PRIVMSG #room :new text",
            "@target-msg-id=explicit :tmi.twitch.tv CLEARMSG #room :removed text",
            "@id=explicit;user-id=2;tmi-sent-ts=1100 :bob!user@host PRIVMSG #room :removed text",
            "@id=flagged;rm-deleted=1;tmi-sent-ts=1200 :bob!user@host PRIVMSG #room :flagged text",
            "@id=broken",
        ]), None, &[], &ChannelId::new("room"), &[], &[]);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.flags.is_deleted)
                .collect::<Vec<_>>(),
            [true, false, true, true]
        );
        assert_eq!(actions.len(), 3);
        assert!(actions
            .iter()
            .all(|action| matches!(action.target, ModerationTarget::Message(_))));
    }

    #[test]
    fn remote_history_mentions_honor_nickname_scope_and_boundaries() {
        let channel = ChannelId::new("room");
        let mut nickname = Nickname::new("alice", "Local name");
        nickname.channel = Some("ROOM".into());
        let raw = lines(&[
            "@id=alias;tmi-sent-ts=1000 :bob!user@host PRIVMSG #room :hello @Local name!",
            "@id=longer;tmi-sent-ts=1001 :bob!user@host PRIVMSG #room :hello @Local names!",
        ]);
        let parse = |nicknames: &[Nickname], account| {
            parse_history_records(&raw, account, nicknames, &channel, &[], &[]).0
        };
        assert_eq!(
            parse(&[nickname.clone()], Some("ALICE"))
                .iter()
                .map(|message| message.flags.is_mention)
                .collect::<Vec<_>>(),
            [true, false]
        );
        assert!(
            !parse(&[nickname.clone()], Some("carol"))[0]
                .flags
                .is_mention
        );
        assert!(!parse(&[nickname.clone()], None)[0].flags.is_mention);
        nickname.channel = Some("elsewhere".into());
        assert!(
            !parse(&[nickname.clone()], Some("alice"))[0]
                .flags
                .is_mention
        );
        nickname.channel = None;
        assert!(
            parse(&[nickname.clone()], Some("alice"))[0]
                .flags
                .is_mention
        );
        nickname.replace_mentions = false;
        assert!(!parse(&[nickname], Some("alice"))[0].flags.is_mention);
    }

    #[test]
    fn remote_history_mentions_preserve_account_names_and_reply_targets() {
        let (messages, _) = parse_history_records(&lines(&[
            "@id=name;tmi-sent-ts=1000 :bob!user@host PRIVMSG #room :hello @ALICE!",
            "@id=longer;tmi-sent-ts=1001 :bob!user@host PRIVMSG #room :hello @alice_other!",
            "@id=reply;reply-parent-msg-id=parent;reply-parent-user-login=ALICE;reply-parent-display-name=Alice;reply-parent-msg-body=question;tmi-sent-ts=1002 :bob!user@host PRIVMSG #room :an answer",
            "@id=other;reply-parent-msg-id=parent;reply-parent-user-login=alice_other;reply-parent-display-name=Other;reply-parent-msg-body=question;tmi-sent-ts=1003 :bob!user@host PRIVMSG #room :another answer",
        ]), Some("alice"), &[], &ChannelId::new("room"), &[], &[]);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.flags.is_mention)
                .collect::<Vec<_>>(),
            [true, false, true, false]
        );
    }
}

/// Fetch recent messages for a channel and send `AppEvent::HistoryLoaded`.
/// Primary source: recent-messages.robotty.de (covers all channels, correct
/// path uses a hyphen: /recent-messages/).  Fallback: logs.ivr.fi (large
/// channels only, returns objects with a "raw" IRC line field, newest-first).
pub(crate) async fn load_recent_messages(
    channel: &str,
    local_nick: Option<&str>,
    nicknames: &[Nickname],
    ignored_phrases: &[IgnoredPhrase],
    emote_index: &crate::EmoteIndex,
    badge_map: &BadgeMap,
    evt_tx: &mpsc::Sender<AppEvent>,
    stv_update_tx: &mpsc::Sender<SevenTvCosmeticUpdate>,
    log_store: Option<LogStore>,
    log_writer: Option<LogWriter>,
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
                local: None,
            })
            .await;
        return;
    }

    let raw_line_count = raw_lines.len();

    // Snapshot shared state once before the parse loop.
    let emote_snapshot = {
        let guard = emote_index.read().unwrap();
        guard.snapshot_for(&channel_id)
    };
    let badge_snapshot: HashMap<(String, String, String), String> = {
        let bm = badge_map.read().unwrap();
        bm.clone()
    };
    let local_nick_owned = local_nick.map(str::to_owned);
    let nicknames = nicknames.to_vec();
    let ignored_phrases = ignored_phrases.to_vec();
    let channel_scope = ch.to_owned();
    let query_channel = channel_id.clone();
    flush_before_read(log_writer.as_ref()).await;

    // Move parse + tokenize CPU work off async executors.
    let (messages, actions) = tokio::task::spawn_blocking(move || {
        let saved_actions = log_store
            .map(|store| store.saved_moderation(&query_channel))
            .transpose()
            .unwrap_or_else(|error| {
                warn!(
                    "chat-history: could not load saved moderation for {}: {error}",
                    query_channel.as_str()
                );
                None
            })
            .unwrap_or_default();
        let (mut messages, actions) = parse_history_records(
            &raw_lines,
            local_nick_owned.as_deref(),
            &nicknames,
            &query_channel,
            &saved_actions,
            &ignored_phrases,
        );
        for msg in &mut messages {
            // Tokenize spans
            msg.spans = crust_core::format::tokenize_with_flags(
                &msg.raw_text,
                msg.flags.is_action,
                &msg.twitch_emotes,
                &|code| {
                    crate::resolve_emote(&emote_snapshot, &msg.channel, &msg.sender.user_id.0, code)
                        .map(|info| {
                            (
                                info.id.clone(),
                                info.code.clone(),
                                info.url_1x.clone(),
                                info.provider.clone(),
                                info.url_4x.clone().or_else(|| info.url_2x.clone()),
                                info.zero_width,
                            )
                        })
                },
            );

            // Resolve badge URLs from the snapshot (no lock needed)
            for badge in &mut msg.sender.badges {
                badge.url =
                    resolve_badge_url(&badge_snapshot, &channel_scope, &badge.name, &badge.version);
            }
        }
        (messages, actions)
    })
    .await
    .unwrap_or_default();

    publish_history_moderation(channel_id.clone(), actions, log_writer.as_ref(), evt_tx).await;

    if messages.is_empty() {
        info!("chat-history: parsed 0 PRIVMSG lines from {raw_line_count} raw lines for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages,
                local: None,
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
                    channel: channel_id.clone(),
                    user_ids: history_user_ids,
                })
                .await;
        }
    }

    let _ = evt_tx
        .send(AppEvent::HistoryLoaded {
            channel: channel_id,
            messages,
            local: None,
        })
        .await;
}
