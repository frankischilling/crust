use serde::{Deserialize, Serialize};

use crate::model::{ChannelId, ChatMessage, SystemNotice};

// ─── Commands (UI → runtime) ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppCommand {
    /// Join a channel.
    JoinChannel { channel: ChannelId },
    /// Leave / close a tab.
    LeaveChannel { channel: ChannelId },
    /// Request emote loading for a channel (by Twitch user-id).
    LoadChannelEmotes { channel_twitch_id: String },
    /// Fetch a single image on-demand (e.g. HD emote on hover).
    FetchImage { url: String },
    /// Log in with a Twitch OAuth token.
    Login { token: String },
    /// Log out and switch back to anonymous mode.
    Logout,
    /// Send a chat message to a channel (requires auth).
    SendMessage { channel: ChannelId, text: String },
}

// ─── Events (runtime → UI) ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AppEvent {
    ConnectionStateChanged {
        state: ConnectionState,
    },
    ChannelJoined {
        channel: ChannelId,
    },
    ChannelParted {
        channel: ChannelId,
    },
    MessageReceived {
        channel: ChannelId,
        message: ChatMessage,
    },
    MessageDeleted {
        channel: ChannelId,
        server_id: String,
    },
    SystemNotice(SystemNotice),
    /// Raw image bytes are ready; egui loaders handle decoding + animation.
    EmoteImageReady {
        uri: String,
        width: u32,
        height: u32,
        raw_bytes: Vec<u8>,
    },
    /// Authenticated successfully with Twitch.
    Authenticated {
        username: String,
        user_id: String,
    },
    /// Logged out / reverted to anonymous.
    LoggedOut,
    Error {
        context: String,
        message: String,
    },
}

// ─── ConnectionState ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
    Error(String),
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::Disconnected
    }
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Disconnected"),
            ConnectionState::Connecting => write!(f, "Connecting…"),
            ConnectionState::Connected => write!(f, "Connected"),
            ConnectionState::Reconnecting { attempt } => {
                write!(f, "Reconnecting (attempt {attempt})…")
            }
            ConnectionState::Error(e) => write!(f, "Error: {e}"),
        }
    }
}
