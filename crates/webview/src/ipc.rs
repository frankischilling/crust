//! Parsed shape of messages sent from injected JS via `window.ipc.postMessage`.
//!
//! The JS side uses `JSON.stringify({ kind: "...", ... })`. Parsing lives in
//! Rust so we can pattern-match instead of threading `serde_json::Value`
//! everywhere.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingMessage {
    /// `auth-token` cookie presence report.
    Login { logged_in: bool },
    /// The claim button existed and was clicked. Actual balance delta shows
    /// up on a subsequent `Balance` tick (Twitch updates the DOM async after
    /// the click).
    Claimed,
    /// Current on-screen channel-points balance, already un-k/m-suffixed.
    Balance { value: u64 },
    /// Something threw inside the injected script. Not fatal - logged only.
    Error { location: String, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown kind: {0}")]
    UnknownKind(String),
}

impl IncomingMessage {
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        #[derive(Deserialize)]
        struct Envelope {
            kind: String,
            #[serde(default)]
            logged_in: Option<bool>,
            #[serde(default)]
            value: Option<u64>,
            #[serde(default, rename = "where")]
            location: Option<String>,
            #[serde(default, rename = "msg")]
            message: Option<String>,
        }
        let env: Envelope = serde_json::from_str(raw)?;
        match env.kind.as_str() {
            "login" => Ok(IncomingMessage::Login {
                logged_in: env.logged_in.unwrap_or(false),
            }),
            "claimed" => Ok(IncomingMessage::Claimed),
            "balance" => Ok(IncomingMessage::Balance {
                value: env.value.unwrap_or(0),
            }),
            "error" => Ok(IncomingMessage::Error {
                location: env.location.unwrap_or_default(),
                message: env.message.unwrap_or_default(),
            }),
            other => Err(ParseError::UnknownKind(other.to_owned())),
        }
    }
}
