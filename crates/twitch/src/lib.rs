pub mod eventsub;
pub mod irc;
pub mod providers;
pub mod session;

pub use irc::parse_line;
pub use session::client::{parse_privmsg_irc, TwitchSession};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TwitchError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("IRC parse error: {0}")]
    IrcParse(String),
    #[error("Not connected")]
    NotConnected,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
