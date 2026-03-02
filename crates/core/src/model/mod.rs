use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

// Identifiers: types for channel, user, and message IDs

/// Normalized lowercase Twitch channel name (without the leading '#').
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub String);

impl ChannelId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_lowercase().trim_start_matches('#').to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns "#channel" form required by IRC JOIN / PRIVMSG.
    pub fn irc_name(&self) -> String {
        format!("#{}", self.0)
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Twitch numeric or string user-id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub String);

/// Local monotonic message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub u64);

// Badge: Twitch badge metadata

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Badge {
    pub name: String,
    pub version: String,
    /// CDN image URL (1x), populated by the badge loader.
    #[serde(default)]
    pub url: Option<String>,
}

// Sender: chat message sender metadata

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sender {
    pub user_id: UserId,
    /// Raw login name (lowercase).
    pub login: String,
    /// Display name as supplied by the server.
    pub display_name: String,
    /// #rrggbb color from IRC tag, or None.
    pub color: Option<String>,
    pub badges: Vec<Badge>,
}

// Span / Token: parsed chat message chunks

/// A pre-parsed chunk of a chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Span {
    Text {
        text: String,
        /// Whether this span is part of a /me action.
        is_action: bool,
    },
    Emote {
        /// Provider-specific emote ID.
        id: String,
        /// The original text code (e.g. "Kappa").
        code: String,
        /// CDN image URL (1x / normal size).
        url: String,
        /// CDN image URL at higher resolution (4x > 2x > 1x) for tooltip previews.
        url_hd: Option<String>,
        /// Provider name: "twitch", "bttv", "ffz", "7tv".
        provider: String,
    },
    Emoji {
        /// Original emoji text (e.g. "😀").
        text: String,
        /// Twemoji CDN URL.
        url: String,
    },
    Badge {
        name: String,
        version: String,
    },
    Mention {
        login: String,
    },
    Url {
        text: String,
        url: String,
    },
}

// MsgKind: chat message classification

/// Classifies a chat-line for special rendering.  `Chat` is the default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum MsgKind {
    /// Ordinary chat message.
    #[default]
    Chat,
    /// Sub / resub / gift-sub notification (USERNOTICE).
    Sub {
        display_name: String,
        /// Cumulative months subscribed (1 for new subs).
        months: u32,
        /// Human-readable plan: "Prime", "Tier 1", "Tier 2", "Tier 3".
        plan: String,
        /// True when the sub was gifted by another user.
        is_gift: bool,
        /// Optional message typed by the subscriber.
        sub_msg: String,
    },
    /// Incoming raid (USERNOTICE msg-id=raid).
    Raid {
        display_name: String,
        viewer_count: u32,
    },
    /// Target user was timed out.
    Timeout {
        login: String,
        seconds: u32,
    },
    /// Target user was permanently banned.
    Ban { login: String },
    /// A moderator cleared the entire chat.
    ChatCleared,
    /// Generic informational notice (NOTICE, JOIN/PART system message, etc.).
    SystemInfo,
    /// Message containing a bits cheermote donation.
    Bits { amount: u32 },
}

// TwitchEmotePos: Twitch emote position metadata

/// One occurrence of a Twitch-native emote parsed from the `emotes` IRC tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitchEmotePos {
    pub id: String,
    /// Character (code-point) start index, inclusive.
    pub start: usize,
    /// Character (code-point) end index, inclusive.
    pub end: usize,
}

// ChatMessage: chat message structure and flags

/// Flags that modify how a message is displayed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageFlags {
    pub is_action: bool,
    pub is_highlighted: bool,
    pub is_deleted: bool,
    pub is_first_msg: bool,
    pub is_self: bool,
    /// True when the message mentions or replies to the locally logged-in user.
    pub is_mention: bool,
    /// Present when the message was sent via a channel-points custom reward.
    pub custom_reward_id: Option<String>,
    /// True for messages loaded from chat history rather than received live.
    pub is_history: bool,
}

/// Metadata for a message that is a reply to another message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyInfo {
    /// Server-assigned UUID of the parent message (used to send replies back).
    pub parent_msg_id: String,
    /// Lowercase login name of the parent sender.
    pub parent_user_login: String,
    /// Display name of the parent sender.
    pub parent_display_name: String,
    /// Raw text body of the parent message.
    pub parent_msg_body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    /// Server-provided msg-id (for deletion).
    pub server_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub channel: ChannelId,
    pub sender: Sender,
    /// Original unprocessed text.
    pub raw_text: String,
    /// Tokenized for rendering.
    pub spans: SmallVec<[Span; 8]>,
    /// Twitch-native emote positions (from IRC `emotes` tag).
    pub twitch_emotes: Vec<TwitchEmotePos>,
    pub flags: MessageFlags,
    /// Set when this message is a reply to another message.
    pub reply: Option<ReplyInfo>,
    /// What kind of event produced this line.
    pub msg_kind: MsgKind,
}

