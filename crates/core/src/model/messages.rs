use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

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

/// Classifies a chat-line for special rendering. `Chat` is the default.
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
        #[serde(default)]
        source_login: Option<String>,
    },
    /// Hype train status row (begin/progress/end).
    HypeTrain {
        phase: String,
        train_id: String,
        level: u32,
        progress: u64,
        goal: u64,
        total: u64,
        top_contributor_login: Option<String>,
        top_contributor_type: Option<String>,
        top_contributor_total: Option<u64>,
        ends_at: Option<String>,
    },
    /// Target user was timed out.
    Timeout { login: String, seconds: u32 },
    /// Target user was permanently banned.
    Ban { login: String },
    /// A moderator cleared the entire chat.
    ChatCleared,
    /// Generic informational notice (NOTICE, JOIN/PART system message, etc.).
    SystemInfo,
    /// Channel points redemption notification (EventSub).
    ChannelPointsReward {
        user_login: String,
        reward_title: String,
        cost: u32,
        reward_id: Option<String>,
        redemption_id: Option<String>,
        user_input: Option<String>,
        status: Option<String>,
    },
    /// Suspicious-user body message (EventSub low-trust chat line).
    SuspiciousUserMessage,
    /// Message containing a bits cheermote donation.
    Bits { amount: u32 },
}

/// One occurrence of a Twitch-native emote parsed from the `emotes` IRC tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitchEmotePos {
    pub id: String,
    /// Character (code-point) start index, inclusive.
    pub start: usize,
    /// Character (code-point) end index, inclusive.
    pub end: usize,
}

/// Flags that modify how a message is displayed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageFlags {
    pub is_action: bool,
    pub is_highlighted: bool,
    pub is_deleted: bool,
    pub is_first_msg: bool,
    /// Twitch pinned chat / Hype Chat (`pinned-chat-paid-*` IRC tags).
    #[serde(default)]
    pub is_pinned: bool,
    pub is_self: bool,
    /// True when the message mentions or replies to the locally logged-in user.
    pub is_mention: bool,
    /// Present when the message was sent via a channel-points custom reward.
    pub custom_reward_id: Option<String>,
    /// True for messages loaded from chat history rather than received live.
    pub is_history: bool,
    /// Equivalent to Chatterino's `MessageFlag::DoNotTriggerNotification`:
    /// skip sound / desktop toast / mentions-feed side effects even when the
    /// message is highlighted. Set for Shared Chat mirrors whose source
    /// channel is also currently open.
    #[serde(default)]
    pub suppress_notification: bool,
}

/// Metadata for messages sent in a Shared Chat session (Twitch cross-channel
/// mirroring). Populated when the incoming message's `source-room-id` IRC tag
/// (or EventSub `source_broadcaster_user_id`) differs from the current room.
///
/// Upstream: see Chatterino `MessageBuilder::parseSharedChatInfo` + Twitch
/// help article <https://help.twitch.tv/s/article/shared-chat>.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SharedChatSource {
    /// Numeric broadcaster user-id of the source channel.
    pub room_id: String,
    /// Source-channel's msg-id tag, if present (`source-id`).
    pub source_message_id: Option<String>,
    /// Login of the source channel, resolved when known.
    #[serde(default)]
    pub login: Option<String>,
    /// Display name of the source channel, resolved when known.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Source-channel profile picture URL (for the shared-chat badge).
    #[serde(default)]
    pub profile_url: Option<String>,
    /// Mod/VIP badges issued by the source channel (Chatterino only appends
    /// moderator + vip from `source-badges` since other slots already overlap
    /// with normal `badges`).
    #[serde(default)]
    pub badges: Vec<super::Badge>,
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
    pub id: super::MessageId,
    /// Server-provided msg-id (for deletion).
    pub server_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub channel: super::ChannelId,
    pub sender: super::Sender,
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
    /// Populated when this message is a mirrored Shared Chat delivery from
    /// another channel; `None` means local to `channel`.
    #[serde(default)]
    pub shared: Option<SharedChatSource>,
}

/// Lightweight emote entry for the UI catalog (autocomplete / picker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmoteCatalogEntry {
    pub code: String,
    pub provider: String,
    pub url: String,
    /// "global" or "channel".
    pub scope: String,
}

#[cfg(test)]
mod tests {
    use super::MsgKind;

    #[test]
    fn raid_deserializes_without_source_login() {
        let msg_kind: MsgKind =
            serde_json::from_str(r#"{"Raid":{"display_name":"Raider","viewer_count":42}}"#).unwrap();

        match msg_kind {
            MsgKind::Raid {
                display_name,
                viewer_count,
                source_login,
            } => {
                assert_eq!(display_name, "Raider");
                assert_eq!(viewer_count, 42);
                assert_eq!(source_login, None);
            }
            other => panic!("unexpected message kind: {other:?}"),
        }
    }
}
