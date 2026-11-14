use std::collections::{HashMap, HashSet};

use crust_core::{
    events::AppEvent,
    model::{ChannelId, ChatMessage, HistoryCursor, LocalHistoryMessageIdentity, LocalHistoryPageInfo, MessageId},
};
use crust_storage::LogStore;
use crust_twitch::{parse_line, parse_privmsg_irc};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::seventv::SevenTvCosmeticUpdate;

use super::badges::{resolve_badge_url, BadgeMap};

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
            AppEvent::WhisperReceived { owner_user_id, from_user_id, from_login, target_login, is_self, .. } => {
                if self.user_id.as_deref() != crate::normalize_whisper_user_id(owner_user_id)
                    || self.user_id.is_none()
                    || (*is_self && crate::normalize_whisper_user_id(from_user_id) != self.user_id.as_deref())
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
pub(crate) fn prepare_live_whisper(
    event: AppEvent,
    local_login: Option<&str>,
    local_user_id: Option<&str>,
    emote_index: &crate::EmoteIndex,
    log_store: Option<&LogStore>,
) -> Option<AppEvent> {
    let AppEvent::WhisperReceived {
        owner_user_id, from_user_id, from_login, target_login, is_self, ..
    } = &event else { return None; };
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
    emote_index.write().unwrap().queue_whisper_sender(local_user_id, from_user_id);
    if !*is_self {
        if let Some(store) = log_store {
            if let Err(error) = crate::persist_whisper_message(
                store, Some(&local_login), Some(local_user_id), &event,
            ) {
                warn!("whisper-history: failed to persist whisper: {error}");
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
    ) {
        let Some(owner) = crate::normalize_whisper_login(owner) else { return; };
        let Some(owner_user_id) = crate::normalize_whisper_user_id(owner_user_id).map(str::to_owned) else { return; };
        let identity = (owner_user_id.clone(), owner.clone());
        if self.owner.as_ref() == Some(&identity) {
            return;
        }
        self.clear();
        self.owner = Some(identity);
        self.task = Some(tokio::spawn(load_local_recent_whispers(
            log_store, Some(owner), Some(owner_user_id), evt_tx, Some(emote_index.clone()),
        )));
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
            from_user_id: if is_self { owner_user_id.to_owned() } else { String::new() },
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
        if let AppEvent::WhisperReceived { text: body, .. } = &mut event { *body = text.to_owned(); }
        crate::persist_whisper_message(store, Some(owner), Some(owner_user_id), &event).unwrap();
    }

    #[test]
    fn account_filter_rejects_old_callbacks_before_plugin_dispatch() {
        let mut account = WhisperAccount::default();
        assert!(!account.accepts(&whisper("1", "alice", false)));
        account.accepts(&AppEvent::Authenticated { username: "Alice".into(), user_id: "1".into() });
        assert!(account.accepts(&whisper("1", "ALICE", false)));
        assert!(account.accepts(&whisper("1", "alice", true)));
        account.accepts(&AppEvent::LoggedOut);
        assert!(!account.accepts(&whisper("1", "alice", false)));
        account.accepts(&AppEvent::Authenticated { username: "carol".into(), user_id: "2".into() });
        assert!(!account.accepts(&whisper("1", "alice", false)));
        assert!(!account.accepts(&whisper("1", "alice", true)));
        assert!(account.accepts(&whisper("2", "carol", false)));
        account.accepts(&AppEvent::Authenticated { username: "carol".into(), user_id: "3".into() });
        assert!(!account.accepts(&whisper("2", "carol", false)));
        assert!(!account.accepts(&whisper("2", "carol", true)));
        assert!(account.accepts(&whisper("3", "carol", false)));
    }

    #[test]
    fn whisper_failures_follow_owner_ids_before_plugin_dispatch() {
        let mut account = WhisperAccount::default();
        let failed = |owner: &str| super::super::outgoing::complete_whisper_send(
            reqwest::StatusCode::FORBIDDEN, "unavailable", owner, "alice", "bob",
            "private message", Vec::new(), None,
        );
        assert!(!account.accepts(&failed("1")));
        account.accepts(&AppEvent::Authenticated { username: "alice".into(), user_id: "1".into() });
        assert!(account.accepts(&failed("1")));
        account.accepts(&AppEvent::LoggedOut);
        assert!(!account.accepts(&failed("1")));
        account.accepts(&AppEvent::Authenticated { username: "alice".into(), user_id: "2".into() });
        assert!(!account.accepts(&failed("1")));
        assert!(!account.accepts(&failed("")));
        assert!(account.accepts(&failed("2")));
        account.accepts(&AppEvent::Authenticated { username: "renamed".into(), user_id: "2".into() });
        assert!(account.accepts(&failed("2")));
    }

    #[test]
    fn account_filter_rejects_stale_avatars_before_plugin_dispatch() {
        let mut account = WhisperAccount::default();
        let avatar = |owner: &str| AppEvent::SelfAvatarLoaded {
            account: owner.to_owned(), avatar_url: "https://example.test/avatar.png".to_owned(),
        };
        assert!(!account.accepts(&avatar("alice")));
        account.accepts(&AppEvent::Authenticated { username: "Alice".into(), user_id: "1".into() });
        assert!(account.accepts(&avatar("ALICE")));
        assert!(!account.accepts(&avatar("")));
        account.accepts(&AppEvent::Authenticated { username: "carol".into(), user_id: "2".into() });
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
            42, ChannelId("whisper:bob".into()), "unowned legacy".into(),
            chrono::Utc::now(), crust_core::model::MsgKind::Chat,
        );
        legacy.sender.login = "bob".into();
        store.append_message(&legacy).unwrap();
        legacy.channel = ChannelId("whisper:reused:bob".into());
        legacy.raw_text = "login-owned legacy".into();
        store.append_message(&legacy).unwrap();
        let (events, mut received) = mpsc::channel(8);
        for (owner, text) in [("1", "first account"), ("2", "second account")] {
            load_local_recent_whispers(store.clone(), Some("reused".into()), Some(owner.into()), events.clone(), None).await;
            let AppEvent::WhisperReceived { owner_user_id, target_login, text: actual, from_login, .. } = received.try_recv().unwrap() else {
                panic!("expected account whisper history");
            };
            assert_eq!(owner_user_id, owner);
            assert_eq!(target_login, "reused");
            assert_eq!(actual, text);
            assert_eq!(from_login, "bob");
            assert!(received.try_recv().is_err());
        }
        load_local_recent_whispers(store.clone(), None, Some("1".into()), events.clone(), None).await;
        load_local_recent_whispers(store.clone(), Some("reused".into()), None, events, None).await;
        assert!(received.try_recv().is_err());
        assert_eq!(store.recent_messages(&ChannelId("whisper:bob".into()), 10).unwrap().len(), 1);
        assert_eq!(store.recent_messages(&ChannelId("whisper:reused:bob".into()), 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn renamed_accounts_keep_history_and_replay_self_rows_by_sender_id() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        save(&store, "10", "before", "incoming before rename");
        let mut sent = whisper("10", "before", true);
        if let AppEvent::WhisperReceived { text, .. } = &mut sent { *text = "sent before rename".into(); }
        crate::persist_whisper_message(&store, Some("before"), Some("10"), &sent).unwrap();
        let channel = crate::whisper_thread_channel_id("10", "bob");
        let mut bad_self = store.recent_messages(&channel, 10).unwrap().into_iter().find(|message| message.flags.is_self).unwrap();
        bad_self.sender.user_id.0 = "99".into();
        bad_self.raw_text = "wrong sender ID".into();
        store.append_message(&bad_self).unwrap();
        bad_self.sender.user_id.0.clear();
        bad_self.raw_text = "unknown sender ID".into();
        store.append_message(&bad_self).unwrap();
        let (events, mut received) = mpsc::channel(8);
        load_local_recent_whispers(store, Some("after".into()), Some("10".into()), events, None).await;
        let mut count = 0;
        while let Ok(event) = received.try_recv() {
            let AppEvent::WhisperReceived { owner_user_id, from_user_id, from_login, target_login, text, is_self, .. } = event else {
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
                (Some("carol"), Some("1")), (Some("alice"), Some("2")),
                (Some("alice"), None), (None, Some("1")),
            ] {
                crate::persist_whisper_message(&store, local, local_id, &event).unwrap();
            }
        }
        assert!(store.recent_channels_with_prefix("whisper:", 20).unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_whisper_sender_ids_survive_storage_and_replay_for_personal_emotes() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let live_index = crate::EmoteIndex::default();
        live_index.write().unwrap().set_account(Some("10"));
        let mut live = whisper("10", "alice", false);
        if let AppEvent::WhisperReceived { from_user_id, text, is_history, .. } = &mut live {
            *from_user_id = "20".into();
            *text = "PersonalHello".into();
            *is_history = false;
        }
        let forwarded = prepare_live_whisper(live, Some("alice"), Some("10"), &live_index, Some(&store)).unwrap();
        assert!(matches!(forwarded, AppEvent::WhisperReceived { from_user_id, is_history: false, .. } if from_user_id == "20"));
        assert_eq!(live_index.write().unwrap().next_deferred_personal_load(), Some(("20".into(), None)));
        let rows = store.recent_messages(&crate::whisper_thread_channel_id("10", "bob"), 10).unwrap();
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
        load_local_recent_whispers(store, Some("alice".into()), Some("10".into()), events,
            Some(replay_index.clone())).await;
        let mut identities = HashMap::new();
        while let Ok(event) = received.try_recv() {
            let AppEvent::WhisperReceived { from_user_id, text, target_login, is_history, .. } = event else {
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
        assert_eq!(registry.next_deferred_personal_load(), Some(("20".into(), None)));
        assert!(registry.next_deferred_personal_load().is_none());
        let generation = registry.begin_personal_load("20", None).unwrap();
        registry.complete_personal_load("20", generation, Some(vec![crust_emotes::EmoteInfo {
            provider: "7tv".into(), id: "personal".into(), code: "PersonalHello".into(),
            url_1x: "https://example.test/personal.png".into(), url_2x: None, url_4x: None,
        }])).unwrap();
        assert!(registry.resolve(&ChannelId::mentions(), "20", "PersonalHello").is_some());
        assert!(registry.resolve(&ChannelId::mentions(), "10", "PersonalHello").is_none());
        assert!(registry.resolve(&ChannelId::mentions(), "", "PersonalHello").is_none());
    }

    #[test]
    fn stale_live_whispers_do_not_schedule_sender_lookups_or_history() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let index = crate::EmoteIndex::default();
        index.write().unwrap().set_account(Some("30"));
        let mut stale = whisper("10", "alice", false);
        if let AppEvent::WhisperReceived { from_user_id, .. } = &mut stale { *from_user_id = "20".into(); }
        assert!(prepare_live_whisper(stale, Some("alice"), Some("30"), &index, Some(&store)).is_none());
        assert!(index.write().unwrap().next_deferred_personal_load().is_none());
        assert!(store.recent_channels_with_prefix("whisper:", 20).unwrap().is_empty());
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
        replay.load(store.clone(), "alice", "1", &emotes, events.clone());
        let handle = replay.task.as_ref().unwrap().abort_handle();
        replay.clear();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !handle.is_finished() { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(matches!(received.try_recv().unwrap(), AppEvent::LoggedOut));
        assert!(received.try_recv().is_err());
        replay.load(store, "carol", "2", &emotes, events);
        tokio::time::timeout(std::time::Duration::from_secs(3), replay.task.take().unwrap()).await.unwrap().unwrap();
        assert!(matches!(received.try_recv().unwrap(), AppEvent::WhisperReceived { target_login, text, .. }
            if target_login == "carol" && text == "new account"));
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
    load_local_history_page(channel, None, LogStore::default_recent_limit(), log_store, evt_tx).await;
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
        log_store.history_page(&channel_for_query, before, limit)
    })
    .await;

    let page = match loaded {
        Ok(Ok(page)) => page,
        Ok(Err(e)) => {
            warn!(
                "chat-history: local older SQLite load failed for #{}: {e}",
                channel.display_name()
            );
            let _ = evt_tx.send(AppEvent::Error {
                context: "History".into(),
                message: format!("Could not load local history for #{}: {e}", channel.display_name()),
            }).await;
            return;
        }
        Err(e) => {
            warn!(
                "chat-history: local older SQLite task failed for #{}: {e}",
                channel.display_name()
            );
            let _ = evt_tx.send(AppEvent::Error {
                context: "History".into(),
                message: format!("Could not load local history for #{}: {e}", channel.display_name()),
            }).await;
            return;
        }
    };

    let mut messages = Vec::with_capacity(page.messages.len());
    let mut identities = Vec::with_capacity(page.messages.len());
    for (mut msg, row_id) in page.messages.into_iter().zip(page.row_ids) {
        let Some(replay_id) = MessageId::from_local_history_row(row_id) else {
            continue;
        };
        identities.push(LocalHistoryMessageIdentity { replay_id, original_id: msg.id });
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
            local: Some(LocalHistoryPageInfo { before, next: page.next, has_more: page.has_more, identities }),
        })
        .await;
}

#[cfg(test)]
mod local_history_identity_tests {
    use super::*;
    use crust_core::model::{ChannelState, MsgKind};

    #[tokio::test]
    async fn local_replays_keep_storage_ids_and_match_live_rows_after_reload() {
        let store = LogStore::with_db_path(std::path::PathBuf::from(":memory:")).unwrap();
        let channel = ChannelId::new("room");
        let mut room = ChannelState::new(channel.clone());
        for id in [17, 18, 19] {
            let message = super::super::system_messages::make_system_message(
                id, channel.clone(), "same text".into(),
                chrono::DateTime::from_timestamp_millis(1000).unwrap(), MsgKind::Chat,
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
            let AppEvent::HistoryLoaded { messages, local: Some(page), .. } = received.recv().await.unwrap() else {
                panic!("expected a local history page");
            };
            let ids: Vec<_> = messages.iter().map(|message| message.id).collect();
            assert!(ids.iter().all(|id| id.is_local_history()));
            assert_eq!(page.identities.iter().map(|identity| identity.original_id.0).collect::<Vec<_>>(), [17, 18, 19]);
            if let Some(previous) = &previous_ids {
                assert_eq!(&ids, previous);
            }
            previous_ids = Some(ids);
            room.prepend_local_history(messages, &page);
            assert_eq!(room.messages.len(), 3);
            assert_eq!(room.messages.iter().filter(|message| message.id == MessageId(19)).count(), 1);
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
    let Some(self_login) = self_login.as_deref().and_then(crate::normalize_whisper_login) else {
        return;
    };
    let Some(self_user_id) = self_user_id.as_deref().and_then(crate::normalize_whisper_user_id) else {
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
                .unwrap_or_default().to_owned();
            let (from_login, from_display_name) = if msg.flags.is_self {
                if from_user_id != self_user_id { continue; }
                (self_login.clone(), self_login.clone())
            } else {
                if stored_login.is_empty() || stored_login != partner_login { continue; }
                (stored_login, msg.sender.display_name)
            };
            let target_login = if msg.flags.is_self {
                partner_login.clone()
            } else {
                self_login.clone()
            };
            if let Some(index) = &emotes {
                index.write().unwrap().queue_whisper_sender(self_user_id, &from_user_id);
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
                .await.is_err()
            {
                return;
            }
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
    let channel_scope = ch.to_owned();

    // Move parse + tokenize CPU work off async executors.
    let messages = tokio::task::spawn_blocking(move || {
        let mut messages: Vec<ChatMessage> = Vec::with_capacity(raw_lines.len());

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
                    crate::resolve_emote(&emote_snapshot, &msg.channel, &msg.sender.user_id.0, code).map(|info| {
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

            messages.push(msg);
        }
        messages
    })
    .await
    .unwrap_or_default();

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
