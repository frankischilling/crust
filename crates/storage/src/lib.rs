pub mod logs;
pub mod settings;

pub use logs::LogStore;
pub use settings::{AccountEntry, AppSettings, SettingsStore};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(String),
    #[error("Keyring error: {0}")]
    Keyring(String),
}
