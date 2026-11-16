use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// A pre-parsed chunk of a chat message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        /// CDN image URL for normal chat rendering.
        url: String,
        /// Optional higher-resolution CDN image for tooltip previews.
        url_hd: Option<String>,
        /// Provider name: "twitch", "bttv", "ffz", "7tv".
        provider: String,
        /// Whether this emote overlays a preceding emote in the same message.
        #[serde(default)]
        zero_width: bool,
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
    /// Moderator announcement with an ordinary sender and chat body.
    Announcement {
        /// Twitch banner color: primary, blue, green, orange, or purple.
        #[serde(default)]
        color: String,
    },
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

impl MsgKind {
    pub fn subscription_notice_text(&self) -> Option<String> {
        let Self::Sub { display_name, months, plan, is_gift, .. } = self else {
            return None;
        };
        Some(if *is_gift {
            format!("{display_name} received a gifted {plan} subscription! ({months} months total)")
        } else if *months <= 1 {
            format!("{display_name} subscribed with {plan}!")
        } else {
            format!("{display_name} resubscribed with {plan}! ({months} months)")
        })
    }
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
    /// Skip sound, desktop toast, and mentions-feed side effects even when the
    /// message is highlighted. Set for Shared Chat mirrors whose source
    /// channel is also currently open.
    #[serde(default)]
    pub suppress_notification: bool,
}

/// Metadata for messages sent in a Shared Chat session (Twitch cross-channel
/// mirroring). Populated when the incoming message's `source-room-id` IRC tag
/// (or EventSub `source_broadcaster_user_id`) differs from the current room.
///
/// See <https://help.twitch.tv/s/article/shared-chat>.
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
    /// Moderator and VIP badges issued by the source channel. Other badge slots
    /// overlap with the message's regular badges.
    #[serde(default)]
    pub badges: Vec<super::Badge>,
}

/// Metadata for a message that is a reply to another message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyInfo {
    /// Server-assigned UUID of the parent message (used to send replies back).
    pub parent_msg_id: String,
    /// Lowercase login name of the parent sender.
    pub parent_user_login: String,
    /// Display name of the parent sender.
    pub parent_display_name: String,
    /// Raw text body of the parent message.
    pub parent_msg_body: String,
    /// Original message at the start of a reply thread, including nested replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_parent_msg_id: Option<String>,
    /// Login of the original thread author, when supplied by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_parent_user_login: Option<String>,
}

