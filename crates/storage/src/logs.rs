use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, NaiveDate, Utc};
use crust_core::model::{ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};
use directories::ProjectDirs;
use rusqlite::{params, Connection, TransactionBehavior};

use crate::StorageError;

const SQLITE_LOG_FILENAME: &str = "chat_logs.sqlite3";
const DEFAULT_RECENT_LIMIT: usize = 800;
const MAX_ROWS_PER_CHANNEL: i64 = 50_000;

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
            "#,
        )?;
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

    /// Load recent messages for a channel, oldest -> newest.
    pub fn recent_messages(
        &self,
        channel: &ChannelId,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, StorageError> {
        let safe_limit = limit.clamp(1, 5_000) as i64;
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let mut stmt = conn.prepare(
            "SELECT payload_json
             FROM chat_messages
             WHERE channel = ?1
             ORDER BY ts_ms DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![channel.as_str(), safe_limit], |row| {
            row.get::<_, String>(0)
        })?;

        let mut out: Vec<ChatMessage> = Vec::new();
        for row in rows {
            let payload = row?;
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&payload) {
                out.push(msg);
            }
        }
        out.reverse(); // oldest first for prepend behavior
        Ok(out)
    }

    /// Load the most-recent cross-channel *mention-matching* messages, oldest
    /// -> newest. A message qualifies if its persisted JSON payload has any
    /// of the "attention" flags set (`is_highlighted`, `is_mention`,
    /// `is_first_msg`, `is_pinned`). Whispers are excluded (they have their
    /// own dedicated UI feed via `whisper:` channel prefix).
    ///
    /// Uses SQLite LIKE patterns against the serialised JSON as a cheap
    /// pre-filter, then validates by parsing the payload - this keeps the
    /// scan bounded to a small slice of the full `chat_messages` table on
    /// realistic datasets without needing a schema migration.
    pub fn recent_mentions(&self, limit: usize) -> Result<Vec<ChatMessage>, StorageError> {
        let safe_limit = limit.clamp(1, 5_000) as i64;
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        // serde emits exactly these substrings for `true` bool fields.
        let mut stmt = conn.prepare(
            r#"SELECT payload_json
             FROM chat_messages
             WHERE channel NOT LIKE 'whisper:%'
               AND CASE WHEN json_valid(payload_json)
                   THEN COALESCE(json_extract(payload_json, '$.flags.suppress_notification'), 0) = 0
                   ELSE 0 END
               AND (
                 payload_json LIKE '%"is_highlighted":true%'
                 OR payload_json LIKE '%"is_mention":true%'
                 OR payload_json LIKE '%"is_first_msg":true%'
                 OR payload_json LIKE '%"is_pinned":true%'
               )
             ORDER BY ts_ms DESC, id DESC
             LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![safe_limit], |row| row.get::<_, String>(0))?;

        let mut out: Vec<ChatMessage> = Vec::new();
        for row in rows {
            let payload = row?;
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&payload) {
                // Double-check flags in-process in case the LIKE pre-filter
                // false-positives (e.g. a user typed `"is_mention":true` in
                // chat and it slipped through escaping).
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
        let safe_limit = limit.clamp(1, 5_000) as i64;
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let mut stmt = conn.prepare(
            "SELECT payload_json
             FROM chat_messages
             WHERE channel = ?1 AND ts_ms < ?2
             ORDER BY ts_ms DESC, id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![channel.as_str(), before_ts_ms, safe_limit], |row| {
            row.get::<_, String>(0)
        })?;

        let mut out: Vec<ChatMessage> = Vec::new();
        for row in rows {
            let payload = row?;
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&payload) {
                out.push(msg);
            }
        }
        out.reverse(); // oldest first for prepend behavior
        Ok(out)
    }

    /// List distinct channel ids that begin with `prefix`, ordered by most
    /// recent message timestamp descending.
    pub fn recent_channels_with_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let safe_limit = limit.clamp(1, 1_000) as i64;
        let like_pattern = format!("{prefix}%");
        let conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let mut stmt = conn.prepare(
            "SELECT channel
             FROM chat_messages
             WHERE channel LIKE ?1
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
                    msg.raw_text
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
}
