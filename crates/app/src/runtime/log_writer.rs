use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use crust_core::events::HistoryUpdate;
use crust_core::model::ModerationAction;
use crust_core::{AppEvent, ChannelId, ChatMessage, MessageId};
use crust_storage::LogStore;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

const WRITE_QUEUE_CAPACITY: usize = 2_048;
const UI_UPDATE_QUEUE_CAPACITY: usize = 128;
const PENDING_OWN_ECHO_LIMIT: usize = 512;
// Covers both 4,096-event queues, 2,048 writer commands, 128 relay updates,
// 128 deferred UI updates, and 512 outgoing publications while the UI catches up.
const CONFIRMED_ECHO_ALIAS_LIMIT: usize = 16_384;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

enum LogWrite {
    Append(Box<ChatMessage>),
    Moderation(ModerationAction),
    ModerationBatch(Vec<ModerationAction>),
    Highlight {
        channel: ChannelId,
        server_id: Option<String>,
        message_id: MessageId,
        timestamp_ms: i64,
    },
    Confirmation {
        message: Box<ChatMessage>,
        original_timestamp_ms: i64,
    },
    Whisper {
        owner_login: String,
        owner_user_id: String,
        event: Box<AppEvent>,
    },
}

impl LogWrite {
    fn apply(self, store: &LogStore) -> Result<(), String> {
        let result = match self {
            Self::Append(message) => store.append_message(&message),
            Self::Moderation(action) => store.apply_moderation(&action),
            Self::ModerationBatch(actions) => store.apply_moderation_batch(&actions),
            Self::Highlight {
                channel,
                server_id,
                message_id,
                timestamp_ms,
            } => store.mark_highlighted(&channel, server_id.as_deref(), message_id, timestamp_ms),
            Self::Confirmation {
                message,
                original_timestamp_ms,
            } => store
                .confirm_message(&message, original_timestamp_ms)
                .map(|_| ()),
            Self::Whisper {
                owner_login,
                owner_user_id,
                event,
            } => crate::persist_whisper_message(
                store,
                Some(&owner_login),
                Some(&owner_user_id),
                &event,
            ),
        };
        result.map_err(|error| error.to_string())
    }
}

type LocalEchoIdentity = (ChannelId, MessageId, i64);

#[derive(Default)]
struct OwnEchoWrites {
    pending: VecDeque<Box<ChatMessage>>,
    matching_disabled: bool,
    confirmed: HashMap<LocalEchoIdentity, String>,
    confirmed_order: VecDeque<LocalEchoIdentity>,
}

impl OwnEchoWrites {
    fn apply(&mut self, write: LogWrite, store: &LogStore) -> Result<(), String> {
        match write {
            LogWrite::Append(message) => self
                .append(message, store)
                .map_err(|error| error.to_string()),
            LogWrite::Highlight {
                channel,
                server_id,
                message_id,
                timestamp_ms,
            } => {
                let server_id = server_id.or_else(|| {
                    self.confirmed
                        .get(&(channel.clone(), message_id, timestamp_ms))
                        .cloned()
                });
                LogWrite::Highlight {
                    channel,
                    server_id,
                    message_id,
                    timestamp_ms,
                }
                .apply(store)
            }
            LogWrite::Confirmation {
                message,
                original_timestamp_ms,
            } => {
                if store
                    .confirm_message(&message, original_timestamp_ms)
                    .map_err(|error| error.to_string())?
                {
                    self.pending.retain(|local| {
                        local.channel != message.channel
                            || local.id != message.id
                            || local.timestamp.timestamp_millis() != original_timestamp_ms
                    });
                    self.confirmed.remove(&(
                        message.channel.clone(),
                        message.id,
                        original_timestamp_ms,
                    ));
                }
                Ok(())
            }
            write => write.apply(store),
        }
    }

