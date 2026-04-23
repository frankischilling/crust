//! Append-only JSON log of successful uploads.
//!
//! Format matches Chatterino's `ImageUploader.json`: a single JSON array of
//! entries, rewritten in full on each successful upload.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::UploadError;

pub const LOG_FILE_NAME: &str = "ImageUploader.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    #[serde(rename = "channelName")]
    pub channel_name: String,
    #[serde(rename = "deletionLink", skip_serializing_if = "Option::is_none")]
    pub deletion_link: Option<String>,
    #[serde(rename = "imageLink")]
    pub image_link: String,
    #[serde(rename = "localPath", skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    pub timestamp: i64,
}

pub fn log_file_path(dir: &Path) -> PathBuf {
    dir.join(LOG_FILE_NAME)
}

/// Append a log entry to `dir/ImageUploader.json`. Creates the file + parent
/// directory if missing. Best-effort: invalid existing contents are replaced.
pub fn append_log_entry(
    dir: &Path,
    channel_name: &str,
    image_link: &str,
    deletion_link: Option<&str>,
    local_path: Option<&Path>,
) -> Result<(), UploadError> {
    std::fs::create_dir_all(dir).map_err(|e| UploadError::Io(e.to_string()))?;
    let path = log_file_path(dir);

    let entries: Vec<Value> = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_else(|_| Vec::new()),
        _ => Vec::new(),
    };

    let entry = LogEntry {
        channel_name: channel_name.to_owned(),
        deletion_link: deletion_link
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned()),
        image_link: image_link.to_owned(),
        local_path: local_path.map(|p| p.display().to_string()),
        timestamp: Utc::now().timestamp(),
    };

    let mut combined: Vec<Value> = entries;
    combined.push(serde_json::to_value(&entry).map_err(|e| UploadError::Json(e.to_string()))?);
    let text =
        serde_json::to_string_pretty(&combined).map_err(|e| UploadError::Json(e.to_string()))?;
    std::fs::write(&path, text).map_err(|e| UploadError::Io(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn append_writes_and_accumulates() {
        let tmp = env::temp_dir().join(format!("crust_upload_log_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        append_log_entry(
            &tmp,
            "#test",
            "https://x/1.png",
            Some("https://x/del/1"),
            None,
        )
        .unwrap();
        append_log_entry(&tmp, "#test", "https://x/2.png", None, None).unwrap();

        let text = std::fs::read_to_string(log_file_path(&tmp)).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["imageLink"], "https://x/1.png");
        assert_eq!(parsed[0]["deletionLink"], "https://x/del/1");
        assert_eq!(parsed[1]["imageLink"], "https://x/2.png");
        assert!(parsed[1].get("deletionLink").map_or(true, |v| v.is_null()
            || v.as_str().map_or(true, str::is_empty)));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
