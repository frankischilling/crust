pub mod api;
pub mod session;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KickError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Channel not found: {0}")]
    ChannelNotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
