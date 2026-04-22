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
    /// the UI to render MONITORED / RESTRICTED chips on message rows.
    pub low_trust_users: std::collections::HashMap<String, LowTrustStatus>,
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

    /// Record a user's low-trust status (monitored/restricted).
    pub fn set_low_trust(&mut self, login: &str, status: LowTrustStatus) {
        let key = login.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        self.low_trust_users.insert(key, status);
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
            .copied()
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

    /// Prepend historical messages (e.g. from recent-messages API) to the
    /// front of the buffer. Duplicates (matched by `server_id`) are skipped,
    /// and the total remains bounded by `MAX_MESSAGES`.
    pub fn prepend_history(&mut self, mut msgs: Vec<super::ChatMessage>) {
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
        if msgs.is_empty() {
            return;
        }

        // Respect the ring-buffer cap: drop oldest history entries when
        // the combined count would exceed MAX_MESSAGES.
        let available = MAX_MESSAGES.saturating_sub(self.messages.len());
        if msgs.len() > available {
            msgs.drain(0..msgs.len() - available);
        }

        // Build a new deque in one allocation: [history...] ++ [live...].
        // This is faster than iterating msgs in reverse with push_front.
        let total = msgs.len() + self.messages.len();
        let mut new_deque = std::collections::VecDeque::with_capacity(total);
        new_deque.extend(msgs);
        new_deque.extend(self.messages.drain(..));
        self.messages = new_deque;
    }
}