    fn append(
        &mut self,
        message: Box<ChatMessage>,
        store: &LogStore,
    ) -> Result<(), crust_storage::StorageError> {
        if !self.matching_disabled && message.flags.is_self && !message.flags.is_history {
            if let Some(server_id) = message.server_id.as_ref().filter(|id| !id.is_empty()) {
                // A repeated server delivery must not consume the next local
                // send when several legitimate sends have identical text.
                if !store.has_server_message(&message.channel, server_id)? {
                    if let Some(index) = self
                        .pending
                        .iter()
                        .position(|local| local.is_pending_own_echo_of(&message))
                    {
                        let local = &self.pending[index];
                        let identity = (
                            local.channel.clone(),
                            local.id,
                            local.timestamp.timestamp_millis(),
                        );
                        let mut confirmed = (*message).clone();
                        confirmed.id = local.id;
                        if !store.confirm_message(&confirmed, identity.2)? {
                            store.append_message(&message)?;
                        }
                        self.pending.remove(index);
                        self.remember_confirmation(identity, server_id.clone());
                        return Ok(());
                    }
                }
            }
        }
        store.append_message(&message)?;
        if !self.matching_disabled
            && message.flags.is_self
            && !message.flags.is_history
            && message.server_id.is_none()
        {
            if self.pending.len() == PENDING_OWN_ECHO_LIMIT {
                // A truncated prefix could assign an old confirmation to a
                // newer repeated send. Keep exact UI confirmations available,
                // but do not resume automatic matching within this session.
                self.pending.clear();
                self.matching_disabled = true;
                warn!("chat-history: automatic own-message matching paused after too many unconfirmed sends");
            } else {
                self.pending.push_back(message);
            }
        }
        Ok(())
    }

    fn remember_confirmation(&mut self, identity: LocalEchoIdentity, server_id: String) {
        if self.confirmed.insert(identity.clone(), server_id).is_none() {
            self.confirmed_order.push_back(identity);
        }
        while self.confirmed_order.len() > CONFIRMED_ECHO_ALIAS_LIMIT {
            if let Some(oldest) = self.confirmed_order.pop_front() {
                self.confirmed.remove(&oldest);
            }
        }
    }
}

enum Command {
    Write(LogWrite),
    Flush(oneshot::Sender<Result<(), String>>),
    Shutdown(oneshot::Sender<Result<(), String>>),
}

/// One ordered writer keeps disk access off the reducer. Backpressure limits
/// retained messages without dropping history or racing highlight updates.
#[derive(Clone)]
pub(crate) struct LogWriter {
    commands: mpsc::Sender<Command>,
    ui_updates: mpsc::Sender<HistoryUpdate>,
}

impl LogWriter {
    pub(crate) fn new(
        runtime: &tokio::runtime::Handle,
        store: LogStore,
        events: mpsc::Sender<AppEvent>,
    ) -> Self {
        let mut own_echoes = OwnEchoWrites::default();
        Self::start(runtime, events, WRITE_QUEUE_CAPACITY, move |write| {
            own_echoes.apply(write, &store)
        })
    }

    fn start(
        runtime: &tokio::runtime::Handle,
        events: mpsc::Sender<AppEvent>,
        capacity: usize,
        mut apply: impl FnMut(LogWrite) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let (commands, mut received) = mpsc::channel(capacity);
        runtime.spawn_blocking(move || {
            let mut pending_error = None;
            let mut reported_failure = false;
            let mut shutdown = None;
            while let Some(command) = received.blocking_recv() {
                match command {
                    Command::Write(write) => match apply(write) {
                        Ok(()) => reported_failure = false,
                        Err(error) => {
                            warn!("chat-history: background write failed: {error}");
                            if pending_error.is_none() {
                                pending_error = Some(error.clone());
                            }
                            if !reported_failure {
                                // A full UI queue must not stop the writer while
                                // the reducer is waiting for write capacity.
                                reported_failure = match events.try_send(AppEvent::Error {
                                    context: "Chat history".into(),
                                    message: format!("Could not save chat history: {error}"),
                                }) {
                                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => true,
                                    Err(mpsc::error::TrySendError::Full(_)) => false,
                                };
                            }
                        }
                    },
                    Command::Flush(done) => {
                        let _ = done.send(pending_error.take().map_or(Ok(()), Err));
                    }
                    Command::Shutdown(done) => {
                        received.close();
                        if shutdown.is_none() {
                            shutdown = Some(done);
                        } else {
                            let _ =
                                done.send(Err("Chat history writer is already stopping.".into()));
                        }
                    }
                }
            }
            if let Some(done) = shutdown {
                let _ = done.send(pending_error.map_or(Ok(()), Err));
            }
        });
        let (ui_updates, mut received_updates) = mpsc::channel(UI_UPDATE_QUEUE_CAPACITY);
        let update_commands = commands.clone();
        runtime.spawn(async move {
            let mut pending_error = None;
            let mut shutdown = None;
            while let Some(update) = received_updates.recv().await {
                let write = match update {
                    HistoryUpdate::Highlight {
                        channel,
                        server_id,
                        message_id,
                        timestamp_ms,
                    } => LogWrite::Highlight {
                        channel,
                        server_id,
                        message_id,
                        timestamp_ms,
                    },
                    HistoryUpdate::Confirmation {
                        message,
                        original_timestamp_ms,
                    } => LogWrite::Confirmation {
                        message,
                        original_timestamp_ms,
                    },
                    HistoryUpdate::Flush(done) => {
                        let _ = done.send(pending_error.clone().map_or(Ok(()), Err));
                        continue;
                    }
                    HistoryUpdate::Shutdown(done) => {
                        received_updates.close();
                        if shutdown.is_none() {
                            shutdown = Some(done);
                        } else {
                            let _ =
                                done.send(Err("Chat history updates are already stopping.".into()));
                        }
                        continue;
                    }
                };
                if pending_error.is_none()
                    && update_commands.send(Command::Write(write)).await.is_err()
                {
                    // Stop accepting UI writes after the writer closes. Drain
                    // accepted barriers so their callers receive the failure.
                    received_updates.close();
                    pending_error =
                        Some("Chat history writer stopped before applying UI updates.".to_owned());
                }
            }
            if let Some(done) = shutdown {
                let _ = done.send(pending_error.map_or(Ok(()), Err));
            }
        });
        Self {
            commands,
            ui_updates,
        }
    }

