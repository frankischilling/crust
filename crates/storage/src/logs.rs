use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, NaiveDate, Utc};
use crust_core::model::{
    ChannelId, ChatMessage, HistoryCursor, MessageFlags, MessageId, ModerationAction,
    ModerationState, ModerationTarget, MsgKind, Sender, UserId, MODERATION_RULE_LIMIT,
};
use directories::ProjectDirs;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::StorageError;

const SQLITE_LOG_FILENAME: &str = "chat_logs.sqlite3";
const DEFAULT_RECENT_LIMIT: usize = 800;
const MAX_ROWS_PER_CHANNEL: i64 = 50_000;

const CONFIRMATION_FLAGS_QUERY: &str = "
    SELECT
        CASE WHEN json_valid(payload_json) THEN COALESCE(json_extract(payload_json, '$.flags.is_deleted'), 0) != 0 ELSE 0 END,
        CASE WHEN json_valid(payload_json) THEN COALESCE(json_extract(payload_json, '$.flags.is_highlighted'), 0) != 0 ELSE 0 END,
        CASE WHEN json_valid(payload_json) THEN COALESCE(json_extract(payload_json, '$.flags.is_mention'), 0) != 0 ELSE 0 END,
        CASE WHEN json_valid(payload_json) THEN COALESCE(json_extract(payload_json, '$.flags.suppress_notification'), 0) != 0 ELSE 0 END
    FROM (
        SELECT payload_json FROM chat_messages WHERE id = ?2 AND channel = ?1
        UNION ALL
        SELECT payload_json FROM chat_messages
        WHERE channel = ?1 AND server_id = ?3 AND id != ?2
    )";

// The CASE guard also protects old corrupt rows from JSON functions. Missing
// msg_kind is the historical default (ordinary chat); notices stay intact.
const MODERATION_MATCH: &str = "
    channel = ?1 AND CASE WHEN json_valid(payload_json) THEN
        (?2 = 'message' AND server_id = ?3)
        OR (?2 != 'message' AND (?6 IS NULL OR ts_ms <= ?6)
            AND (COALESCE(json_extract(payload_json, '$.msg_kind'), 'Chat') IN ('Chat', 'SuspiciousUserMessage')
                 OR json_type(payload_json, '$.msg_kind.Bits') = 'object'
                 OR COALESCE(json_extract(payload_json, '$.msg_kind.Sub.sub_msg'), '') != '')
            AND (?2 = 'channel' OR
                CASE WHEN ?5 IS NOT NULL AND COALESCE(json_extract(payload_json, '$.sender.user_id'), '') != ''
                    THEN json_extract(payload_json, '$.sender.user_id') = ?5
                    ELSE ?4 != '' AND json_extract(payload_json, '$.sender.login') = ?4 COLLATE NOCASE END))
    ELSE 0 END";

// Each target has its own indexed probe. Combining these into one OR-filtered
// subquery would scan every tombstone in a busy channel on each chat insert.
fn moderation_insert_predicate(row: &str) -> String {
    format!(
        r#"
        {row}.channel NOT LIKE 'whisper:%'
        AND CASE WHEN json_valid({row}.payload_json) THEN
            EXISTS(SELECT 1 FROM chat_moderation AS rule
                WHERE rule.channel = {row}.channel AND rule.target = 'message'
                    AND rule.target_key = {row}.server_id)
            OR ((COALESCE(json_extract({row}.payload_json, '$.msg_kind'), 'Chat') IN ('Chat', 'SuspiciousUserMessage')
                 OR json_type({row}.payload_json, '$.msg_kind.Bits') = 'object'
                 OR COALESCE(json_extract({row}.payload_json, '$.msg_kind.Sub.sub_msg'), '') != '') AND (
                EXISTS(SELECT 1 FROM chat_moderation AS rule
                    WHERE rule.channel = {row}.channel AND rule.target = 'channel'
                        AND rule.target_key = '' AND rule.cutoff_ms >= {row}.ts_ms)
                OR CASE WHEN COALESCE(json_extract({row}.payload_json, '$.sender.user_id'), '') != '' THEN
                    EXISTS(SELECT 1 FROM chat_moderation AS rule
                        WHERE rule.channel = {row}.channel AND rule.target = 'user'
                            AND rule.user_id = json_extract({row}.payload_json, '$.sender.user_id')
                            AND rule.cutoff_ms >= {row}.ts_ms)
                    OR EXISTS(SELECT 1 FROM chat_moderation AS rule
                        WHERE rule.channel = {row}.channel AND rule.target = 'user' AND rule.user_id IS NULL
                            AND rule.login != '' AND rule.login = json_extract({row}.payload_json, '$.sender.login') COLLATE NOCASE
                            AND rule.cutoff_ms >= {row}.ts_ms)
                ELSE EXISTS(SELECT 1 FROM chat_moderation AS rule
                        WHERE rule.channel = {row}.channel AND rule.target = 'user'
                            AND rule.login != '' AND rule.login = json_extract({row}.payload_json, '$.sender.login') COLLATE NOCASE
                            AND rule.cutoff_ms >= {row}.ts_ms)
                END))
        ELSE 0 END"#
    )
}

// Keep the query and partial index predicates identical so SQLite can read the
// newest attention rows directly without scanning ordinary chat payloads.
const RECENT_MENTIONS_FILTER: &str = "
    channel NOT LIKE 'whisper:%'
    AND CASE WHEN json_valid(payload_json) THEN
        COALESCE(json_extract(payload_json, '$.flags.suppress_notification'), 0) = 0
        AND (
            json_extract(payload_json, '$.flags.is_highlighted') = 1
            OR json_extract(payload_json, '$.flags.is_mention') = 1
            OR json_extract(payload_json, '$.flags.is_first_msg') = 1
            OR json_extract(payload_json, '$.flags.is_pinned') = 1
        )
    ELSE 0 END";

fn recent_mentions_query() -> String {
    format!(
        "SELECT payload_json FROM chat_messages WHERE {RECENT_MENTIONS_FILTER}
         ORDER BY ts_ms DESC, id DESC LIMIT ?1"
    )
}

pub struct HistoryPage {
    pub messages: Vec<ChatMessage>,
    /// SQLite row IDs in the same order as the decoded messages.
    pub row_ids: Vec<i64>,
    pub next: Option<HistoryCursor>,
    pub has_more: bool,
}

