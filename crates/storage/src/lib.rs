pub mod settings;
pub mod logs;

pub use settings::{AccountEntry, AppSettings, SettingsStore};
pub use logs::LogStore;

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