    pub(crate) fn update_sender(&self) -> mpsc::Sender<HistoryUpdate> {
        self.ui_updates.clone()
    }

    async fn finish_ui_updates(&self, shutdown: bool) -> Result<(), String> {
        let (done, completed) = oneshot::channel();
        let update = if shutdown {
            HistoryUpdate::Shutdown(done)
        } else {
            HistoryUpdate::Flush(done)
        };
        self.ui_updates
            .send(update)
            .await
            .map_err(|_| "Chat history update queue has stopped.".to_owned())?;
        completed.await.map_err(|_| {
            "Chat history update queue stopped before forwarding pending updates.".to_owned()
        })?
    }

    async fn enqueue(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .await
            .map_err(|_| "Chat history writer has stopped.".to_owned())
    }

    pub(crate) async fn append(&self, message: &ChatMessage) -> Result<(), String> {
        if message.flags.is_history {
            return Ok(());
        }
        self.enqueue(Command::Write(LogWrite::Append(Box::new(message.clone()))))
            .await
    }

    pub(crate) async fn moderate(&self, action: ModerationAction) -> Result<(), String> {
        self.enqueue(Command::Write(LogWrite::Moderation(action)))
            .await
    }

    async fn moderate_history(&self, actions: Vec<ModerationAction>) -> Result<(), String> {
        self.enqueue(Command::Write(LogWrite::ModerationBatch(actions)))
            .await
    }

    #[cfg(test)]
    pub(crate) async fn mark_highlighted(
        &self,
        channel: ChannelId,
        server_id: Option<String>,
        message_id: MessageId,
        timestamp_ms: i64,
    ) -> Result<(), String> {
        self.enqueue(Command::Write(LogWrite::Highlight {
            channel,
            server_id,
            message_id,
            timestamp_ms,
        }))
        .await
    }

    pub(crate) async fn whisper(
        &self,
        owner_login: String,
        owner_user_id: String,
        event: AppEvent,
    ) -> Result<(), String> {
        self.enqueue(Command::Write(LogWrite::Whisper {
            owner_login,
            owner_user_id,
            event: Box::new(event),
        }))
        .await
    }

    pub(crate) async fn flush(&self) -> Result<(), String> {
        self.finish_ui_updates(false).await?;
        let (done, completed) = oneshot::channel();
        self.enqueue(Command::Flush(done)).await?;
        completed
            .await
            .map_err(|_| "Chat history writer stopped before saving queued messages.".to_owned())?
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT).await
    }

    async fn shutdown_with_timeout(&self, timeout: Duration) -> Result<(), String> {
        tokio::time::timeout(timeout, async {
            self.finish_ui_updates(true).await?;
            let (done, completed) = oneshot::channel();
            self.enqueue(Command::Shutdown(done)).await?;
            completed
                .await
                .map_err(|_| "Chat history writer stopped before finishing shutdown.".to_owned())?
        })
        .await
        .map_err(|_| "Timed out waiting for queued chat history to finish saving.".to_owned())?
    }
}

pub(crate) async fn flush_before_read(writer: Option<&LogWriter>) {
    if let Some(writer) = writer {
        if let Err(error) = writer.flush().await {
            warn!("chat-history: could not flush writes before reading saved messages: {error}");
        }
    }
}

/// Queue the durable change before publication, preserving the reducer's
/// message/action order while a history reader waits at its flush barrier.
pub(crate) async fn publish_moderation(
    event: AppEvent,
    writer: Option<&LogWriter>,
    events: &mpsc::Sender<AppEvent>,
) {
    if let (Some(writer), Some(action)) = (writer, event.moderation_action()) {
        if let Err(error) = writer.moderate(action).await {
            warn!("chat-history: failed to queue moderation: {error}");
        }
    }
    let _ = events.send(event).await;
}

