use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Twitch user profile fetched from the IVR API (no auth required).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub login: String,
    pub display_name: String,
    /// Channel description / bio.
    pub description: String,
    /// ISO 8601 creation timestamp, e.g. `"2013-06-15T19:21:06Z"`.
    pub created_at: Option<String>,
    /// CDN URL for the user's avatar image.
    pub avatar_url: Option<String>,
    pub followers: Option<u64>,
    pub is_partner: bool,
    pub is_affiliate: bool,

    // Extended fields (IVR v2)
    /// The user's chosen chat-message colour as a CSS hex string, e.g. `"#FF6905"`.
    pub chat_color: Option<String>,
    /// `true` if the user is currently live.
    pub is_live: bool,
    /// Title of the active stream (only populated when `is_live`).
    pub stream_title: Option<String>,
    /// Game / category of the active stream.
    pub stream_game: Option<String>,
    /// Live viewer count.
    pub stream_viewers: Option<u64>,
    /// ISO 8601 timestamp when the current (or last) broadcast started.
    pub last_broadcast_at: Option<String>,
    /// `true` if the account is suspended / permanently banned on Twitch.
    pub is_banned: bool,
    /// Reason the account was banned (if known).
    pub ban_reason: Option<String>,
}

/// System notice event structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemNotice {
    pub channel: Option<super::ChannelId>,
    pub text: String,
    pub timestamp: DateTime<Utc>,
}
