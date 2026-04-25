use serde::{Deserialize, Serialize};

/// Twitch room state metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoomState {
    pub emote_only: bool,
    pub followers_only: Option<i32>, // -1 = off, 0+ = minutes
    pub slow_mode: Option<u32>,      // seconds
    pub subscribers_only: bool,
    pub r9k: bool,
}

const MAX_MESSAGES: usize = 1500;

/// Twitch low-trust / suspicious-user treatment for a single user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LowTrustStatus {
    Monitored,
    Restricted,
}

/// Per-user low-trust record kept on a [`ChannelState`]. Carries the
/// [`LowTrustStatus`] alongside the metadata the moderation tools window
/// needs to act on the user (Twitch user-id, original-cased display name).
#[derive(Debug, Clone)]
pub struct LowTrustEntry {
    pub status: LowTrustStatus,
    pub user_id: String,
    pub display_name: String,
}

/// State and message buffer for a channel.
#[derive(Debug)]
pub struct ChannelState {
    pub id: super::ChannelId,
    /// Ring-buffer of messages, capped at MAX_MESSAGES.
    pub messages: std::collections::VecDeque<super::ChatMessage>,
    pub room_state: RoomState,
    /// Chatters currently in the channel (from NAMES / JOIN / PART).
    pub chatters: std::collections::HashSet<String>,
    /// Total new messages received while this channel was not active (excluding history).
    pub unread_count: u32,
    /// Subset of unread_count that are attention-worthy (mentions/highlights/first/pinned).
    pub unread_mentions: u32,
    /// Whether the logged-in user is a moderator in this channel.
    pub is_mod: bool,
    /// Channel topic (IRC only).
    pub topic: Option<String>,
    /// Low-trust (suspicious-user) treatment keyed by lowercased login.
    /// Populated from EventSub `channel.suspicious_user.*`; consumed by
    /// the UI to render MONITORED / RESTRICTED chips on message rows
    /// and to populate the moderation tools "Low Trust" tab.
    pub low_trust_users: std::collections::HashMap<String, LowTrustEntry>,
}

impl ChannelState {
    pub fn new(id: super::ChannelId) -> Self {
        Self {
            id,
            messages: std::collections::VecDeque::with_capacity(MAX_MESSAGES),
            room_state: RoomState::default(),
            chatters: std::collections::HashSet::new(),
            unread_count: 0,
            unread_mentions: 0,
            is_mod: false,
            topic: None,
            low_trust_users: std::collections::HashMap::new(),
        }
    }

    /// Record a user's low-trust status (monitored/restricted) along with
    /// the Twitch user-id and display name needed to act on them later.
    pub fn set_low_trust(
        &mut self,
        login: &str,
        user_id: &str,
        display_name: &str,
        status: LowTrustStatus,
    ) {
        let key = login.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        self.low_trust_users.insert(
            key,
            LowTrustEntry {
                status,
                user_id: user_id.to_owned(),
                display_name: display_name.to_owned(),
            },
        );
    }

    /// Clear a user's low-trust status.
    pub fn clear_low_trust(&mut self, login: &str) {
        let key = login.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        self.low_trust_users.remove(&key);
    }

    /// Look up a user's current low-trust status, if any.
    pub fn low_trust_status(&self, login: &str) -> Option<LowTrustStatus> {
        self.low_trust_users
            .get(&login.trim().to_ascii_lowercase())
            .map(|e| e.status)
    }

    pub fn push_message(&mut self, msg: super::ChatMessage) {
        if self.messages.len() >= MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }

