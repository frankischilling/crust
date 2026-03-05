use serde::{Deserialize, Serialize};

use crate::model::{
    Badge, ChannelId, ChatMessage, EmoteCatalogEntry, ReplyInfo, SystemNotice,
    UserProfile,
};

/// A single chat log entry from the IVR logs API.
#[derive(Debug, Clone)]
pub struct IvrLogEntry {
    /// Message text.
    pub text: String,
    /// ISO 8601 timestamp, e.g. "2026-03-05T09:35:03.061Z".
    pub timestamp: String,
    /// Display name of the sender.
    pub display_name: String,
    /// 1 = normal message, 2 = timeout/ban event.
    pub msg_type: u8,
}
// LinkPreview: Open Graph / Twitter Card metadata for a URL

/// Open Graph / Twitter Card metadata fetched for a URL.
#[derive(Debug, Clone)]
pub struct LinkPreview {
    pub title: Option<String>,
    pub description: Option<String>,
    /// og:image URL (thumbnail). The image is fetched into `emote_bytes`.
    pub thumbnail_url: Option<String>,
    /// Site name from `og:site_name` (e.g. "YouTube", "Twitter").
    pub site_name: Option<String>,
    /// True once the fetch attempt has completed (even if it returned nothing).
    pub fetched: bool,
}
// Commands (UI to runtime): actions initiated by the UI

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppCommand {
    /// Join a channel.
    JoinChannel { channel: ChannelId },
    /// Join an IRC channel with an optional channel key.
    JoinIrcChannel {
        channel: ChannelId,
        key: Option<String>,
    },
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
    SendMessage {
        channel: ChannelId,
        text: String,
        /// If set, the message is a reply to this server-assigned message ID.
        reply_to_msg_id: Option<String>,
        /// Optional reply context used for local-echo rendering.
        #[serde(default)]
        reply: Option<ReplyInfo>,
    },
    /// Request a Twitch user profile lookup by login name.
    FetchUserProfile { login: String },
    /// Timeout a user in a channel via the Twitch Helix API.
    TimeoutUser {
        channel: ChannelId,
        login: String,
        user_id: String,
        seconds: u32,
        reason: Option<String>,
    },
    /// Permanently ban a user from a channel via the Twitch Helix API.
    BanUser {
        channel: ChannelId,
        login: String,
        user_id: String,
        reason: Option<String>,
    },
    /// Lift an active ban or timeout for a user via the Twitch Helix API.
    UnbanUser {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    /// Clears all messages in the channel display (visual-only, not sent to Twitch).
    ClearLocalMessages { channel: ChannelId },
    /// Opens a URL in the system default browser.
    OpenUrl { url: String },
    /// Injects a local informational message into a channel's feed (not sent to Twitch).
    InjectLocalMessage { channel: ChannelId, text: String },
    /// Opens the user-card popup for the given login in a channel.
    ShowUserCard { login: String, channel: ChannelId },
    /// Fetch Open-Graph / Twitter-Card metadata for a URL to show a hover preview.
    FetchLinkPreview { url: String },
    /// Add a new account by validating and saving the given OAuth token.
    AddAccount { token: String },
    /// Switch the active session to an already-saved account.
    SwitchAccount { username: String },
    /// Remove a saved account (and its token) permanently.
    RemoveAccount { username: String },
    /// Mark an account as the one to auto-login on next startup.
    SetDefaultAccount { username: String },
    /// Set the IRC nickname used for generic IRC servers.
    SetIrcNick { nick: String },
    /// Set NickServ credentials for automatic IRC identification.
    SetIrcAuth {
        nickserv_user: String,
        nickserv_pass: String,
    },
    /// Persist beta transport feature toggles.
    SetBetaFeatures {
        kick_enabled: bool,
        irc_enabled: bool,
    },
    /// Toggle always-on-top window mode.
    SetAlwaysOnTop { enabled: bool },
    /// Switch UI theme ("dark" or "light") and persist to settings.
    SetTheme { theme: String },
    /// Fetch external chat logs for a user from the IVR logs API.
    FetchIvrLogs { channel: String, username: String },
}

// Events (runtime to UI): notifications sent from runtime to UI

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
    /// An IRC channel redirect occurred (e.g. #chat → ##chat on Libera).
    /// The UI should replace the old channel tab with the new one.
    ChannelRedirected {
        old_channel: ChannelId,
        new_channel: ChannelId,
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
    /// Full snapshot of the emote catalog (sent after each load).
    EmoteCatalogUpdated {
        emotes: Vec<EmoteCatalogEntry>,
    },
    /// Logged out / reverted to anonymous.
    LoggedOut,
    Error {
        context: String,
        message: String,
    },
    /// Historical messages loaded from an external API (e.g. recent-messages).
    /// Should be prepended to the channel's message buffer.
    HistoryLoaded {
        channel: ChannelId,
        messages: Vec<ChatMessage>,
    },
    /// Twitch user profile loaded from the IVR API.
    UserProfileLoaded {
        profile: UserProfile,
    },
    /// A user profile lookup finished without data (network/API/user not found).
    UserProfileUnavailable {
        login: String,
    },
    /// Mark all visible messages from a user as deleted (timeout / ban).
    UserMessagesCleared {
        channel: ChannelId,
        login: String,
    },
    /// USERSTATE received - badges, color and mod status for the logged-in user.
    UserStateUpdated {
        channel: ChannelId,
        is_mod: bool,
        badges: Vec<crate::model::Badge>,
        color: Option<String>,
    },
    /// Clear all messages from the given channel's UI buffer (response to ClearLocalMessages).
    ChannelMessagesCleared {
        channel: ChannelId,
    },
    /// The logged-in user's avatar URL has been resolved.
    SelfAvatarLoaded {
        avatar_url: String,
    },
    /// Open-Graph / Twitter-Card metadata is ready for a URL.
    LinkPreviewReady {
        url: String,
        title: Option<String>,
        description: Option<String>,
        /// og:image URL; the image bytes land in emote_bytes under this key.
        thumbnail_url: Option<String>,
        /// Site name from `og:site_name` (e.g. "YouTube", "Twitter").
        site_name: Option<String>,
    },
    /// The set of saved accounts or the active account changed.
    AccountListUpdated {
        /// Ordered list of all saved account usernames.
        accounts: Vec<String>,
        /// Username of the currently active (authenticated) account, if any.
        active: Option<String>,
        /// Username of the account that auto-logs in on startup, if set.
        default: Option<String>,
    },
    /// The topic for an IRC channel was set or changed.
    IrcTopicChanged {
        channel: ChannelId,
        topic: String,
    },
    /// Channel emote catalog loaded (including 0 when none exist).
    ChannelEmotesLoaded {
        channel: ChannelId,
        count: usize,
    },
    /// Beta transport feature toggles loaded/updated from settings.
    BetaFeaturesUpdated {
        kick_enabled: bool,
        irc_enabled: bool,
        /// NickServ username for IRC auto-identification.
        irc_nickserv_user: String,
        /// NickServ password for IRC auto-identification.
        irc_nickserv_pass: String,
        /// Whether always-on-top is enabled.
        always_on_top: bool,
    },
    /// A batch of image prefetch tasks has been queued.  The loading screen
    /// uses this to track progress vs `EmoteImageReady` completions.
    ImagePrefetchQueued {
        count: usize,
    },
    /// Twitch ROOMSTATE tags updated - room modes for a channel.
    RoomStateUpdated {
        channel: ChannelId,
        emote_only: Option<bool>,
        followers_only: Option<i32>,
        slow: Option<u32>,
        subs_only: Option<bool>,
        r9k: Option<bool>,
    },
    /// 7TV cosmetics resolved for a Twitch user id; UI should update visible
    /// messages from this sender so badges appear without waiting for
    /// a new message.
    SenderCosmeticsUpdated {
        user_id: String,
        color: Option<String>,
        badge: Option<Badge>,
        /// 7TV animated avatar URL (if the user has one set).
        avatar_url: Option<String>,
    },
    /// External IVR chat logs loaded for a user.
    IvrLogsLoaded {
        username: String,
        messages: Vec<IvrLogEntry>,
    },
    /// External IVR log fetch failed.
    IvrLogsFailed {
        username: String,
        error: String,
    },
}

// ConnectionState: connection status enumeration

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