/// SQLite chat history with indexed reads and a per-channel retention limit.
#[derive(Clone)]
pub struct LogStore {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl LogStore {
    pub fn new() -> Result<Self, StorageError> {
        let dirs = ProjectDirs::from("dev", "crust", "crust")
            .ok_or_else(|| StorageError::Io(std::io::Error::other("cannot find config dir")))?;
        let base = dirs.data_dir().join("logs");
        std::fs::create_dir_all(&base)?;
        Self::with_db_path(base.join(SQLITE_LOG_FILENAME))
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    pub fn with_db_path(path: PathBuf) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(&path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            CREATE TABLE IF NOT EXISTS chat_messages (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                channel      TEXT    NOT NULL,
                ts_ms        INTEGER NOT NULL,
                server_id    TEXT,
                payload_json TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_chat_messages_channel_ts
                ON chat_messages(channel, ts_ms DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_chat_messages_channel_server_id
                ON chat_messages(channel, server_id);
            CREATE TABLE IF NOT EXISTS chat_moderation (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                channel TEXT NOT NULL,
                target TEXT NOT NULL,
                target_key TEXT NOT NULL,
                login TEXT NOT NULL DEFAULT '',
                user_id TEXT,
                cutoff_ms INTEGER,
                UNIQUE(channel, target, target_key)
            );
            CREATE INDEX IF NOT EXISTS idx_chat_moderation_user_id
                ON chat_moderation(channel, user_id, cutoff_ms)
                WHERE target = 'user' AND user_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_chat_moderation_login
                ON chat_moderation(channel, login COLLATE NOCASE, cutoff_ms)
                WHERE target = 'user';
            CREATE INDEX IF NOT EXISTS idx_chat_moderation_unknown_login
                ON chat_moderation(channel, login COLLATE NOCASE, cutoff_ms)
                WHERE target = 'user' AND user_id IS NULL;
            "#,
        )?;
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS chat_message_apply_moderation
             AFTER INSERT ON chat_messages WHEN {}
             BEGIN
                 UPDATE chat_messages
                 SET payload_json = json_set(payload_json, '$.flags.is_deleted', json('true'))
                 WHERE id = NEW.id;
             END;",
            moderation_insert_predicate("NEW")
        ))?;
        // Seed counts once for existing databases. Triggers keep them accurate
        // across transactions and other connections without rescanning history.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_counts: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'chat_message_counts')",
            [],
            |row| row.get(0),
        )?;
        if !has_counts {
            tx.execute_batch(
                "CREATE TABLE chat_message_counts (
                    channel TEXT PRIMARY KEY,
                    message_count INTEGER NOT NULL
                 );
                 INSERT INTO chat_message_counts(channel, message_count)
                 SELECT channel, COUNT(*) FROM chat_messages GROUP BY channel;",
            )?;
        }
        tx.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS chat_message_count_insert
             AFTER INSERT ON chat_messages BEGIN
                 INSERT INTO chat_message_counts(channel, message_count)
                 VALUES (NEW.channel, 1)
                 ON CONFLICT(channel) DO UPDATE
                     SET message_count = message_count + 1;
             END;
             CREATE TRIGGER IF NOT EXISTS chat_message_count_delete
             AFTER DELETE ON chat_messages BEGIN
                 UPDATE chat_message_counts SET message_count = message_count - 1
                 WHERE channel = OLD.channel;
             END;",
        )?;
        tx.commit()?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path,
        })
    }

    /// Persist a fully-populated chat message row.
    pub fn append_message(&self, msg: &ChatMessage) -> Result<(), StorageError> {
        // Avoid writing replayed history back into persistent history.
        if msg.flags.is_history {
            return Ok(());
        }
        let payload = serde_json::to_string(msg).map_err(|e| StorageError::Serde(e.to_string()))?;
        let ts_ms = msg.timestamp.timestamp_millis();
        let channel = msg.channel.as_str();
        let server_id = msg.server_id.as_deref().filter(|id| !id.is_empty());
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            "INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
             SELECT ?1, ?2, ?3, ?4
             WHERE ?3 IS NULL OR NOT EXISTS (
                 SELECT 1 FROM chat_messages WHERE channel = ?1 AND server_id = ?3
             )",
            params![channel, ts_ms, server_id, payload],
        )?;
        if inserted != 0 {
            let count: i64 = tx.query_row(
                "SELECT message_count FROM chat_message_counts WHERE channel = ?1",
                params![channel],
                |row| row.get(0),
            )?;
            if count > MAX_ROWS_PER_CHANNEL {
                // The timestamp index reaches the oldest excess rows directly.
                tx.execute(
                    "DELETE FROM chat_messages WHERE id IN (
                         SELECT id FROM chat_messages WHERE channel = ?1
                         ORDER BY ts_ms ASC, id ASC LIMIT ?2
                     )",
                    params![channel, count - MAX_ROWS_PER_CHANNEL],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Check a channel/server identity before pairing another pending own send.
    pub fn has_server_message(
        &self,
        channel: &ChannelId,
        server_id: &str,
    ) -> Result<bool, StorageError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM chat_messages WHERE channel = ?1 AND server_id = ?2)",
            params![channel.as_str(), server_id],
            |row| row.get(0),
        )?)
    }

    /// Reconcile one identified local echo with its server-confirmed message.
    /// The existing SQLite row and local message ID remain stable for history.
    /// Returns false when neither the pending nor confirmed identity exists.
    pub fn confirm_message(
        &self,
        confirmed: &ChatMessage,
        original_timestamp_ms: i64,
    ) -> Result<bool, StorageError> {
        let Some(server_id) = confirmed.server_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(false);
        };
        let Ok(local_id) = i64::try_from(confirmed.id.0) else {
            return Ok(false);
        };
        if !confirmed.flags.is_self || confirmed.flags.is_history {
            return Ok(false);
        }
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let tx = conn.transaction()?;
        let pending_row: Option<i64> = tx
            .query_row(
                "SELECT id FROM chat_messages
             WHERE channel = ?1 AND ts_ms = ?2 AND server_id IS NULL
                 AND CASE WHEN json_valid(payload_json)
                     THEN json_extract(payload_json, '$.id') = ?3 ELSE 0 END
             ORDER BY id ASC LIMIT 1",
                params![confirmed.channel.as_str(), original_timestamp_ms, local_id],
                |row| row.get(0),
            )
            .optional()?;
        // The ordered writer can confirm before the UI handles the event. A
        // later UI update enriches only that exact confirmed local identity.
        let pending_row = match pending_row {
            Some(row) => Some(row),
            None => tx.query_row(
                "SELECT id FROM chat_messages
                 WHERE channel = ?1 AND server_id = ?2
                     AND CASE WHEN json_valid(payload_json)
                         THEN json_extract(payload_json, '$.id') = ?3 ELSE 0 END
                 ORDER BY id ASC LIMIT 1",
                params![confirmed.channel.as_str(), server_id, local_id],
                |row| row.get(0),
            ).optional()?,
        };
        let Some(pending_row) = pending_row else {
            return Ok(false);
        };

        let mut merged = confirmed.clone();
        {
            let mut statement = tx.prepare(CONFIRMATION_FLAGS_QUERY)?;
            let flags = statement.query_map(
                params![confirmed.channel.as_str(), pending_row, server_id],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )?;
            for flags in flags {
                let (deleted, highlighted, mention, suppressed) = flags?;
                merged.flags.is_deleted |= deleted;
                merged.flags.is_highlighted |= highlighted;
                merged.flags.is_mention |= mention;
                merged.flags.suppress_notification |= suppressed;
            }
        }
        let payload = serde_json::to_string(&merged)
            .map_err(|error| StorageError::Serde(error.to_string()))?;
        tx.execute(
            "UPDATE chat_messages SET server_id = ?2, ts_ms = ?3, payload_json = ?4 WHERE id = ?1",
            params![
                pending_row,
                server_id,
                merged.timestamp.timestamp_millis(),
                payload
            ],
        )?;
        tx.execute(
            &format!(
                "UPDATE chat_messages
                SET payload_json = json_set(payload_json, '$.flags.is_deleted', json('true'))
                WHERE id = ?1 AND {}",
                moderation_insert_predicate("chat_messages")
            ),
            params![pending_row],
        )?;
        tx.execute(
            "DELETE FROM chat_messages WHERE channel = ?1 AND server_id = ?2 AND id != ?3",
            params![confirmed.channel.as_str(), server_id, pending_row],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Mark saved rows and remember bounded source cutoffs or exact identities.
    /// The insert trigger protects messages queued after the moderation action.
    pub fn apply_moderation(&self, action: &ModerationAction) -> Result<(), StorageError> {
        self.apply_moderation_batch(std::slice::from_ref(action))
    }

    pub fn apply_moderation_batch(&self, actions: &[ModerationAction]) -> Result<(), StorageError> {
        if actions.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let tx = conn.transaction()?;
        for action in actions {
            Self::apply_moderation_in_transaction(&tx, action)?;
        }
        tx.execute(
            "DELETE FROM chat_moderation WHERE id NOT IN
                (SELECT id FROM chat_moderation ORDER BY id DESC LIMIT ?1)",
            params![MODERATION_RULE_LIMIT as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn apply_moderation_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        action: &ModerationAction,
    ) -> Result<(), StorageError> {
        if !action.is_channel_action() {
            return Ok(());
        }
        let (target, key, login, user_id) = match &action.target {
            ModerationTarget::Message(id) if !id.is_empty() => ("message", id.clone(), "", None),
            ModerationTarget::Message(_) => return Ok(()),
            ModerationTarget::User { login, user_id } => {
                if login.is_empty() && user_id.is_none() {
                    return Ok(());
                }
                let key = serde_json::to_string(&(login, user_id))
                    .map_err(|error| StorageError::Serde(error.to_string()))?;
                ("user", key, login.as_str(), user_id.as_deref())
            }
            ModerationTarget::Channel => ("channel", String::new(), "", None),
        };
        let cutoff = action.timestamp.map(|time| time.timestamp_millis());
        if target == "message" || cutoff.is_some() {
            tx.execute(
                "INSERT INTO chat_moderation(channel, target, target_key, login, user_id, cutoff_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(channel, target, target_key) DO UPDATE
                    SET id = excluded.id, cutoff_ms = excluded.cutoff_ms
                    WHERE excluded.cutoff_ms > chat_moderation.cutoff_ms",
                params![action.channel.as_str(), target, key, login, user_id, cutoff],
            )?;
        } else {
            // No source time: retain only identities which actually existed
            // when this action ran, never guess a cutoff from the local clock.
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO chat_moderation(channel, target, target_key)
                    SELECT channel, 'message', server_id FROM chat_messages
                    WHERE {MODERATION_MATCH} AND server_id IS NOT NULL AND server_id != ''
                    ORDER BY ts_ms DESC, id DESC LIMIT ?7"
                ),
                params![
                    action.channel.as_str(),
                    target,
                    key,
                    login,
                    user_id,
                    cutoff,
                    MODERATION_RULE_LIMIT as i64
                ],
            )?;
        }
        if target == "message" {
            tx.execute(
                "UPDATE chat_messages SET payload_json = json_set(payload_json, '$.flags.is_deleted', json('true'))
                 WHERE channel = ?1 AND server_id = ?2 AND json_valid(payload_json)",
                params![action.channel.as_str(), key],
            )?;
        } else {
            tx.execute(
                &format!("UPDATE chat_messages SET payload_json = json_set(payload_json, '$.flags.is_deleted', json('true'))
                          WHERE {MODERATION_MATCH}"),
                params![action.channel.as_str(), target, key, login, user_id, cutoff],
            )?;
        }
        Ok(())
    }

    /// Apply durable moderation to a remote replay before it reaches the UI.
    /// Loading only this channel's bounded rules avoids scanning saved payloads.
    pub fn apply_saved_moderation(
        &self,
        channel: &ChannelId,
        messages: &mut [ChatMessage],
    ) -> Result<(), StorageError> {
        let state = ModerationState::default();
        for action in self.saved_moderation(channel)? {
            state.apply_to_retained(&action, std::iter::empty());
        }
        for message in messages {
            state.apply(message);
        }
        Ok(())
    }

    pub fn saved_moderation(
        &self,
        channel: &ChannelId,
    ) -> Result<Vec<ModerationAction>, StorageError> {
        if channel.as_str().starts_with("whisper:") {
            return Ok(Vec::new());
        }
        let rules = {
            let conn = self.conn.lock().map_err(|_| {
                StorageError::Io(std::io::Error::other("chat log DB mutex poisoned"))
            })?;
            let mut statement = conn.prepare(
                "SELECT target, target_key, login, user_id, cutoff_ms FROM chat_moderation
                 WHERE channel = ?1 ORDER BY id ASC",
            )?;
            let rows = statement
                .query_map(params![channel.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut actions = Vec::with_capacity(rules.len());
        for (target, key, login, user_id, cutoff) in rules {
            let timestamp = cutoff.and_then(DateTime::from_timestamp_millis);
            let action = match target.as_str() {
                "message" => ModerationAction::message(channel.clone(), key),
                "user" if timestamp.is_some() => {
                    ModerationAction::user(channel.clone(), login, user_id, timestamp)
                }
                "channel" if timestamp.is_some() => {
                    ModerationAction::channel(channel.clone(), timestamp)
                }
                _ => continue,
            };
            actions.push(action);
        }
        Ok(actions)
    }

    /// Update the existing row after context-dependent highlight evaluation.
    pub fn mark_highlighted(
        &self,
        channel: &ChannelId,
        server_id: Option<&str>,
        message_id: MessageId,
        timestamp_ms: i64,
    ) -> Result<(), StorageError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        if let Some(server_id) = server_id.filter(|id| !id.is_empty()) {
            conn.execute(
                "UPDATE chat_messages
                 SET payload_json = json_set(payload_json, '$.flags.is_highlighted', json('true'))
                 WHERE channel = ?1 AND server_id = ?2 AND json_valid(payload_json)",
                params![channel.as_str(), server_id],
            )?;
        } else if let Ok(local_id) = i64::try_from(message_id.0) {
            conn.execute(
                "UPDATE chat_messages
                 SET payload_json = json_set(payload_json, '$.flags.is_highlighted', json('true'))
                 WHERE channel = ?1 AND ts_ms = ?2
                   AND CASE WHEN json_valid(payload_json)
                       THEN json_extract(payload_json, '$.id') = ?3 ELSE 0 END",
                params![channel.as_str(), timestamp_ms, local_id],
            )?;
        }
        Ok(())
    }

    /// Load recent messages for a channel, oldest -> newest.
    pub fn recent_messages(
        &self,
        channel: &ChannelId,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, StorageError> {
        Ok(self.history_page(channel, None, limit)?.messages)
    }

    /// Load the most-recent cross-channel *mention-matching* messages, oldest
    /// -> newest. A message qualifies if its persisted JSON payload has any
    /// of the "attention" flags set (`is_highlighted`, `is_mention`,
    /// `is_first_msg`, `is_pinned`). Whispers are excluded (they have their
    /// own dedicated UI feed via `whisper:` channel prefix).
    ///
    /// A partial timestamp index excludes ordinary chat and whisper threads.
    /// The first read builds it for existing databases; callers should run
    /// history reads on a blocking worker. SQLite maintains it when messages
    /// are inserted, highlighted, suppressed, or removed.
    pub fn recent_mentions(&self, limit: usize) -> Result<Vec<ChatMessage>, StorageError> {
        let safe_limit = limit.clamp(1, 5_000) as i64;
        let payloads = {
            let conn = self.conn.lock().map_err(|_| {
                StorageError::Io(std::io::Error::other("chat log DB mutex poisoned"))
            })?;
            // Defer the one-time scan from database opening to the history
            // worker. Interrupted index creation rolls back atomically and
            // the next read can retry it.
            conn.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS idx_chat_messages_attention_ts
                 ON chat_messages(ts_ms DESC, id DESC) WHERE {RECENT_MENTIONS_FILTER};"
            ))?;
            let mut stmt = conn.prepare(&recent_mentions_query())?;
            let rows = stmt.query_map(params![safe_limit], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut out: Vec<ChatMessage> = Vec::new();
        // Decoding saved messages does not need the shared database lock.
        for payload in payloads {
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&payload) {
                let f = &msg.flags;
                if !f.suppress_notification
                    && (f.is_highlighted || f.is_mention || f.is_first_msg || f.is_pinned)
                {
                    out.push(msg);
                }
            }
        }
        out.reverse(); // oldest -> newest so callers can merge by timestamp.
        Ok(out)
    }

    /// Load older messages for a channel before `before_ts_ms`, oldest -> newest.
    pub fn older_messages(
        &self,
        channel: &ChannelId,
        before_ts_ms: i64,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, StorageError> {
        Ok(self
            .history_page(
                channel,
                Some(HistoryCursor {
                    timestamp_ms: before_ts_ms,
                    row_id: i64::MIN,
                }),
                limit,
            )?
            .messages)
    }

    /// Read an indexed page and advance past corrupt payloads as well as valid rows.
    pub fn history_page(
        &self,
        channel: &ChannelId,
        before: Option<HistoryCursor>,
        limit: usize,
    ) -> Result<HistoryPage, StorageError> {
        let safe_limit = limit.clamp(1, 5_000);
        let boundary = before.unwrap_or(HistoryCursor {
            timestamp_ms: i64::MAX,
            row_id: i64::MAX,
        });
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let mut stmt = conn.prepare(
            "SELECT ts_ms, id, payload_json
             FROM chat_messages
             WHERE channel = ?1 AND (ts_ms, id) < (?2, ?3)
             ORDER BY ts_ms DESC, id DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                channel.as_str(),
                boundary.timestamp_ms,
                boundary.row_id,
                (safe_limit + 1) as i64
            ],
            |row| {
                Ok((
                    HistoryCursor {
                        timestamp_ms: row.get(0)?,
                        row_id: row.get(1)?,
                    },
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        let mut out = Vec::new();
        let mut row_ids = Vec::new();
        let mut next = before;
        let mut has_more = false;
        for (index, row) in rows.enumerate() {
            let (cursor, payload) = row?;
            if index == safe_limit {
                has_more = true;
                break;
            }
            next = Some(cursor);
            if cursor.row_id <= 0 {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&payload) {
                out.push(msg);
                row_ids.push(cursor.row_id);
            }
        }
        out.reverse();
        row_ids.reverse();
        Ok(HistoryPage {
            messages: out,
            row_ids,
            next,
            has_more,
        })
    }

    /// List distinct channel ids that begin with `prefix`, ordered by most
    /// recent message timestamp descending.
    pub fn recent_channels_with_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let safe_limit = limit.clamp(1, 1_000) as i64;
        let like_pattern = format!(
            "{}%",
            prefix
                .replace('!', "!!")
                .replace('%', "!%")
                .replace('_', "!_")
        );
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let mut stmt = conn.prepare(
            "SELECT channel
             FROM chat_messages
             WHERE channel LIKE ?1 ESCAPE '!'
             GROUP BY channel
             ORDER BY MAX(ts_ms) DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like_pattern, safe_limit], |row| {
            row.get::<_, String>(0)
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Compatibility wrapper: append a plain text line as a chat message.
    pub async fn append(
        &self,
        channel: &str,
        ts: DateTime<Utc>,
        sender: &str,
        text: &str,
    ) -> Result<(), StorageError> {
        let msg = ChatMessage {
            id: MessageId(0),
            server_id: None,
            timestamp: ts,
            channel: ChannelId::new(channel),
            sender: Sender {
                original_display_name: None,
                user_id: UserId(sender.to_owned()),
                login: sender.to_lowercase(),
                display_name: sender.to_owned(),
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: text.to_owned(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags {
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
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        };
        self.append_message(&msg)
    }

    /// Compatibility wrapper: read all lines for a UTC date (`YYYY-MM-DD`).
    pub async fn read(&self, channel: &str, date: &str) -> Result<String, StorageError> {
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|e| StorageError::Serde(e.to_string()))?;
        let start_ms = day
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| StorageError::Serde("invalid start-of-day".to_owned()))?
            .and_utc()
            .timestamp_millis();
        let end_ms = day
            .succ_opt()
            .ok_or_else(|| StorageError::Serde("invalid end-of-day".to_owned()))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| StorageError::Serde("invalid next-day start".to_owned()))?
            .and_utc()
            .timestamp_millis();

        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let mut stmt = conn.prepare(
            "SELECT payload_json
             FROM chat_messages
             WHERE channel = ?1 AND ts_ms >= ?2 AND ts_ms < ?3
             ORDER BY ts_ms ASC, id ASC",
        )?;
        let mut text = String::new();
        let channel_id = ChannelId::new(channel);
        let rows = stmt.query_map(params![channel_id.as_str(), start_ms, end_ms], |row| {
            row.get::<_, String>(0)
        })?;
        for row in rows {
            let payload = row?;
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&payload) {
                let line = format!(
                    "[{}] <{}> {}\n",
                    msg.timestamp.with_timezone(&Utc).format("%H:%M:%S"),
                    msg.sender.display_name,
                    msg.plain_text()
                );
                text.push_str(&line);
            }
        }
        Ok(text)
    }

    pub fn default_recent_limit() -> usize {
        DEFAULT_RECENT_LIMIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("crust-{name}-{nanos}.sqlite3"))
    }

    fn message(id: u64, timestamp_ms: i64, channel: &str) -> ChatMessage {
        ChatMessage {
            id: MessageId(id),
            server_id: Some(format!("message-{id}")),
            timestamp: Utc.timestamp_millis_opt(timestamp_ms).unwrap(),
            channel: ChannelId::new(channel),
            sender: Sender {
                original_display_name: None,
                user_id: UserId("1".to_owned()),
                login: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: format!("message {id}"),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn moderation_insert_probes_use_exact_message_and_sender_indexes() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let conn = store.conn.lock().unwrap();
        let predicate = moderation_insert_predicate("incoming");
        let query = format!(
            "WITH incoming(channel, server_id, ts_ms, payload_json) AS (VALUES (?1, ?2, ?3, ?4))
            SELECT {predicate} FROM incoming"
        );
        let payload = serde_json::to_string(&message(1, 1000, "first")).unwrap();
        let mut plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap();
        let details: Vec<String> = plan
            .query_map(params!["first", "message-1", 1000, payload], |row| {
                row.get(3)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let details = details.join("\n");
        assert!(
            details.contains("channel=? AND target=? AND target_key=?"),
            "{details}"
        );
        assert!(
            details
                .contains("idx_chat_moderation_user_id (channel=? AND user_id=? AND cutoff_ms>?)"),
            "{details}"
        );
        assert!(
            details.contains("idx_chat_moderation_login (channel=? AND login=? AND cutoff_ms>?)"),
            "{details}"
        );
        assert!(
            details.contains(
                "idx_chat_moderation_unknown_login (channel=? AND login=? AND cutoff_ms>?)"
            ),
            "{details}"
        );
        assert!(!details.contains("SCAN rule"), "{details}");
    }

    #[test]
    fn moderation_survives_reopen_and_respects_channel_user_and_cutoff() {
        let path = temp_db_path("moderation-reopen");
        let channel = ChannelId::new("first");
        {
            let store = LogStore::with_db_path(path.clone()).unwrap();
            let mut old = message(1, 1000, "first");
            old.flags.is_mention = true;
            store.append_message(&old).unwrap();
            store.append_message(&message(1, 1000, "second")).unwrap();
            store.append_message(&message(2, 1001, "first")).unwrap();
            let mut other_user = message(3, 999, "first");
            other_user.sender.user_id = UserId("2".into());
            store.append_message(&other_user).unwrap();
            let mut notice = message(4, 999, "first");
            notice.msg_kind = MsgKind::SystemInfo;
            store.append_message(&notice).unwrap();
            let mut legacy = message(5, 999, "first");
            legacy.sender.user_id.0.clear();
            store.append_message(&legacy).unwrap();
            store
                .apply_moderation(&ModerationAction::user(
                    channel.clone(),
                    "ALICE".into(),
                    Some("1".into()),
                    Some(Utc.timestamp_millis_opt(1000).unwrap()),
                ))
                .unwrap();
            assert!(store.recent_mentions(10).unwrap()[0].flags.is_deleted);
        }
        {
            let store = LogStore::with_db_path(path.clone()).unwrap();
            store.append_message(&message(6, 998, "first")).unwrap();
            store.append_message(&message(7, 1002, "first")).unwrap();
            let rows = store.recent_messages(&channel, 20).unwrap();
            for row in rows {
                assert_eq!(
                    row.flags.is_deleted,
                    [1, 5, 6].contains(&row.id.0),
                    "row {}",
                    row.id.0
                );
                assert_eq!(row.raw_text, format!("message {}", row.id.0));
            }
            assert!(
                !store
                    .recent_messages(&ChannelId::new("second"), 10)
                    .unwrap()[0]
                    .flags
                    .is_deleted
            );
            let mut replay = vec![
                message(8, 999, "first"),
                message(9, 1001, "first"),
                message(8, 999, "second"),
            ];
            store.apply_saved_moderation(&channel, &mut replay).unwrap();
            assert_eq!(
                replay
                    .iter()
                    .map(|row| row.flags.is_deleted)
                    .collect::<Vec<_>>(),
                [true, false, false]
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn untimed_moderation_keeps_saved_identities_without_guessing_a_cutoff() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let channel = ChannelId::new("first");
        let mut old = message(1, 1000, "first");
        store.append_message(&old).unwrap();
        store.append_message(&message(1, 1000, "second")).unwrap();
        store.conn.lock().unwrap().execute(
            "INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json) VALUES ('first', 999, 'corrupt', '{bad json')", []).unwrap();
        store
            .apply_moderation(&ModerationAction::user(
                channel.clone(),
                "alice".into(),
                None,
                None,
            ))
            .unwrap();
        store.append_message(&message(2, 999, "first")).unwrap();
        let mut replay = vec![
            old.clone(),
            message(2, 999, "first"),
            message(1, 1000, "second"),
        ];
        store.apply_saved_moderation(&channel, &mut replay).unwrap();
        assert_eq!(
            replay
                .iter()
                .map(|row| row.flags.is_deleted)
                .collect::<Vec<_>>(),
            [true, false, false]
        );
        assert_eq!(
            store
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT payload_json FROM chat_messages WHERE server_id = 'corrupt'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "{bad json"
        );
        old.channel = ChannelId("whisper:uid:1:alice".into());
        store.append_message(&old).unwrap();
        store
            .apply_moderation(&ModerationAction::channel(
                old.channel.clone(),
                Some(Utc::now()),
            ))
            .unwrap();
        store
            .apply_moderation(&ModerationAction::message(
                old.channel.clone(),
                old.server_id.unwrap(),
            ))
            .unwrap();
        assert!(
            !store.recent_messages(&old.channel, 10).unwrap()[0]
                .flags
                .is_deleted
        );
    }

    #[test]
    fn deletion_before_append_and_channel_clear_preserve_text_and_notices() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let channel = ChannelId::new("first");
        store
            .apply_moderation(&ModerationAction::message(
                channel.clone(),
                "message-1".into(),
            ))
            .unwrap();
        store.append_message(&message(1, 999, "first")).unwrap();
        store.append_message(&message(1, 999, "second")).unwrap();
        let mut notice = message(2, 999, "first");
        notice.msg_kind = MsgKind::Timeout {
            login: "alice".into(),
            seconds: 60,
        };
        store.append_message(&notice).unwrap();
        store
            .apply_moderation(&ModerationAction::channel(
                channel.clone(),
                Some(Utc.timestamp_millis_opt(1000).unwrap()),
            ))
            .unwrap();
        for (id, time, kind) in [
            (3, 999, MsgKind::Bits { amount: 10 }),
            (4, 999, MsgKind::SuspiciousUserMessage),
            (5, 1001, MsgKind::Chat),
        ] {
            let mut row = message(id, time, "first");
            row.msg_kind = kind;
            store.append_message(&row).unwrap();
        }
        store
            .apply_moderation(&ModerationAction::channel(
                channel.clone(),
                Some(Utc.timestamp_millis_opt(500).unwrap()),
            ))
            .unwrap();
        for row in store.recent_messages(&channel, 10).unwrap() {
            assert_eq!(row.flags.is_deleted, [1, 3, 4].contains(&row.id.0));
            assert_eq!(row.raw_text, format!("message {}", row.id.0));
        }
        assert!(
            !store
                .recent_messages(&ChannelId::new("second"), 10)
                .unwrap()[0]
                .flags
                .is_deleted
        );
    }

    #[test]
    fn subscription_moderation_matches_retained_and_late_history_bodies() {
        let channel = ChannelId::new("room");
        for action in [
            ModerationAction::user(channel.clone(), "alice".into(), Some("1".into()),
                Some(Utc.timestamp_millis_opt(1000).unwrap())),
            ModerationAction::channel(channel.clone(), Some(Utc.timestamp_millis_opt(1000).unwrap())),
        ] {
            let store = LogStore::with_db_path(":memory:".into()).unwrap();
            let subscription = |id, body: &str, timestamp| {
                let mut row = message(id, timestamp, "room");
                row.raw_text = body.into();
                row.msg_kind = MsgKind::Sub { display_name: "Alice".into(), months: 1,
                    plan: "Tier 1".into(), is_gift: false, sub_msg: body.into() };
                row
            };
            for row in [subscription(1, "old subscriber text", 999),
                subscription(2, "", 999), subscription(3, "new subscriber text", 1001)] {
                store.append_message(&row).unwrap();
            }
            store.apply_moderation(&action).unwrap();
            for row in [subscription(4, "late subscriber text", 999), subscription(5, "", 999)] {
                store.append_message(&row).unwrap();
            }
            let rows = store.recent_messages(&channel, 10).unwrap();
            assert_eq!(rows.len(), 5);
            for row in rows {
                assert_eq!(row.flags.is_deleted, matches!(row.id.0, 1 | 4), "row {}", row.id.0);
                assert!(matches!(row.msg_kind, MsgKind::Sub { .. }));
            }
        }
    }

    #[test]
    fn confirmation_flag_reads_use_exact_row_and_server_id_indexes() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let conn = store.conn.lock().unwrap();
        let mut plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {CONFIRMATION_FLAGS_QUERY}"))
            .unwrap();
        let details: Vec<String> = plan
            .query_map(params!["first", 1, "confirmed-id"], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let details = details.join("\n");
        assert!(
            details.contains("SEARCH chat_messages USING INTEGER PRIMARY KEY (rowid=?)"),
            "{details}"
        );
        assert!(
            details.contains("idx_chat_messages_channel_server_id (channel=? AND server_id=?)"),
            "{details}"
        );
        assert!(
            !details.contains("SCAN chat_messages") && !details.contains("(channel=?)"),
            "{details}"
        );
    }

    #[test]
    fn confirmed_own_echoes_keep_one_durable_row_in_each_write_order() {
        for order in [
            "local-server-confirm",
            "server-local-confirm",
            "local-confirm-server",
        ] {
            let path = temp_db_path(order);
            let channel = ChannelId::new("first");
            let mut local = message(1, 1000, "first");
            local.server_id = None;
            local.flags.is_self = true;
            local.flags.is_highlighted = true;
            let mut server = local.clone();
            server.id = MessageId(2);
            server.server_id = Some("confirmed-id".into());
            server.timestamp = Utc.timestamp_millis_opt(1100).unwrap();
            server.sender.color = Some("#123456".into());
            server.flags.is_highlighted = false;
            let mut confirmed = server.clone();
            confirmed.id = local.id;
            {
                let store = LogStore::with_db_path(path.clone()).unwrap();
                if order == "server-local-confirm" {
                    store.append_message(&server).unwrap();
                }
                store.append_message(&local).unwrap();
                let original_row_id = store.history_page(&channel, None, 10).unwrap().row_ids[0];
                if order == "local-server-confirm" {
                    store.append_message(&server).unwrap();
                }
                store.confirm_message(&confirmed, 1000).unwrap();
                if order == "local-confirm-server" {
                    store.append_message(&server).unwrap();
                }
                store.confirm_message(&confirmed, 1000).unwrap();
                let page = store.history_page(&channel, None, 10).unwrap();
                assert_eq!(page.messages.len(), 1, "{order}");
                assert_eq!(page.row_ids, [original_row_id], "{order}");
                assert_eq!(page.messages[0].id, local.id, "{order}");
                assert_eq!(page.messages[0].timestamp, server.timestamp, "{order}");
                assert_eq!(
                    page.messages[0].sender.color, server.sender.color,
                    "{order}"
                );
                assert!(page.messages[0].flags.is_highlighted, "{order}");
                store
                    .apply_moderation(&ModerationAction::message(
                        channel.clone(),
                        "confirmed-id".into(),
                    ))
                    .unwrap();
            }
            {
                let store = LogStore::with_db_path(path.clone()).unwrap();
                let rows = store.recent_messages(&channel, 10).unwrap();
                assert_eq!(rows.len(), 1, "{order}");
                assert_eq!(rows[0].id, local.id, "{order}");
                assert!(
                    rows[0].flags.is_deleted && rows[0].flags.is_highlighted,
                    "{order}"
                );
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn confirmation_uses_exact_local_identity_and_preserves_saved_deletions() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let channel = ChannelId::new("first");
        let mut local = message(1, 1000, "first");
        local.server_id = None;
        local.flags.is_self = true;
        store.append_message(&local).unwrap();
        let mut repeated = local.clone();
        repeated.id = MessageId(3);
        store.append_message(&repeated).unwrap();
        let mut older = local.clone();
        older.timestamp = Utc.timestamp_millis_opt(900).unwrap();
        store.append_message(&older).unwrap();
        let mut server = local.clone();
        server.id = MessageId(2);
        server.server_id = Some("confirmed-id".into());
        server.timestamp = Utc.timestamp_millis_opt(1100).unwrap();
        server.flags.is_deleted = true;
        store.append_message(&server).unwrap();
        let mut other_channel = server.clone();
        other_channel.channel = ChannelId::new("second");
        other_channel.flags.is_deleted = false;
        store.append_message(&other_channel).unwrap();
        let mut confirmed = server;
        confirmed.id = local.id;
        confirmed.flags.is_deleted = false;
        store.confirm_message(&confirmed, 999).unwrap();
        assert_eq!(store.recent_messages(&channel, 10).unwrap().len(), 4);
        store.confirm_message(&confirmed, 1000).unwrap();
        let rows = store.recent_messages(&channel, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].server_id.is_none() && rows[1].server_id.is_none());
        assert!(!rows[0].flags.is_deleted && !rows[1].flags.is_deleted);
        assert_eq!(rows[1].id, repeated.id);
        assert!(rows[2].flags.is_deleted);
        assert_eq!(rows[2].id, local.id);
        assert!(
            !store.recent_messages(&other_channel.channel, 10).unwrap()[0]
                .flags
                .is_deleted
        );
    }

    #[test]
    fn confirmation_applies_deletion_that_arrived_before_the_server_row() {
        let store = LogStore::with_db_path(":memory:".into()).unwrap();
        let mut local = message(1, 1000, "first");
        local.server_id = None;
        local.flags.is_self = true;
        store.append_message(&local).unwrap();
        store
            .apply_moderation(&ModerationAction::message(
                local.channel.clone(),
                "confirmed-id".into(),
            ))
            .unwrap();
        let mut confirmed = local;
        confirmed.server_id = Some("confirmed-id".into());
        confirmed.timestamp = Utc.timestamp_millis_opt(1100).unwrap();
        store.confirm_message(&confirmed, 1000).unwrap();
        let rows = store.recent_messages(&confirmed.channel, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].flags.is_deleted);
    }

    #[test]
    fn custom_highlights_are_available_after_reopening_saved_history() {
        let path = temp_db_path("saved-highlight");
        let channel = ChannelId::new("first");
        {
            let store = LogStore::with_db_path(path.clone()).unwrap();
            let first = message(1, 1000, "first");
            let mut second = message(2, 1000, "first");
            second.server_id = None;
            store.append_message(&first).unwrap();
            store.append_message(&second).unwrap();
            store.append_message(&message(3, 1000, "first")).unwrap();
            store.append_message(&message(1, 1000, "second")).unwrap();
            store
                .mark_highlighted(&channel, first.server_id.as_deref(), first.id, 1000)
                .unwrap();
            store
                .mark_highlighted(&channel, None, second.id, 1000)
                .unwrap();
        }
        {
            let store = LogStore::with_db_path(path.clone()).unwrap();
            let mentions = store.recent_mentions(10).unwrap();
            assert_eq!(
                mentions.iter().map(|row| row.id.0).collect::<Vec<_>>(),
                [1, 2]
            );
            assert!(mentions
                .iter()
                .all(|row| row.flags.is_highlighted && row.channel == channel));
            assert_eq!(mentions[0].raw_text, "message 1");
            assert!(
                !store.recent_messages(&channel, 3).unwrap()[2]
                    .flags
                    .is_highlighted
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn history_pages_keep_every_row_at_equal_timestamps() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let channel = ChannelId::new("first");
        for id in 1..=11 {
            let mut row = message(id, if id == 1 { 999 } else { 1000 }, "first");
            row.server_id = None;
            store.append_message(&row).unwrap();
        }
        store.append_message(&message(90, 1000, "second")).unwrap();
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = store.history_page(&channel, cursor, 3).unwrap();
            if let (Some(before), Some(after)) = (cursor, page.next) {
                assert!(after < before);
            }
            ids.extend(page.messages.iter().rev().map(|message| message.id.0));
            cursor = page.next;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(ids, (1..=11).rev().collect::<Vec<_>>());
        assert!(store
            .history_page(&channel, cursor, 3)
            .unwrap()
            .messages
            .is_empty());
    }

    #[test]
    fn history_pages_retain_storage_identity_without_rewriting_original_ids() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let channel = ChannelId::new("first");
        for id in [17, 17, 18] {
            let mut row = message(id, 1000, "first");
            row.server_id = None;
            row.raw_text = "same text".into();
            store.append_message(&row).unwrap();
        }
        let first = store.history_page(&channel, None, 2).unwrap();
        assert_eq!(first.row_ids, [2, 3]);
        assert_eq!(
            first
                .messages
                .iter()
                .map(|row| row.id.0)
                .collect::<Vec<_>>(),
            [17, 18]
        );
        let older = store.history_page(&channel, first.next, 2).unwrap();
        assert_eq!(older.row_ids, [1]);
        assert_eq!(older.messages[0].id, MessageId(17));
        for _ in 0..3 {
            let replay = store.history_page(&channel, None, 2).unwrap();
            assert_eq!(replay.row_ids, first.row_ids);
            assert_eq!(replay.next, first.next);
        }
    }

    #[test]
    fn history_cursor_advances_past_corrupt_payloads() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let channel = ChannelId::new("first");
        for id in 1..=5 {
            store.append_message(&message(id, 1000, "first")).unwrap();
        }
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE chat_messages SET payload_json = 'invalid' WHERE id >= 4",
                [],
            )
            .unwrap();
        let first = store.history_page(&channel, None, 2).unwrap();
        assert!(first.messages.is_empty());
        assert!(first.row_ids.is_empty());
        assert!(first.has_more);
        let next = store.history_page(&channel, first.next, 3).unwrap();
        assert_eq!(
            next.messages.iter().map(|row| row.id.0).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(next.row_ids, [1, 2, 3]);
        assert!(!next.has_more);
    }

    #[test]
    fn repeated_server_ids_are_deduplicated_per_channel() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let original = message(1, 1000, "first");
        store.append_message(&original).unwrap();
        store.append_message(&original).unwrap();
        let mut other_channel = original.clone();
        other_channel.channel = ChannelId::new("second");
        store.append_message(&other_channel).unwrap();
        let mut without_id = message(2, 2000, "first");
        without_id.server_id = None;
        store.append_message(&without_id).unwrap();
        store.append_message(&without_id).unwrap();
        let mut history = message(3, 3000, "first");
        history.flags.is_history = true;
        store.append_message(&history).unwrap();
        assert_eq!(
            store.recent_messages(&original.channel, 10).unwrap().len(),
            3
        );
        assert_eq!(
            store
                .recent_messages(&other_channel.channel, 10)
                .unwrap()
                .len(),
            1
        );
        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT message_count FROM chat_message_counts WHERE channel = 'first'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn channel_prefixes_treat_sql_wildcards_as_literal_account_names() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let channels = [
            "whisper:some_one:bob",
            "whisper:somexone:bob",
            "whisper:some%one:bob",
            "whisper:some!one:bob",
        ];
        for (index, channel) in channels.iter().enumerate() {
            store
                .append_message(&message(index as u64, 1000 + index as i64, channel))
                .unwrap();
        }
        for channel in channels {
            let prefix = channel.strip_suffix("bob").unwrap();
            assert_eq!(
                store.recent_channels_with_prefix(prefix, 1).unwrap(),
                vec![channel]
            );
        }
    }

    #[test]
    fn retention_keeps_exact_limit_and_evicts_by_timestamp_then_id() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "WITH RECURSIVE rows(n) AS (
                     VALUES(1) UNION ALL SELECT n + 1 FROM rows WHERE n < ?1
                 )
                 INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
                 SELECT 'busy', n, 'seed-' || n, '{}' FROM rows",
                params![MAX_ROWS_PER_CHANNEL],
            )
            .unwrap();
        }
        store
            .append_message(&message(100_000, MAX_ROWS_PER_CHANNEL + 1, "busy"))
            .unwrap();
        store.append_message(&message(100_001, 0, "busy")).unwrap();
        store
            .append_message(&message(100_002, MAX_ROWS_PER_CHANNEL + 1, "busy"))
            .unwrap();
        store.append_message(&message(100_003, 0, "other")).unwrap();
        let conn = store.conn.lock().unwrap();
        let (count, oldest, newest): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MIN(ts_ms), MAX(ts_ms) FROM chat_messages WHERE channel = 'busy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (count, oldest, newest),
            (MAX_ROWS_PER_CHANNEL, 3, MAX_ROWS_PER_CHANNEL + 1)
        );
        let recorded: i64 = conn
            .query_row(
                "SELECT message_count FROM chat_message_counts WHERE channel = 'busy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, count);
        let other: i64 = conn
            .query_row(
                "SELECT message_count FROM chat_message_counts WHERE channel = 'other'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(other, 1);
    }

    #[test]
    #[ignore = "manual performance comparison; timings depend on the machine"]
    fn perf_log_retention() {
        use std::time::Instant;

        const APPENDS: i64 = 500;
        let messages: Vec<_> = (1..=APPENDS)
            .map(|n| {
                message(
                    (MAX_ROWS_PER_CHANNEL + n) as u64,
                    MAX_ROWS_PER_CHANNEL + n,
                    "busy",
                )
            })
            .collect();
        let mut previous_samples = Vec::new();
        let mut current_samples = Vec::new();

        for round in 0..5 {
            // Alternate order to reduce warm-up bias. Seed and migration are
            // outside the timing; both variants start with 50,000 indexed rows.
            for current in [round % 2 == 0, round % 2 != 0] {
                let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
                {
                    let conn = store.conn.lock().unwrap();
                    if !current {
                        conn.execute_batch(
                            "DROP TRIGGER chat_message_count_insert;
                             DROP TRIGGER chat_message_count_delete;
                             DROP TABLE chat_message_counts;",
                        )
                        .unwrap();
                    }
                    conn.execute(
                        "WITH RECURSIVE rows(n) AS (
                             VALUES(1) UNION ALL SELECT n + 1 FROM rows WHERE n < ?1
                         )
                         INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
                         SELECT 'busy', n, 'seed-' || n, '{}' FROM rows",
                        params![MAX_ROWS_PER_CHANNEL],
                    )
                    .unwrap();
                }

                let started = Instant::now();
                for msg in &messages {
                    if current {
                        store.append_message(msg).unwrap();
                    } else {
                        // The previous append path, including serialization,
                        // locking, insert, offset-based retention and commit.
                        let payload = serde_json::to_string(msg).unwrap();
                        let channel = msg.channel.as_str().to_owned();
                        let server_id = msg.server_id.clone();
                        let mut conn = store.conn.lock().unwrap();
                        let tx = conn.transaction().unwrap();
                        tx.execute(
                            "INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![
                                channel,
                                msg.timestamp.timestamp_millis(),
                                server_id,
                                payload
                            ],
                        )
                        .unwrap();
                        tx.execute(
                            "DELETE FROM chat_messages WHERE id IN (
                                 SELECT id FROM chat_messages WHERE channel = ?1
                                 ORDER BY ts_ms DESC, id DESC LIMIT -1 OFFSET ?2
                             )",
                            params![msg.channel.as_str(), MAX_ROWS_PER_CHANNEL],
                        )
                        .unwrap();
                        tx.commit().unwrap();
                    }
                }
                let elapsed = started.elapsed();
                let conn = store.conn.lock().unwrap();
                let retained: (i64, i64, i64) = conn.query_row(
                    "SELECT COUNT(*), MIN(ts_ms), MAX(ts_ms) FROM chat_messages WHERE channel = 'busy'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                ).unwrap();
                assert_eq!(
                    retained,
                    (
                        MAX_ROWS_PER_CHANNEL,
                        APPENDS + 1,
                        MAX_ROWS_PER_CHANNEL + APPENDS
                    )
                );
                if current {
                    current_samples.push(elapsed);
                } else {
                    previous_samples.push(elapsed);
                }
            }
        }
        previous_samples.sort_unstable();
        current_samples.sort_unstable();
        eprintln!(
            "SQLite {}: {APPENDS} appends at {MAX_ROWS_PER_CHANNEL} retained rows; median of 5, in-memory, dev profile. Previous {:?}; current {:?}; ratio {:.2}x",
            rusqlite::version(),
            previous_samples[2],
            current_samples[2],
            previous_samples[2].as_secs_f64() / current_samples[2].as_secs_f64(),
        );
    }

    #[test]
    fn existing_database_migrates_counts_and_multiple_connections_stay_consistent() {
        let path = temp_db_path("logs-migration");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE chat_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL,
                    ts_ms INTEGER NOT NULL, server_id TEXT, payload_json TEXT NOT NULL
                 );
                 INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
                 VALUES ('channel', 1, 'existing', '{}');",
            )
            .unwrap();
        }
        let first = LogStore::with_db_path(path.clone()).unwrap();
        let second = LogStore::with_db_path(path.clone()).unwrap();
        first.append_message(&message(1, 1000, "channel")).unwrap();
        second.append_message(&message(2, 2000, "channel")).unwrap();
        second.append_message(&message(1, 1000, "channel")).unwrap();
        {
            let mut conn = second.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute("DELETE FROM chat_messages WHERE channel = 'channel'", [])
                .unwrap();
            tx.rollback().unwrap();
        }
        {
            let conn = first.conn.lock().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT message_count FROM chat_message_counts WHERE channel = 'channel'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 3);
            conn.execute("DELETE FROM chat_messages WHERE server_id = 'existing'", [])
                .unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT message_count FROM chat_message_counts WHERE channel = 'channel'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 2);
        }
        drop(first);
        drop(second);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn round_trip_recent_messages() {
        let path = temp_db_path("logs-roundtrip");
        let store = LogStore::with_db_path(path.clone()).expect("create store");
        let channel = ChannelId::new("rustlang");

        let mk = |id: u64, sec: i64, txt: &str| ChatMessage {
            id: MessageId(id),
            server_id: Some(format!("srv-{id}")),
            timestamp: Utc.timestamp_opt(1_700_000_000 + sec, 0).single().unwrap(),
            channel: channel.clone(),
            sender: Sender {
                original_display_name: None,
                user_id: UserId("1".to_owned()),
                login: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: txt.to_owned(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        };

        store.append_message(&mk(1, 0, "one")).expect("append one");
        store.append_message(&mk(2, 1, "two")).expect("append two");
        store
            .append_message(&mk(3, 2, "three"))
            .expect("append three");

        let rows = store
            .recent_messages(&channel, 2)
            .expect("load recent rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].raw_text, "two");
        assert_eq!(rows[1].raw_text, "three");

        let older = store
            .older_messages(&channel, rows[0].timestamp.timestamp_millis(), 8)
            .expect("load older rows");
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].raw_text, "one");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recent_mentions_filters_across_channels_and_ignores_whispers() {
        let path = temp_db_path("logs-mentions");
        let store = LogStore::with_db_path(path.clone()).expect("create store");

        let mk = |id: u64, sec: i64, ch: ChannelId, flags: MessageFlags, txt: &str| ChatMessage {
            id: MessageId(id),
            server_id: Some(format!("srv-{id}")),
            timestamp: Utc.timestamp_opt(1_700_000_000 + sec, 0).single().unwrap(),
            channel: ch,
            sender: Sender {
                original_display_name: None,
                user_id: UserId("1".to_owned()),
                login: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: txt.to_owned(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags,
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        };

        let hl = MessageFlags {
            is_highlighted: true,
            ..Default::default()
        };
        let mention = MessageFlags {
            is_mention: true,
            ..Default::default()
        };
        let plain = MessageFlags::default();

        // Two different real channels + one whisper channel.
        store
            .append_message(&mk(1, 10, ChannelId::new("forsen"), plain.clone(), "plain"))
            .expect("append plain");
        store
            .append_message(&mk(
                2,
                20,
                ChannelId::new("forsen"),
                hl.clone(),
                "highlighted",
            ))
            .expect("append hl");
        store
            .append_message(&mk(
                3,
                30,
                ChannelId::new("xqc"),
                mention.clone(),
                "mention",
            ))
            .expect("append mention");
        // Whisper: must be excluded even if highlighted.
        store
            .append_message(&mk(
                4,
                40,
                ChannelId::new("whisper:alice"),
                hl,
                "whisper-hl",
            ))
            .expect("append whisper");

        store
            .append_message(&mk(
                5,
                50,
                ChannelId::new("partner"),
                MessageFlags {
                    is_highlighted: true,
                    suppress_notification: true,
                    ..Default::default()
                },
                "suppressed shared chat mirror",
            ))
            .expect("append suppressed mirror");

        let rows = store.recent_mentions(10).expect("load mentions");
        let ids: Vec<_> = rows.iter().map(|m| m.raw_text.as_str()).collect();
        assert_eq!(ids, vec!["highlighted", "mention"]);
        let latest = store.recent_mentions(1).expect("load latest mention");
        assert_eq!(latest[0].raw_text, "mention");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mentions_index_migrates_saved_rows_without_rewriting_payloads() {
        let path = temp_db_path("mentions-index-migration");
        let mut messages: Vec<_> = (1..=8).map(|id| message(id, 1000, "room")).collect();
        messages[0].flags.is_highlighted = true;
        messages[1].flags.is_mention = true;
        messages[2].flags.is_first_msg = true;
        messages[3].flags.is_pinned = true;
        messages[4].flags.is_mention = true;
        messages[4].flags.suppress_notification = true;
        messages[5].flags.is_mention = true;
        messages[5].channel = ChannelId("whisper:uid:1:bob".into());
        messages[6].raw_text = r#"quoted flag: "is_mention":true"#.into();
        messages[7].flags.is_mention = true;
        messages[7].channel = ChannelId("whisper:alice".into());
        let payloads: Vec<_> = messages
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let mut value = serde_json::to_value(row).unwrap();
                if index == 1 {
                    value["flags"]
                        .as_object_mut()
                        .unwrap()
                        .remove("suppress_notification");
                }
                if index == 3 {
                    serde_json::to_string_pretty(&value).unwrap()
                } else {
                    serde_json::to_string(&value).unwrap()
                }
            })
            .collect();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE chat_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL,
                    ts_ms INTEGER NOT NULL, server_id TEXT, payload_json TEXT NOT NULL
                 );",
            )
            .unwrap();
            for (row, payload) in messages.iter().zip(&payloads) {
                conn.execute(
                    "INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        row.channel.as_str(),
                        row.timestamp.timestamp_millis(),
                        row.server_id,
                        payload
                    ],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO chat_messages(channel, ts_ms, payload_json)
                 VALUES ('room', 2000, 'invalid archived JSON')",
                [],
            )
            .unwrap();
        }
        {
            let store = LogStore::with_db_path(path.clone()).unwrap();
            let index_exists = || {
                store.conn.lock().unwrap().query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'idx_chat_messages_attention_ts')",
                [], |row| row.get::<_, bool>(0),
            ).unwrap()
            };
            assert!(
                !index_exists(),
                "opening saved logs must defer the migration scan"
            );
            assert_eq!(
                store
                    .recent_mentions(20)
                    .unwrap()
                    .iter()
                    .map(|row| row.id.0)
                    .collect::<Vec<_>>(),
                [1, 2, 3, 4]
            );
            assert!(index_exists());
            assert_eq!(
                store
                    .recent_mentions(2)
                    .unwrap()
                    .iter()
                    .map(|row| row.id.0)
                    .collect::<Vec<_>>(),
                [3, 4]
            );
            let conn = store.conn.lock().unwrap();
            let mut query = conn
                .prepare("SELECT payload_json FROM chat_messages WHERE id <= 8 ORDER BY id")
                .unwrap();
            let saved = query
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(saved, payloads);
        }
        {
            let store = LogStore::with_db_path(path.clone()).unwrap();
            assert_eq!(store.recent_mentions(20).unwrap().len(), 4);
            let conn = store.conn.lock().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM chat_messages", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 9);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mentions_index_tracks_writes_highlights_suppression_and_removal() {
        let path = temp_db_path("mentions-index-updates");
        let first = LogStore::with_db_path(path.clone()).unwrap();
        assert!(first.recent_mentions(10).unwrap().is_empty());
        let second = LogStore::with_db_path(path.clone()).unwrap();
        let row = message(1, 1000, "room");
        second.append_message(&row).unwrap();
        assert!(first.recent_mentions(10).unwrap().is_empty());
        second
            .mark_highlighted(&row.channel, row.server_id.as_deref(), row.id, 1000)
            .unwrap();
        assert_eq!(first.recent_mentions(10).unwrap()[0].id.0, 1);

        let mut next = message(2, 1000, "room");
        next.flags.is_mention = true;
        second.append_message(&next).unwrap();
        assert_eq!(first.recent_mentions(1).unwrap()[0].id.0, 2);
        {
            let conn = second.conn.lock().unwrap();
            conn.execute(
                "UPDATE chat_messages SET payload_json = json_set(payload_json, '$.flags.suppress_notification', json('true'))
                 WHERE server_id = 'message-2'", [],
            ).unwrap();
            conn.execute(
                "INSERT INTO chat_messages(channel, ts_ms, payload_json) VALUES ('room', 3000, 'invalid')", [],
            ).unwrap();
        }
        assert_eq!(
            first
                .recent_mentions(10)
                .unwrap()
                .iter()
                .map(|row| row.id.0)
                .collect::<Vec<_>>(),
            [1]
        );
        second
            .conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM chat_messages WHERE server_id = 'message-1'",
                [],
            )
            .unwrap();
        assert!(first.recent_mentions(10).unwrap().is_empty());
        drop(first);
        drop(second);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn mentions_query_uses_the_attention_index_without_sorting_the_log() {
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let mut attention = message(1, 1000, "room");
        attention.flags.is_mention = true;
        store.append_message(&attention).unwrap();
        for id in 2..=100 {
            store
                .append_message(&message(id, id as i64, "room"))
                .unwrap();
        }
        assert_eq!(store.recent_mentions(1).unwrap()[0].id.0, 1);
        let conn = store.conn.lock().unwrap();
        let mut query = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", recent_mentions_query()))
            .unwrap();
        let plan = query
            .query_map(params![500], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("USING INDEX idx_chat_messages_attention_ts")),
            "{plan:?}"
        );
        assert!(
            plan.iter().all(|line| !line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }

    #[test]
    #[ignore = "manual performance comparison; timings depend on the machine"]
    fn perf_mentions_history() {
        use std::time::Instant;

        const ROWS: i64 = 100_000;
        let store = LogStore::with_db_path(PathBuf::from(":memory:")).unwrap();
        let mut sample = message(1, 1000, "room");
        sample.raw_text = "sample ".repeat(100);
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "WITH RECURSIVE rows(n) AS (
                     VALUES(1) UNION ALL SELECT n + 1 FROM rows WHERE n < ?1
                 )
                 INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
                 SELECT 'room' || (n % 10), n, 'seed-' || n,
                     json_set(?2, '$.id', n, '$.channel', 'room' || (n % 10),
                         '$.flags.is_mention', json(CASE WHEN n % 1000 = 0 THEN 'true' ELSE 'false' END))
                 FROM rows",
                params![ROWS, serde_json::to_string(&sample).unwrap()],
            ).unwrap();
        }
        let started = Instant::now();
        let expected = store.recent_mentions(500).unwrap();
        let migration = started.elapsed();
        assert_eq!(expected.len(), 100);
        let mut previous_samples = Vec::new();
        let mut current_samples = Vec::new();
        for round in 0..5 {
            for current in [round % 2 == 0, round % 2 != 0] {
                let started = Instant::now();
                let messages = if current {
                    store.recent_mentions(500).unwrap()
                } else {
                    let conn = store.conn.lock().unwrap();
                    // The former query used payload substrings and had to
                    // inspect every saved row, even when fewer than 500 matched.
                    let mut query = conn.prepare(
                        r#"SELECT payload_json FROM chat_messages
                           WHERE channel NOT LIKE 'whisper:%'
                             AND CASE WHEN json_valid(payload_json)
                                 THEN COALESCE(json_extract(payload_json, '$.flags.suppress_notification'), 0) = 0
                                 ELSE 0 END
                             AND (payload_json LIKE '%"is_highlighted":true%'
                                 OR payload_json LIKE '%"is_mention":true%'
                                 OR payload_json LIKE '%"is_first_msg":true%'
                                 OR payload_json LIKE '%"is_pinned":true%')
                           ORDER BY ts_ms DESC, id DESC LIMIT 500"#,
                    ).unwrap();
                    let mut rows: Vec<ChatMessage> = query
                        .query_map([], |row| row.get::<_, String>(0))
                        .unwrap()
                        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
                        .collect();
                    rows.reverse();
                    rows
                };
                let elapsed = started.elapsed();
                assert_eq!(
                    messages.iter().map(|row| row.id).collect::<Vec<_>>(),
                    expected.iter().map(|row| row.id).collect::<Vec<_>>()
                );
                if current {
                    current_samples.push(elapsed);
                } else {
                    previous_samples.push(elapsed);
                }
            }
        }
        previous_samples.sort_unstable();
        current_samples.sort_unstable();
        let profile = if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        };
        eprintln!(
            "SQLite {}: mentions from {ROWS} rows, 100 matches; median of 5, in-memory, {profile} profile. Previous {:?}; indexed {:?}; first read including index migration {:?}",
            rusqlite::version(), previous_samples[2], current_samples[2], migration,
        );
    }
}