impl ReplyInfo {
    pub fn for_message(message: &ChatMessage) -> Option<Self> {
        if message.flags.is_deleted || !message.channel.is_twitch() {
            return None;
        }
        let parent_msg_id = message.server_id.as_deref()?.trim();
        if parent_msg_id.is_empty() { return None; }
        let (thread_parent_msg_id, thread_parent_user_login) = match &message.reply {
            Some(reply) => (reply.thread_parent_msg_id.clone(), reply.thread_parent_user_login.clone()),
            None => (Some(parent_msg_id.to_owned()), Some(message.sender.login.clone())),
        };
        Some(Self {
            parent_msg_id: parent_msg_id.to_owned(),
            parent_user_login: message.sender.login.clone(),
            parent_display_name: message.sender.display_name.clone(),
            parent_msg_body: message.raw_text.clone(),
            thread_parent_msg_id,
            thread_parent_user_login,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: super::MessageId,
    /// Server-provided msg-id (for deletion).
    pub server_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub channel: super::ChannelId,
    pub sender: super::Sender,
    /// Original body retained when local text rules rewrite a self message.
    /// Confirmations match this body even when display rules change while queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_text: Option<String>,
    /// Chat body after local text rules have been applied.
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

impl ChatMessage {
    /// Body used to match socket confirmations to pending local echoes.
    pub fn confirmation_text(&self) -> &str {
        self.original_text.as_deref().unwrap_or(&self.raw_text)
    }

    /// Text for search and copying, including subscription messages in older logs.
    pub fn plain_text(&self) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;

        let MsgKind::Sub { sub_msg, .. } = &self.msg_kind else {
            return Cow::Borrowed(&self.raw_text);
        };
        if sub_msg.trim().is_empty() {
            if self.raw_text.trim().is_empty() {
                return Cow::Owned(self.msg_kind.subscription_notice_text().unwrap_or_default());
            }
            return Cow::Borrowed(&self.raw_text);
        }
        if self.raw_text == *sub_msg || self.raw_text.is_empty() {
            let notice = self.msg_kind.subscription_notice_text().unwrap_or_default();
            return Cow::Owned(format!("{notice}\n{sub_msg}"));
        }
        if self.raw_text.strip_suffix(sub_msg).is_some_and(|prefix| prefix.ends_with('\n')) {
            return Cow::Borrowed(&self.raw_text);
        }
        Cow::Owned(format!("{}\n{sub_msg}", self.raw_text.trim_end()))
    }

    /// Match a live pending own echo without consulting historical messages.
    /// Callers take the oldest matching candidate to preserve repeated sends.
    pub fn is_pending_own_echo_of(&self, confirmed: &Self) -> bool {
        self.server_id.is_none()
            && matches!(self.msg_kind, MsgKind::Chat | MsgKind::Bits { .. })
            && matches!(confirmed.msg_kind, MsgKind::Chat | MsgKind::Bits { .. })
            && self.flags.is_self
            && !self.flags.is_history
            && confirmed.flags.is_self
            && !confirmed.flags.is_history
            && confirmed
                .server_id
                .as_ref()
                .is_some_and(|id| !id.is_empty())
            && self.channel == confirmed.channel
            && self.shared.as_ref().map(|source| source.room_id.as_str())
                == confirmed.shared.as_ref().map(|source| source.room_id.as_str())
            && self.flags.is_action == confirmed.flags.is_action
            && self.reply.as_ref().map(|reply| reply.parent_msg_id.as_str())
                == confirmed.reply.as_ref().map(|reply| reply.parent_msg_id.as_str())
            && self
                .sender
                .login
                .eq_ignore_ascii_case(&confirmed.sender.login)
            && self.confirmation_text() == confirmed.confirmation_text()
    }

    /// Import server metadata while keeping local identity and accumulated UI state.
    pub(crate) fn apply_confirmation(&mut self, confirmed: &Self) {
        let local_id = self.id;
        let local_flags = self.flags.clone();
        *self = confirmed.clone();
        self.id = local_id;
        self.flags.is_history = local_flags.is_history;
        self.flags.is_deleted |= local_flags.is_deleted;
        self.flags.is_highlighted |= local_flags.is_highlighted;
        self.flags.is_mention |= local_flags.is_mention;
        self.flags.suppress_notification |= local_flags.suppress_notification;
    }
}

/// Lightweight emote entry for the UI catalog (autocomplete / picker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoteCatalogEntry {
    #[serde(default)]
    pub id: String,
    pub code: String,
    pub provider: String,
    pub url: String,
    #[serde(default)]
    pub url_hd: Option<String>,
    #[serde(default)]
    pub zero_width: bool,
    /// "global", "channel", or "personal".
    pub scope: String,
    /// Channel where this entry is available; personal entries may also be restricted.
    #[serde(default)]
    pub channel: Option<super::ChannelId>,
    /// Sender who may use this personal emote.
    #[serde(default)]
    pub user_id: Option<super::UserId>,
}

impl EmoteCatalogEntry {
    pub fn is_available_for(&self, channel: &super::ChannelId, sender_user_id: &str) -> bool {
        self.channel.as_ref().is_none_or(|owner| owner == channel)
            && self.user_id.as_ref().is_none_or(|owner| {
                channel.is_twitch() && !sender_user_id.is_empty() && owner.0 == sender_user_id
            })
    }

    /// Larger values win when several available entries use the same code.
    pub fn resolution_priority(&self) -> u8 {
        if self.user_id.is_some() {
            return match (self.provider.as_str(), self.channel.is_some()) {
                ("twitch", true) => 95,
                ("twitch", false) => 90,
                _ => 80,
            };
        }
        match (self.channel.is_some(), self.provider.as_str()) {
            (true, "kick") => 70,
            (true, "ffz") => 60,
            (true, "bttv") => 50,
            (true, "7tv") => 40,
            (false, "ffz") => 30,
            (false, "bttv") => 20,
            (false, "7tv") => 10,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MsgKind;

    #[test]
    fn reply_thread_metadata_round_trips_and_old_history_stays_readable() {
        let old = serde_json::json!({
            "parent_msg_id": "parent", "parent_user_login": "alice",
            "parent_display_name": "Alice", "parent_msg_body": "question"
        });
        let mut reply: super::ReplyInfo = serde_json::from_value(old).unwrap();
        assert_eq!(reply.thread_parent_msg_id, None);
        assert_eq!(reply.thread_parent_user_login, None);
        reply.thread_parent_msg_id = Some("root".into());
        reply.thread_parent_user_login = Some("bob".into());
        let restored: super::ReplyInfo = serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
        assert_eq!(restored, reply);
    }

    fn subscription_message(raw_text: &str, sub_msg: &str) -> super::ChatMessage {
        super::ChatMessage {
            id: super::super::MessageId(1),
            server_id: Some("subscription-1".into()),
            timestamp: chrono::Utc::now(),
            channel: super::super::ChannelId::new("room"),
            sender: super::super::Sender {
                user_id: super::super::UserId("10".into()), login: "alice".into(),
                display_name: "Alice".into(), original_display_name: None,
                color: None, name_paint: None, badges: Vec::new(),
            },
            original_text: None,
            raw_text: raw_text.into(), spans: Default::default(), twitch_emotes: Vec::new(),
            flags: Default::default(), reply: None, shared: None,
            msg_kind: MsgKind::Sub { display_name: "Alice".into(), months: 2,
                plan: "Tier 1".into(), is_gift: false, sub_msg: sub_msg.into() },
        }
    }

    #[test]
    fn subscription_plain_text_preserves_new_and_legacy_bodies_once() {
        let notice = "Alice resubscribed with Tier 1! (2 months)";
        let body = "  hello 🦀 Kappa  ";
        let complete = format!("{notice}\n{body}");
        for raw_text in [notice, body, complete.as_str()] {
            let message = subscription_message(raw_text, body);
            let stored = serde_json::to_string(&message).unwrap();
            let restored: super::ChatMessage = serde_json::from_str(&stored).unwrap();
            assert_eq!(restored.plain_text(), complete);
            assert_eq!(restored.plain_text().matches(body).count(), 1);
        }
        assert_eq!(subscription_message(notice, "").plain_text(), notice);
        assert_eq!(subscription_message("", "").plain_text(), notice);
        let mut ordinary = subscription_message("ordinary chat", "unused");
        ordinary.msg_kind = MsgKind::Chat;
        assert!(matches!(ordinary.plain_text(), std::borrow::Cow::Borrowed("ordinary chat")));
    }

    #[test]
    fn own_subscription_body_cannot_confirm_an_outgoing_chat_with_the_same_text() {
        let mut pending = subscription_message("hello", "hello");
        pending.msg_kind = MsgKind::Chat;
        pending.server_id = None;
        pending.flags.is_self = true;
        let mut confirmed = subscription_message("hello", "hello");
        confirmed.flags.is_self = true;
        assert!(!pending.is_pending_own_echo_of(&confirmed));
        confirmed.msg_kind = MsgKind::Chat;
        assert!(pending.is_pending_own_echo_of(&confirmed));
    }

    #[test]
    fn confirmation_matches_original_body_after_display_rules_change() {
        let mut pending = subscription_message("masked", "");
        pending.msg_kind = MsgKind::Chat;
        pending.server_id = None;
        pending.flags.is_self = true;
        pending.original_text = Some("original body".into());
        let mut confirmed = pending.clone();
        confirmed.server_id = Some("confirmed".into());
        confirmed.raw_text = "new replacement".into();
        assert!(pending.is_pending_own_echo_of(&confirmed));

        confirmed.raw_text = "original body".into();
        confirmed.original_text = None;
        assert!(pending.is_pending_own_echo_of(&confirmed));
    }

    #[test]
    fn confirmation_keeps_different_wire_bodies_distinct_when_masks_match() {
        let mut pending = subscription_message("***", "");
        pending.msg_kind = MsgKind::Chat;
        pending.server_id = None;
        pending.flags.is_self = true;
        pending.original_text = Some("first body".into());
        let mut confirmed = pending.clone();
        confirmed.server_id = Some("confirmed".into());
        confirmed.original_text = Some("second body".into());
        assert!(!pending.is_pending_own_echo_of(&confirmed));
    }

    #[test]
    fn original_confirmation_body_round_trips_and_old_history_defaults_to_display() {
        let mut pending = subscription_message("display body", "");
        let old = serde_json::to_value(&pending).unwrap();
        assert!(old.get("original_text").is_none());
        let restored: super::ChatMessage = serde_json::from_value(old).unwrap();
        assert_eq!(restored.confirmation_text(), "display body");
        pending.original_text = Some("wire body".into());
        let restored: super::ChatMessage =
            serde_json::from_str(&serde_json::to_string(&pending).unwrap()).unwrap();
        assert_eq!(restored.confirmation_text(), "wire body");
        assert_eq!(restored.plain_text(), "display body");
    }

    #[test]
    fn shared_mirrors_cannot_confirm_identical_sends_from_another_source() {
        let mut pending = subscription_message("hello", "");
        pending.msg_kind = MsgKind::Chat;
        pending.server_id = None;
        pending.flags.is_self = true;
        let mut confirmed = pending.clone();
        confirmed.server_id = Some("confirmed".into());
        confirmed.shared = Some(super::SharedChatSource {
            room_id: "other-room".into(),
            source_message_id: Some("original".into()),
            ..Default::default()
        });
        assert!(!pending.is_pending_own_echo_of(&confirmed));
        pending.shared = confirmed.shared.clone();
        assert!(pending.is_pending_own_echo_of(&confirmed));
        pending.shared.as_mut().unwrap().room_id = "third-room".into();
        assert!(!pending.is_pending_own_echo_of(&confirmed));
        pending.shared = None;
        confirmed.shared = None;
        assert!(pending.is_pending_own_echo_of(&confirmed));
    }

    #[test]
    fn old_emote_metadata_defaults_to_normal_and_overlays_round_trip() {
        let old_span = serde_json::json!({"Emote": {
            "id": "hat", "code": "Hat", "url": "https://example.test/hat.webp",
            "url_hd": null, "provider": "7tv"
        }});
        let span: super::Span = serde_json::from_value(old_span.clone()).unwrap();
        assert!(matches!(span, super::Span::Emote { zero_width: false, .. }));
        let mut overlay = old_span;
        overlay["Emote"]["zero_width"] = true.into();
        let span: super::Span = serde_json::from_value(overlay).unwrap();
        let decoded: super::Span = serde_json::from_str(&serde_json::to_string(&span).unwrap()).unwrap();
        assert_eq!(decoded, span);
        assert!(matches!(decoded, super::Span::Emote { zero_width: true, .. }));

        let old_catalog = serde_json::json!({"code": "Hat", "provider": "7tv",
            "url": "https://example.test/hat.webp", "scope": "channel"});
        let mut entry: super::EmoteCatalogEntry = serde_json::from_value(old_catalog).unwrap();
        assert!(!entry.zero_width);
        entry.zero_width = true;
        let decoded: super::EmoteCatalogEntry = serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn raid_deserializes_without_source_login() {
        let msg_kind: MsgKind =
            serde_json::from_str(r#"{"Raid":{"display_name":"Raider","viewer_count":42}}"#)
                .unwrap();

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
