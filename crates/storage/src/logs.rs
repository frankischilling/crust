use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, NaiveDate, Utc};
use crust_core::model::{ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};
use directories::ProjectDirs;
use rusqlite::{params, Connection};

use crate::StorageError;

const SQLITE_LOG_FILENAME: &str = "chat_logs.sqlite3";
const DEFAULT_RECENT_LIMIT: usize = 800;
const MAX_ROWS_PER_CHANNEL: i64 = 50_000;

/// SQLite-backed chat log store.
///
/// This replaces legacy flat-file day logs with indexed per-channel history.
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
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
        let channel = msg.channel.as_str().to_owned();
        let server_id = msg.server_id.clone();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StorageError::Io(std::io::Error::other("chat log DB mutex poisoned")))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO chat_messages(channel, ts_ms, server_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![channel, ts_ms, server_id, payload],
        )?;
        // Keep only the most recent rows for each channel.
        tx.execute(
            "DELETE FROM chat_messages
             WHERE id IN (
               SELECT id
               FROM chat_messages
               WHERE channel = ?1
               ORDER BY ts_ms DESC, id DESC
               LIMIT -1 OFFSET ?2
             )",
            params![msg.channel.as_str(), MAX_ROWS_PER_CHANNEL],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Load recent messages for a channel, oldest → newest.
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

    /// Load older messages for a channel before `before_ts_ms`, oldest → newest.
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
            },
            reply: None,
            msg_kind: MsgKind::Chat,
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
}