// EmoteCatalogEntry: lightweight emote entry for UI catalog

/// Lightweight emote entry for the UI catalog (autocomplete / picker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmoteCatalogEntry {
    pub code: String,
    pub provider: String,
    pub url: String,
    /// `"global"` or `"channel"`.
    pub scope: String,
}

// RoomState: Twitch room state metadata

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoomState {
    pub emote_only: bool,
    pub followers_only: Option<i32>, // -1 = off, 0+ = minutes
    pub slow_mode: Option<u32>,      // seconds
    pub subscribers_only: bool,
    pub r9k: bool,
}

// ChannelState: state and message buffer for a channel

const MAX_MESSAGES: usize = 1500;

#[derive(Debug)]
pub struct ChannelState {
    pub id: ChannelId,
    /// Ring-buffer of messages, capped at MAX_MESSAGES.
    pub messages: std::collections::VecDeque<ChatMessage>,
    pub room_state: RoomState,
    /// Chatters currently in the channel (from NAMES / JOIN / PART).
    pub chatters: std::collections::HashSet<String>,
    /// Total new messages received while this channel was not active (excluding history).
    pub unread_count: u32,
    /// Subset of unread_count that are mentions or Twitch highlights — shown in amber.
    pub unread_mentions: u32,
    /// Whether the logged-in user is a moderator in this channel.
    pub is_mod: bool,
}

impl ChannelState {
    pub fn new(id: ChannelId) -> Self {
        Self {
            id,
            messages: std::collections::VecDeque::with_capacity(MAX_MESSAGES),
            room_state: RoomState::default(),
            chatters: std::collections::HashSet::new(),
            unread_count: 0,
            unread_mentions: 0,
            is_mod: false,
        }
    }

    pub fn push_message(&mut self, msg: ChatMessage) {
        if self.messages.len() >= MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }

    /// Clear unread counters (call when the user switches to this channel).
    pub fn mark_read(&mut self) {
        self.unread_count = 0;
        self.unread_mentions = 0;
    }

    pub fn delete_message(&mut self, server_id: &str) {
        if let Some(m) = self
            .messages
            .iter_mut()
            .find(|m| m.server_id.as_deref() == Some(server_id))
        {
            m.flags.is_deleted = true;
        }
    }

    /// Mark every non-deleted message from `login` as deleted (timeout/ban).
    pub fn delete_messages_from(&mut self, login: &str) {
        for m in &mut self.messages {
            if m.msg_kind == MsgKind::Chat
                && m.sender.login.eq_ignore_ascii_case(login)
                && !m.flags.is_deleted
            {
                m.flags.is_deleted = true;
            }
        }
    }

    /// Prepend historical messages (e.g. from recent-messages API) to the
    /// front of the buffer.  Duplicates (matched by `server_id`) are skipped,
    /// and the total remains bounded by `MAX_MESSAGES`.
    pub fn prepend_history(&mut self, mut msgs: Vec<ChatMessage>) {
        // Build a set of already-known server IDs to skip duplicates.
        let existing_ids: std::collections::HashSet<&str> = self
            .messages
            .iter()
            .filter_map(|m| m.server_id.as_deref())
            .collect();
        msgs.retain(|m| {
            m.server_id
                .as_deref()
                .map(|id| !existing_ids.contains(id))
                .unwrap_or(true)
        });

        // Respect the ring-buffer cap.
        let available = MAX_MESSAGES.saturating_sub(self.messages.len());
        if msgs.len() > available {
            // Drop the oldest history (the beginning of the slice) when we
            // would exceed MAX_MESSAGES.
            msgs.drain(0..msgs.len() - available);
        }

        // Push oldest-first to the front so chronological order is preserved.
        for msg in msgs.into_iter().rev() {
            self.messages.push_front(msg);
        }
    }
}

// UserProfile: Twitch user profile metadata

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
}

// SystemNotice: system notice event structure

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemNotice {
    pub channel: Option<ChannelId>,
    pub text: String,
    pub timestamp: DateTime<Utc>,
}