pub(crate) async fn publish_history_moderation(
    channel: ChannelId,
    actions: Vec<ModerationAction>,
    writer: Option<&LogWriter>,
    events: &mpsc::Sender<AppEvent>,
) {
    if actions.is_empty() {
        return;
    }
    if let Some(writer) = writer {
        if let Err(error) = writer.moderate_history(actions.clone()).await {
            warn!("chat-history: failed to queue historical moderation: {error}");
        }
    }
    let _ = events
        .send(AppEvent::HistoryModerationLoaded { channel, actions })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn message(id: u64) -> ChatMessage {
        super::super::system_messages::make_system_message(
            id,
            ChannelId::new("room"),
            format!("message {id}"),
            chrono::Utc::now(),
            crust_core::model::MsgKind::Chat,
        )
    }

    fn highlight_update(message: &ChatMessage) -> HistoryUpdate {
        HistoryUpdate::Highlight {
            channel: message.channel.clone(),
            server_id: message.server_id.clone(),
            message_id: message.id,
            timestamp_ms: message.timestamp.timestamp_millis(),
        }
    }

    #[tokio::test]
    async fn bounded_queue_waits_for_capacity_and_preserves_write_order() {
        let (events, _received) = mpsc::channel(4);
        let (entered, started) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let mut entered = Some(entered);
        let applied = Arc::new(Mutex::new(Vec::new()));
        let observed = applied.clone();
        let writer = LogWriter::start(
            &tokio::runtime::Handle::current(),
            events,
            1,
            move |write| {
                let LogWrite::Append(message) = write else {
                    panic!("expected append")
                };
                observed.lock().unwrap().push(message.id.0);
                if let Some(entered) = entered.take() {
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                Ok(())
            },
        );
        writer.append(&message(1)).await.unwrap();
        started.await.unwrap();
        writer.append(&message(2)).await.unwrap();
        let third_message = message(3);
        let third = writer.append(&third_message);
        tokio::pin!(third);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut third)
            .await
            .is_err());
        release.send(()).unwrap();
        third.await.unwrap();
        writer.shutdown().await.unwrap();
        assert_eq!(*applied.lock().unwrap(), [1, 2, 3]);
        assert!(writer.append(&message(4)).await.is_err());
    }

    #[tokio::test]
    async fn highlights_follow_inserts_and_flush_makes_history_readable() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let row = message(1);
        writer.append(&row).await.unwrap();
        writer
            .mark_highlighted(
                row.channel.clone(),
                row.server_id.clone(),
                row.id,
                row.timestamp.timestamp_millis(),
            )
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let saved = store.recent_mentions(10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, row.id);
        assert!(saved[0].flags.is_highlighted);
        let mut replay = message(2);
        replay.flags.is_history = true;
        writer.append(&replay).await.unwrap();
        writer.shutdown().await.unwrap();
        assert_eq!(store.recent_messages(&row.channel, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_writes_reach_the_ui_and_flush_before_later_work_recovers() {
        let (events, mut received) = mpsc::channel(4);
        let writer = LogWriter::start(&tokio::runtime::Handle::current(), events, 4, |write| {
            let LogWrite::Append(message) = write else {
                panic!("expected append")
            };
            if message.id.0 < 3 {
                Err("test disk failure".into())
            } else {
                Ok(())
            }
        });
        for id in 1..=2 {
            writer.append(&message(id)).await.unwrap();
        }
        assert_eq!(writer.flush().await.unwrap_err(), "test disk failure");
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::Error { context, message }
            if context == "Chat history" && message.contains("test disk failure"))
        );
        assert!(received.try_recv().is_err());
        writer.append(&message(3)).await.unwrap();
        writer.flush().await.unwrap();
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_write_notification_retries_after_a_full_ui_queue_drains() {
        let (events, mut received) = mpsc::channel(1);
        events.send(AppEvent::LoggedOut).await.unwrap();
        let writer = LogWriter::start(&tokio::runtime::Handle::current(), events, 2, |_| {
            Err("test disk failure".into())
        });
        writer.append(&message(1)).await.unwrap();
        assert!(writer.flush().await.is_err());
        assert!(matches!(received.try_recv().unwrap(), AppEvent::LoggedOut));
        writer.append(&message(2)).await.unwrap();
        assert!(writer.flush().await.is_err());
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::Error { message, .. }
            if message.contains("test disk failure"))
        );
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn moderation_and_highlights_follow_the_same_ordered_write_stream() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, mut received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        let mut row = message(1);
        row.server_id = Some("deleted-before-arrival".into());
        publish_moderation(
            AppEvent::MessageDeleted {
                channel: row.channel.clone(),
                server_id: row.server_id.clone().unwrap(),
            },
            Some(&writer),
            &events,
        )
        .await;
        assert!(matches!(
            received.try_recv().unwrap(),
            AppEvent::MessageDeleted { .. }
        ));
        writer.append(&row).await.unwrap();
        writer
            .mark_highlighted(
                row.channel.clone(),
                row.server_id.clone(),
                row.id,
                row.timestamp.timestamp_millis(),
            )
            .await
            .unwrap();
        let mut newer = message(2);
        newer.server_id = Some("newer".into());
        writer.append(&newer).await.unwrap();
        writer.flush().await.unwrap();
        let rows = store.recent_messages(&row.channel, 10).unwrap();
        assert!(rows[0].flags.is_deleted && rows[0].flags.is_highlighted);
        assert_eq!(rows[0].raw_text, row.raw_text);
        assert!(!rows[1].flags.is_deleted);
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn repeated_server_deliveries_do_not_consume_another_identical_send() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut first = message(1);
        first.server_id = None;
        first.flags.is_self = true;
        let mut second = first.clone();
        second.id = MessageId(2);
        second.timestamp += chrono::Duration::milliseconds(1);
        writer.append(&first).await.unwrap();
        writer.append(&second).await.unwrap();
        let mut server = first.clone();
        server.id = MessageId(100);
        server.server_id = Some("first-confirmed".into());
        server.timestamp += chrono::Duration::seconds(1);
        writer.append(&server).await.unwrap();
        writer.append(&server).await.unwrap();
        server.id = MessageId(101);
        server.server_id = Some("second-confirmed".into());
        server.timestamp += chrono::Duration::milliseconds(1);
        writer.append(&server).await.unwrap();
        writer.shutdown().await.unwrap();
        let rows = store.recent_messages(&first.channel, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, first.id);
        assert_eq!(rows[0].server_id.as_deref(), Some("first-confirmed"));
        assert_eq!(rows[1].id, second.id);
        assert_eq!(rows[1].server_id.as_deref(), Some("second-confirmed"));
    }

    #[tokio::test]
    async fn shared_mirrors_do_not_consume_or_delete_identical_local_sends() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut local = message(1);
        local.server_id = None;
        local.flags.is_self = true;
        writer.append(&local).await.unwrap();

        let mut mirror = local.clone();
        mirror.id = MessageId(100);
        mirror.server_id = Some("mirrored-message".into());
        mirror.timestamp += chrono::Duration::seconds(1);
        mirror.shared = Some(crust_core::model::SharedChatSource {
            room_id: "other-room".into(),
            source_message_id: Some("source-message".into()),
            ..Default::default()
        });
        writer.append(&mirror).await.unwrap();
        writer.append(&mirror).await.unwrap();
        let mut server = local.clone();
        server.id = MessageId(101);
        server.server_id = Some("local-confirmed".into());
        server.timestamp += chrono::Duration::seconds(2);
        server.flags.is_first_msg = true;
        writer.append(&server).await.unwrap();
        writer.moderate(ModerationAction::message(
            local.channel.clone(), "mirrored-message".into(),
        )).await.unwrap();
        writer.shutdown().await.unwrap();

        let rows = store.recent_messages(&local.channel, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let mirrored = rows.iter().find(|row| row.server_id == mirror.server_id).unwrap();
        assert_eq!(mirrored.id, mirror.id);
        assert!(mirrored.flags.is_deleted);
        assert_eq!(mirrored.shared.as_ref().unwrap().room_id, "other-room");
        let confirmed = rows.iter().find(|row| row.server_id == server.server_id).unwrap();
        assert_eq!(confirmed.id, local.id);
        assert!(confirmed.shared.is_none());
        assert!(!confirmed.flags.is_deleted);
        assert!(confirmed.flags.is_first_msg);
    }

    #[tokio::test]
    async fn successful_ui_confirmation_retires_its_exact_pending_candidate() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut first = message(1);
        first.server_id = None;
        first.flags.is_self = true;
        let mut second = first.clone();
        second.id = MessageId(2);
        second.timestamp += chrono::Duration::milliseconds(1);
        writer.append(&first).await.unwrap();
        writer.append(&second).await.unwrap();
        let mut confirmed = first.clone();
        confirmed.server_id = Some("recovered-confirmation".into());
        confirmed.timestamp += chrono::Duration::seconds(1);
        // The exact UI path can recover an earlier failed automatic write.
        writer
            .update_sender()
            .send(HistoryUpdate::Confirmation {
                message: Box::new(confirmed),
                original_timestamp_ms: first.timestamp.timestamp_millis(),
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let mut server = second.clone();
        server.id = MessageId(100);
        server.server_id = Some("next-confirmation".into());
        server.timestamp += chrono::Duration::seconds(1);
        writer.append(&server).await.unwrap();
        writer.shutdown().await.unwrap();
        let rows = store.recent_messages(&first.channel, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, first.id);
        assert_eq!(rows[0].server_id.as_deref(), Some("recovered-confirmation"));
        assert_eq!(rows[1].id, second.id);
        assert_eq!(rows[1].server_id.as_deref(), Some("next-confirmation"));
    }

    #[tokio::test]
    async fn delayed_ui_highlights_and_enrichment_preserve_early_confirmation() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut local = message(1);
        local.server_id = None;
        local.flags.is_self = true;
        let mut other = local.clone();
        other.channel = ChannelId::new("other");
        writer.append(&local).await.unwrap();
        writer.append(&other).await.unwrap();
        let mut server = local.clone();
        server.id = MessageId(100);
        server.server_id = Some("early-confirmed".into());
        server.timestamp += chrono::Duration::seconds(1);
        writer.append(&server).await.unwrap();
        writer.flush().await.unwrap();
        let saved = store.recent_messages(&local.channel, 10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].timestamp, server.timestamp);
        assert!(!saved[0].flags.is_highlighted);
        writer
            .moderate(ModerationAction::message(
                local.channel.clone(),
                "early-confirmed".into(),
            ))
            .await
            .unwrap();
        let updates = writer.update_sender();
        updates.try_send(highlight_update(&local)).unwrap();
        server.id = local.id;
        server.sender.display_name = "Updated display name".into();
        updates
            .try_send(HistoryUpdate::Confirmation {
                message: Box::new(server.clone()),
                original_timestamp_ms: local.timestamp.timestamp_millis(),
            })
            .unwrap();
        writer.shutdown().await.unwrap();
        let saved = store.recent_messages(&local.channel, 10).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, local.id);
        assert_eq!(saved[0].timestamp, server.timestamp);
        assert_eq!(saved[0].sender.display_name, "Updated display name");
        assert!(saved[0].flags.is_highlighted && saved[0].flags.is_deleted);
        let unrelated = store.recent_messages(&other.channel, 10).unwrap();
        assert_eq!(unrelated.len(), 1);
        assert!(unrelated[0].server_id.is_none());
        assert!(!unrelated[0].flags.is_highlighted && !unrelated[0].flags.is_deleted);
    }

    #[tokio::test]
    async fn candidate_overflow_never_assigns_an_old_confirmation_to_a_newer_send() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut first = message(1);
        first.server_id = None;
        first.flags.is_self = true;
        for index in 0..=PENDING_OWN_ECHO_LIMIT {
            let mut local = first.clone();
            local.id = MessageId(index as u64 + 1);
            local.timestamp += chrono::Duration::milliseconds(index as i64);
            writer.append(&local).await.unwrap();
        }
        let mut server = first.clone();
        server.id = MessageId(10_000);
        server.server_id = Some("oldest-confirmation".into());
        server.timestamp += chrono::Duration::seconds(2);
        writer.append(&server).await.unwrap();
        writer.flush().await.unwrap();
        let rows = store.recent_messages(&first.channel, 1_000).unwrap();
        assert_eq!(rows.len(), PENDING_OWN_ECHO_LIMIT + 2);
        assert_eq!(
            rows.iter().filter(|row| row.server_id.is_none()).count(),
            PENDING_OWN_ECHO_LIMIT + 1
        );
        assert_eq!(
            rows.iter().find(|row| row.server_id.is_some()).unwrap().id,
            server.id
        );
        // The UI still supplies an exact original identity after overflow.
        server.id = first.id;
        writer
            .update_sender()
            .send(HistoryUpdate::Confirmation {
                message: Box::new(server),
                original_timestamp_ms: first.timestamp.timestamp_millis(),
            })
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let rows = store.recent_messages(&first.channel, 1_000).unwrap();
        assert_eq!(rows.len(), PENDING_OWN_ECHO_LIMIT + 1);
        assert_eq!(
            rows.iter().find(|row| row.server_id.is_some()).unwrap().id,
            first.id
        );
        assert_eq!(
            rows.iter().filter(|row| row.server_id.is_none()).count(),
            PENDING_OWN_ECHO_LIMIT
        );
    }

    #[tokio::test]
    async fn automatic_confirmation_does_not_match_saved_rows_from_an_earlier_session() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let mut old = message(1);
        old.server_id = None;
        old.flags.is_self = true;
        store.append_message(&old).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut incoming = old.clone();
        incoming.id = MessageId(100);
        incoming.server_id = Some("untracked-self-message".into());
        writer.append(&incoming).await.unwrap();
        writer.shutdown().await.unwrap();
        let rows = store.recent_messages(&old.channel, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|row| row.id == old.id && row.server_id.is_none()));
        assert!(rows
            .iter()
            .any(|row| row.id == incoming.id && row.server_id == incoming.server_id));
    }

    #[tokio::test]
    async fn ui_confirmation_updates_finish_before_history_reads() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut local = message(1);
        local.server_id = None;
        local.flags.is_self = true;
        let timestamp_ms = local.timestamp.timestamp_millis();
        writer.append(&local).await.unwrap();
        let updates = writer.update_sender();
        updates.try_send(highlight_update(&local)).unwrap();
        let mut server = local.clone();
        server.id = MessageId(2);
        server.server_id = Some("confirmed".into());
        writer.append(&server).await.unwrap();
        writer
            .moderate(ModerationAction::message(
                local.channel.clone(),
                "confirmed".into(),
            ))
            .await
            .unwrap();
        server.id = local.id;
        updates
            .try_send(HistoryUpdate::Confirmation {
                message: Box::new(server),
                original_timestamp_ms: timestamp_ms,
            })
            .unwrap();
        // No task has polled the relay yet on this current-thread runtime.
        assert_eq!(updates.capacity(), UI_UPDATE_QUEUE_CAPACITY - 2);
        writer.flush().await.unwrap();
        let rows = store.recent_messages(&local.channel, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, local.id);
        assert!(rows[0].flags.is_highlighted && rows[0].flags.is_deleted);
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn saturated_ui_updates_progress_without_draining_ui_events() {
        let (events, mut received) = mpsc::channel(1);
        events.try_send(AppEvent::LoggedOut).unwrap();
        let (entered, started) = oneshot::channel();
        let mut entered = Some(entered);
        let (release, blocked) = std::sync::mpsc::channel();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let observed = applied.clone();
        let final_id = UI_UPDATE_QUEUE_CAPACITY as u64 + 4;
        let writer = LogWriter::start(
            &tokio::runtime::Handle::current(),
            events,
            1,
            move |write| {
                let id = match write {
                    LogWrite::Append(message) => message.id.0,
                    LogWrite::Highlight { message_id, .. } => message_id.0,
                    _ => panic!("expected append or highlight"),
                };
                observed.lock().unwrap().push(id);
                if let Some(entered) = entered.take() {
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                if id == final_id {
                    Err("test disk failure".into())
                } else {
                    Ok(())
                }
            },
        );
        writer.append(&message(1)).await.unwrap();
        started.await.unwrap();
        writer.append(&message(2)).await.unwrap();
        let updates = writer.update_sender();
        for id in 3..final_id - 1 {
            updates.try_send(highlight_update(&message(id))).unwrap();
        }
        // The relay takes one update, then waits for the full writer queue.
        // Refilling that slot leaves both queues at their bounded capacity.
        updates
            .send(highlight_update(&message(final_id - 1)))
            .await
            .unwrap();
        assert_eq!(updates.capacity(), 0);
        let final_update = match updates.try_send(highlight_update(&message(final_id))) {
            Err(mpsc::error::TrySendError::Full(update)) => update,
            result => panic!("expected bounded UI queue, got {result:?}"),
        };
        let pending = updates.send(final_update);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(3), writer.flush())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error, "test disk failure");
        writer.shutdown().await.unwrap();
        assert_eq!(*applied.lock().unwrap(), (1..=final_id).collect::<Vec<_>>());
        assert!(matches!(received.try_recv().unwrap(), AppEvent::LoggedOut));
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn shutdown_drains_ui_updates_accepted_after_its_marker() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(1);
        events.try_send(AppEvent::LoggedOut).unwrap();
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let mut local = message(1);
        local.server_id = None;
        local.flags.is_self = true;
        let timestamp_ms = local.timestamp.timestamp_millis();
        writer.append(&local).await.unwrap();
        let mut confirmed = local.clone();
        confirmed.id = MessageId(2);
        confirmed.server_id = Some("confirmed-on-exit".into());
        writer.append(&confirmed).await.unwrap();
        let updates = writer.update_sender();
        updates.try_send(highlight_update(&local)).unwrap();
        let shutdown = writer.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::FutureExt::now_or_never(shutdown.as_mut()).is_none());
        confirmed.id = local.id;
        updates
            .try_send(HistoryUpdate::Confirmation {
                message: Box::new(confirmed),
                original_timestamp_ms: timestamp_ms,
            })
            .unwrap();
        shutdown.await.unwrap();
        let rows = store.recent_messages(&local.channel, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, local.id);
        assert_eq!(rows[0].server_id.as_deref(), Some("confirmed-on-exit"));
        assert!(rows[0].flags.is_highlighted);
        assert!(updates.is_closed());
        assert!(matches!(
            updates.try_send(highlight_update(&local)),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
        assert!(writer.append(&message(3)).await.is_err());
    }

    #[tokio::test]
    async fn underlying_writer_closure_closes_ui_updates_and_fails_barriers() {
        let (events, _received) = mpsc::channel(1);
        let writer = LogWriter::start(&tokio::runtime::Handle::current(), events, 1, |_| Ok(()));
        let updates = writer.update_sender();
        let (done, completed) = oneshot::channel();
        writer.commands.send(Command::Shutdown(done)).await.unwrap();
        completed.await.unwrap().unwrap();
        updates.try_send(highlight_update(&message(1))).unwrap();
        let (flush_done, flushed) = oneshot::channel();
        updates.try_send(HistoryUpdate::Flush(flush_done)).unwrap();
        let (shutdown_done, shutdown) = oneshot::channel();
        updates
            .try_send(HistoryUpdate::Shutdown(shutdown_done))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            updates.closed().await;
            for error in [flushed.await.unwrap(), shutdown.await.unwrap()] {
                assert!(error.unwrap_err().contains("writer stopped"));
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            updates.try_send(highlight_update(&message(2))),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
        assert!(writer.flush().await.is_err());
    }

    #[tokio::test]
    async fn history_moderation_is_saved_as_one_batch_and_published_as_history() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, mut received) = mpsc::channel(4);
        let writer = LogWriter::new(
            &tokio::runtime::Handle::current(),
            store.clone(),
            events.clone(),
        );
        let channel = ChannelId::new("room");
        let actions = vec![
            ModerationAction::message(channel.clone(), "first".into()),
            ModerationAction::message(channel.clone(), "second".into()),
        ];
        publish_history_moderation(channel.clone(), actions, Some(&writer), &events).await;
        assert!(
            matches!(received.try_recv().unwrap(), AppEvent::HistoryModerationLoaded { actions, .. } if actions.len() == 2)
        );
        assert!(received.try_recv().is_err());
        for (id, server_id) in [(1, "first"), (2, "second")] {
            let mut row = message(id);
            row.server_id = Some(server_id.into());
            writer.append(&row).await.unwrap();
        }
        writer.flush().await.unwrap();
        assert!(store
            .recent_messages(&channel, 10)
            .unwrap()
            .iter()
            .all(|message| message.flags.is_deleted));
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn whispers_keep_the_captured_owner_and_reject_mismatched_accounts() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let (events, _received) = mpsc::channel(4);
        let writer = LogWriter::new(&tokio::runtime::Handle::current(), store.clone(), events);
        let whisper = |owner: &str, text: &str| AppEvent::WhisperReceived {
            owner_user_id: owner.into(),
            from_user_id: "99".into(),
            from_login: "bob".into(),
            from_display_name: "Bob".into(),
            target_login: "alice".into(),
            text: text.into(),
            twitch_emotes: Vec::new(),
            is_self: false,
            timestamp: chrono::Utc::now(),
            is_history: false,
        };
        writer
            .whisper("alice".into(), "1".into(), whisper("1", "first account"))
            .await
            .unwrap();
        writer
            .whisper("alice".into(), "2".into(), whisper("1", "stale account"))
            .await
            .unwrap();
        writer
            .whisper("alice".into(), "2".into(), whisper("2", "second account"))
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        for (owner, text) in [("1", "first account"), ("2", "second account")] {
            let rows = store
                .recent_messages(&crate::whisper_thread_channel_id(owner, "bob"), 10)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].raw_text, text);
        }
    }

    #[tokio::test]
    async fn shutdown_reports_a_stalled_writer_without_waiting_forever() {
        let (events, _received) = mpsc::channel(4);
        let (entered, started) = oneshot::channel();
        let mut entered = Some(entered);
        let (release, blocked) = std::sync::mpsc::channel();
        let writer = LogWriter::start(&tokio::runtime::Handle::current(), events, 1, move |_| {
            let _ = entered.take().unwrap().send(());
            blocked.recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(())
        });
        writer.append(&message(1)).await.unwrap();
        started.await.unwrap();
        let error = writer
            .shutdown_with_timeout(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("Timed out"));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), writer.commands.closed())
            .await
            .unwrap();
    }
}
