use std::path::PathBuf;

use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use tokio::io::AsyncWriteExt;
use crate::StorageError;

/// Append-only per-channel log storage.
/// Format: logs/<channel>/YYYY-MM-DD.log
pub struct LogStore {
    base_dir: PathBuf,
}

impl LogStore {
    pub fn new() -> Result<Self, StorageError> {
        let dirs = ProjectDirs::from("dev", "crust", "crust")
            .ok_or_else(|| StorageError::Io(std::io::Error::other("cannot find config dir")))?;
        let base = dirs.data_dir().join("logs");
        std::fs::create_dir_all(&base)?;
        Ok(Self { base_dir: base })
    }

    fn log_path(&self, channel: &str, ts: &DateTime<Utc>) -> PathBuf {
        let day = ts.format("%Y-%m-%d").to_string();
        let channel_dir = self.base_dir.join(channel);
        channel_dir.join(format!("{day}.log"))
    }

    /// Append a single log line for the given channel.
    pub async fn append(
        &self,
        channel: &str,
        ts: DateTime<Utc>,
        sender: &str,
        text: &str,
    ) -> Result<(), StorageError> {
        let path = self.log_path(channel, &ts);
        // Ensure directory exists.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line = format!(
            "[{}] <{}> {}\n",
            ts.format("%H:%M:%S"),
            sender,
            text
        );
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        Ok(())
    }

    /// Read all lines for a channel on the given date (YYYY-MM-DD).
    pub async fn read(&self, channel: &str, date: &str) -> Result<String, StorageError> {
        let ts: DateTime<Utc> = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|e| StorageError::Serde(e.to_string()))?
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let path = self.log_path(channel, &ts);
        tokio::fs::read_to_string(&path).await.map_err(Into::into)
    }
}