    /// Try to absorb a Twitch echo of one of our own sent messages.
    ///
    /// Twitch echoes every PRIVMSG back to the sender. We also add a local
    /// echo immediately on send (with `server_id = None`). When the real
    /// echo arrives we should update the local copy in-place rather than
    /// pushing a second copy of the same message.
    ///
    /// Returns `true` if a local echo was found and updated (caller should
    /// NOT push a new message). Returns `false` if no match was found
    /// (caller should push normally).
    pub fn absorb_own_echo(&mut self, msg: &super::ChatMessage) -> bool {
        // Only applies to own messages with a real server_id from Twitch.
        let Some(ref echo_id) = msg.server_id else {
            return false;
        };
        // Look for the most recent local echo: same sender login, same text,
        // server_id = None (i.e. not yet confirmed by Twitch).
        let sender_login = msg.sender.login.to_lowercase();
        let raw_text = &msg.raw_text;
        // Iterate from newest to oldest so we grab the closest pending echo.
        if let Some(existing) = self.messages.iter_mut().rev().find(|m| {
            m.server_id.is_none()
                && m.flags.is_self
                && m.sender.login.to_lowercase() == sender_login
                && m.raw_text == *raw_text
        }) {
            // Stamp the local echo with the real server id and any Twitch-side
            // metadata we now know (badges, colour, timestamp, ...).
            existing.server_id = Some(echo_id.clone());
            existing.timestamp = msg.timestamp;
            existing.sender = msg.sender.clone();
            existing.twitch_emotes = msg.twitch_emotes.clone();
            existing.spans = msg.spans.clone();
            existing.reply = msg.reply.clone();
            existing.msg_kind = msg.msg_kind.clone();

            // Preserve local echo invariants while importing server metadata.
            existing.flags.is_self = true;
            existing.flags.is_history = false;
            existing.flags.is_action = msg.flags.is_action;
            existing.flags.is_highlighted = msg.flags.is_highlighted;
            existing.flags.is_mention = msg.flags.is_mention;
            existing.flags.custom_reward_id = msg.flags.custom_reward_id.clone();
            return true;
        }
        false
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
            if m.msg_kind == super::MsgKind::Chat
                && m.sender.login.eq_ignore_ascii_case(login)
                && !m.flags.is_deleted
            {
                m.flags.is_deleted = true;
            }
        }
    }

    /// Update the status for the most recent channel points redemption message
    /// with the given redemption id.
    ///
    /// Returns `true` when a matching message was found and updated.
    pub fn update_redemption_status(&mut self, redemption_id: &str, status: &str) -> bool {
        let redemption_id = redemption_id.trim();
        let status = status.trim();
        if redemption_id.is_empty() || status.is_empty() {
            return false;
        }

        for m in self.messages.iter_mut().rev() {
            if let super::MsgKind::ChannelPointsReward {
                redemption_id: rid,
                status: current_status,
                ..
            } = &mut m.msg_kind
            {
                let matches = rid
                    .as_deref()
                    .map(|id| id.eq_ignore_ascii_case(redemption_id))
                    .unwrap_or(false);
                if matches {
                    *current_status = Some(status.to_owned());
                    return true;
                }
            }
        }

        false
    }

    pub fn upsert_live_hype_train_row(&mut self, msg: super::ChatMessage) -> bool {
        let (target_train_id, incoming_phase) = match &msg.msg_kind {
            super::MsgKind::HypeTrain {
                train_id, phase, ..
            } => (train_id.as_str(), phase.as_str()),
            _ => return false,
        };

        if incoming_phase == "end" {
            return false;
        }

        if let Some(existing) = self.messages.iter_mut().rev().find(|m| {
            matches!(
                &m.msg_kind,
                super::MsgKind::HypeTrain { train_id, .. } if train_id == target_train_id
            )
        }) {
            if matches!(
                &existing.msg_kind,
                super::MsgKind::HypeTrain { phase, .. } if phase == "end"
            ) {
                return false;
            }

            existing.server_id = msg.server_id;
            existing.timestamp = msg.timestamp;
            existing.sender = msg.sender;
            existing.raw_text = msg.raw_text;
            existing.spans = msg.spans;
            existing.twitch_emotes = msg.twitch_emotes;
            existing.flags = msg.flags;
            existing.reply = msg.reply;
            existing.msg_kind = msg.msg_kind;
            return true;
        }

        false
    }

    /// Prepend historical messages (e.g. from recent-messages API or local
    /// SQLite log) to the buffer. Duplicates (matched by `server_id`) are
    /// skipped. After the merge the buffer is sorted by timestamp ascending
    /// so two independent history sources (e.g. local DB + robotty) can
    /// arrive in any order and still interleave chronologicallyavoiding
    /// the bug where a later-arriving batch of newer messages ended up at
    /// the very top of the chat. Live messages (pushed via `push_message`
    /// with timestamps >= history) naturally sort to the end.
    pub fn prepend_history(&mut self, mut msgs: Vec<super::ChatMessage>) {
        // Drop duplicates by server_id against what's already in the buffer.
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
        if msgs.is_empty() {
            return;
        }

        // Merge into a single Vec and sort chronologically (stable, so rows
        // within the same millisecond keep their arrival order).
        let total_cap = (msgs.len() + self.messages.len()).min(MAX_MESSAGES);
        let mut merged: Vec<super::ChatMessage> =
            Vec::with_capacity(msgs.len() + self.messages.len());
        merged.extend(self.messages.drain(..));
        merged.extend(msgs);
        merged.sort_by_key(|m| m.timestamp);

        // Enforce capdrop oldest.
        if merged.len() > MAX_MESSAGES {
            let excess = merged.len() - MAX_MESSAGES;
            merged.drain(0..excess);
        }

        let mut new_deque = std::collections::VecDeque::with_capacity(total_cap);
        new_deque.extend(merged);
        self.messages = new_deque;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use smallvec::SmallVec;

    #[test]
    fn low_trust_set_clear_round_trips_with_metadata() {
        let mut ch = ChannelState::new(super::super::ChannelId::new("rustlang"));
        ch.set_low_trust("Alice", "12345", "Alice", LowTrustStatus::Restricted);
        ch.set_low_trust("BoB", "67890", "BoB", LowTrustStatus::Monitored);
        // Login keys normalised to lowercase.
        let alice = ch.low_trust_users.get("alice").expect("alice tracked");
        assert_eq!(alice.user_id, "12345");
        assert_eq!(alice.display_name, "Alice");
        assert_eq!(alice.status, LowTrustStatus::Restricted);
        let bob = ch.low_trust_users.get("bob").expect("bob tracked");
        assert_eq!(bob.status, LowTrustStatus::Monitored);
        assert_eq!(ch.low_trust_status("Alice"), Some(LowTrustStatus::Restricted));
        ch.clear_low_trust("alice");
        assert!(ch.low_trust_status("Alice").is_none());
        assert!(ch.low_trust_status("bob").is_some());
    }

    #[test]
    fn low_trust_ignores_blank_login() {
        let mut ch = ChannelState::new(super::super::ChannelId::new("rustlang"));
        ch.set_low_trust("   ", "99", "ghost", LowTrustStatus::Monitored);
        assert!(ch.low_trust_users.is_empty());
    }

    fn test_hype_message_with_metadata(
        train_id: &str,
        phase: &str,
        level: u32,
        progress: u64,
        goal: u64,
        message_id: u64,
        server_id: Option<&str>,
        sender_login: &str,
        raw_text: &str,
        is_highlighted: bool,
        is_deleted: bool,
        reply: Option<&str>,
        emote_id: Option<&str>,
        timestamp_seconds: i64,
    ) -> super::super::ChatMessage {
        super::super::ChatMessage {
            id: super::super::MessageId(message_id),
            server_id: server_id.map(str::to_owned),
            timestamp: Utc.timestamp_opt(timestamp_seconds, 0).unwrap(),
            channel: super::super::ChannelId::new("rustlang"),
            sender: super::super::Sender {
                user_id: super::super::UserId("1".to_owned()),
                login: sender_login.to_owned(),
                display_name: sender_login.to_owned(),
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: raw_text.to_owned(),
            spans: SmallVec::new(),
            twitch_emotes: emote_id
                .map(|id| {
                    vec![super::super::TwitchEmotePos {
                        id: id.to_owned(),
                        start: 0,
                        end: 4,
                    }]
                })
                .unwrap_or_default(),
            flags: super::super::MessageFlags {
                is_highlighted,
                is_deleted,
                ..super::super::MessageFlags::default()
            },
            reply: reply.map(|parent_msg_id| super::super::ReplyInfo {
                parent_msg_id: parent_msg_id.to_owned(),
                parent_user_login: "parent".to_owned(),
                parent_display_name: "Parent".to_owned(),
                parent_msg_body: "parent body".to_owned(),
            }),
            msg_kind: super::super::MsgKind::HypeTrain {
                phase: phase.to_owned(),
                train_id: train_id.to_owned(),
                level,
                progress,
                goal,
                total: progress,
                top_contributor_login: None,
                top_contributor_type: None,
                top_contributor_total: None,
                ends_at: None,
            },
            shared: None,
        }
    }

    fn test_hype_message(
        train_id: &str,
        phase: &str,
        level: u32,
        progress: u64,
        goal: u64,
    ) -> super::super::ChatMessage {
        test_hype_message_with_metadata(
            train_id,
            phase,
            level,
            progress,
            goal,
            1,
            None,
            "twitch",
            &format!("hype train {phase}"),
            false,
            false,
            None,
            None,
            1,
        )
    }

    #[test]
    fn upsert_live_hype_train_row_replaces_begin_with_progress() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        room.push_message(test_hype_message("train-1", "begin", 1, 100, 500));

        let updated =
            room.upsert_live_hype_train_row(test_hype_message("train-1", "progress", 1, 300, 500));

        assert!(updated);
        assert_eq!(room.messages.len(), 1);
        match &room.messages[0].msg_kind {
            super::super::MsgKind::HypeTrain {
                phase, progress, ..
            } => {
                assert_eq!(phase, "progress");
                assert_eq!(*progress, 300);
            }
            other => panic!("unexpected message kind: {other:?}"),
        }
    }

    #[test]
    fn upsert_live_hype_train_row_does_not_replace_final_end_row() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        room.push_message(test_hype_message("train-1", "end", 3, 0, 0));

        let updated =
            room.upsert_live_hype_train_row(test_hype_message("train-1", "progress", 3, 400, 500));

        assert!(!updated);
        assert_eq!(room.messages.len(), 1);
    }

    #[test]
    fn upsert_live_hype_train_row_does_not_replace_older_live_row_after_end_exists() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        room.push_message(test_hype_message("train-1", "begin", 1, 100, 500));
        room.push_message(test_hype_message("train-2", "progress", 2, 200, 500));
        room.push_message(test_hype_message("train-1", "end", 3, 0, 0));

        let updated =
            room.upsert_live_hype_train_row(test_hype_message("train-1", "progress", 3, 400, 500));

        assert!(!updated);
        assert_eq!(room.messages.len(), 3);
        match &room.messages[0].msg_kind {
            super::super::MsgKind::HypeTrain {
                phase, progress, ..
            } => {
                assert_eq!(phase, "begin");
                assert_eq!(*progress, 100);
            }
            other => panic!("unexpected message kind: {other:?}"),
        }
        match &room.messages[2].msg_kind {
            super::super::MsgKind::HypeTrain { phase, .. } => assert_eq!(phase, "end"),
            other => panic!("unexpected message kind: {other:?}"),
        }
    }

    #[test]
    fn upsert_live_hype_train_row_clears_stale_metadata() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        room.push_message(test_hype_message_with_metadata(
            "train-1",
            "begin",
            1,
            100,
            500,
            10,
            Some("old-server"),
            "old_sender",
            "old raw",
            true,
            true,
            Some("old-parent"),
            Some("old-emote"),
            10,
        ));

        let updated = room.upsert_live_hype_train_row(test_hype_message_with_metadata(
            "train-1",
            "progress",
            1,
            300,
            500,
            99,
            None,
            "new_sender",
            "new raw",
            false,
            false,
            None,
            None,
            20,
        ));

        assert!(updated);
        let row = &room.messages[0];
        assert_eq!(row.id, super::super::MessageId(10));
        assert_eq!(row.server_id, None);
        assert_eq!(row.timestamp, Utc.timestamp_opt(20, 0).unwrap());
        assert_eq!(row.sender.login, "new_sender");
        assert_eq!(row.raw_text, "new raw");
        assert_eq!(row.flags.is_highlighted, false);
        assert_eq!(row.flags.is_deleted, false);
        assert!(row.reply.is_none());
        assert!(row.twitch_emotes.is_empty());
        match &row.msg_kind {
            super::super::MsgKind::HypeTrain {
                phase, progress, ..
            } => {
                assert_eq!(phase, "progress");
                assert_eq!(*progress, 300);
            }
            other => panic!("unexpected message kind: {other:?}"),
        }
    }
}
