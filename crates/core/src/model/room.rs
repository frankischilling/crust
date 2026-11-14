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
const MAX_LOADED_HISTORY: usize = 50_000;
const MODERATION_DEDUP_ROWS: usize = 20;
const MODERATION_DEDUP_MILLIS: u64 = 5000;

pub const IRC_MODERATION_ID_PREFIX: &str = "irc:moderation:";
pub const EVENTSUB_MODERATION_ID_PREFIX: &str = "eventsub:moderation:";

/// Stable position in locally stored history, including equal timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HistoryCursor {
    pub timestamp_ms: i64,
    pub row_id: i64,
}

#[derive(Debug, Clone)]
pub struct LocalHistoryMessageIdentity {
    pub replay_id: super::MessageId,
    pub original_id: super::MessageId,
}

#[derive(Debug, Clone)]
pub struct LocalHistoryPageInfo {
    pub before: Option<HistoryCursor>,
    pub next: Option<HistoryCursor>,
    pub has_more: bool,
    pub identities: Vec<LocalHistoryMessageIdentity>,
}

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
    /// Live buffer, expanded within a fixed limit when older logs are requested.
    pub messages: std::collections::VecDeque<super::ChatMessage>,
    pub local_history_cursor: Option<HistoryCursor>,
    pub local_history_exhausted: bool,
    local_history_aliases: std::collections::HashMap<
        super::MessageId,
        (super::MessageId, chrono::DateTime<chrono::Utc>),
    >,
    message_limit: usize,
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
    paired_moderation_notices: std::collections::VecDeque<super::MessageId>,
}

impl ChannelState {
    pub fn new(id: super::ChannelId) -> Self {
        Self {
            id,
            messages: std::collections::VecDeque::with_capacity(MAX_MESSAGES),
            local_history_cursor: None,
            local_history_exhausted: false,
            local_history_aliases: Default::default(),
            message_limit: MAX_MESSAGES,
            room_state: RoomState::default(),
            chatters: std::collections::HashSet::new(),
            unread_count: 0,
            unread_mentions: 0,
            is_mod: false,
            topic: None,
            low_trust_users: std::collections::HashMap::new(),
            paired_moderation_notices: std::collections::VecDeque::new(),
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
        if self.messages.len() >= self.message_limit {
            self.messages.pop_front();
            self.local_history_cursor = None;
            self.local_history_exhausted = false;
        }
        self.messages.push_back(msg);
    }

    /// Merge paired IRC and EventSub notices, retaining EventSub's details.
    /// Each notice can pair once so repeated actions remain separate rows.
    pub fn absorb_moderation_notice(&mut self, msg: &super::ChatMessage) -> bool {
        let Some(incoming_eventsub) = moderation_notice_origin(msg) else {
            return false;
        };
        if msg.channel != self.id {
            return false;
        }
        let matched = self
            .messages
            .iter()
            .enumerate()
            .rev()
            .take(MODERATION_DEDUP_ROWS)
            .find_map(|(index, existing)| {
                let existing_eventsub = moderation_notice_origin(existing)?;
                if incoming_eventsub == existing_eventsub
                    || self.paired_moderation_notices.contains(&existing.id)
                    || existing.channel != msg.channel
                    || (msg.timestamp - existing.timestamp)
                        .num_milliseconds()
                        .unsigned_abs()
                        > MODERATION_DEDUP_MILLIS
                    || !same_moderation_action(&existing.msg_kind, &msg.msg_kind)
                {
                    return None;
                }
                Some(index)
            });
        let Some(index) = matched else {
            return false;
        };
        let existing = &mut self.messages[index];
        let id = existing.id;
        if incoming_eventsub {
            *existing = msg.clone();
            existing.id = id;
        }
        if self.paired_moderation_notices.len() >= MODERATION_DEDUP_ROWS {
            self.paired_moderation_notices.pop_front();
        }
        self.paired_moderation_notices.push_back(id);
        true
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
            if matches!(
                m.msg_kind,
                super::MsgKind::Chat
                    | super::MsgKind::Bits { .. }
                    | super::MsgKind::SuspiciousUserMessage
            ) && m.sender.login.eq_ignore_ascii_case(login)
                && !m.flags.is_deleted
            {
                m.flags.is_deleted = true;
            }
        }
    }

    /// Mark chat messages deleted without removing notices or scroll context.
    pub fn delete_all_chat_messages(&mut self) {
        for message in &mut self.messages {
            if matches!(
                message.msg_kind,
                super::MsgKind::Chat
                    | super::MsgKind::Bits { .. }
                    | super::MsgKind::SuspiciousUserMessage
            ) {
                message.flags.is_deleted = true;
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

    /// Merge history in timestamp order, skipping repeated server IDs both
    /// within the incoming batch and against the current buffer.
    pub fn prepend_history(&mut self, msgs: Vec<super::ChatMessage>) {
        self.merge_history(msgs, false);
    }

    /// Keep explicitly requested older logs available for search and navigation.
    pub fn prepend_older_history(&mut self, msgs: Vec<super::ChatMessage>) {
        self.merge_history(msgs, true);
    }

    /// Merge stored rows without duplicating either prior replays or live rows
    /// that are still in the buffer. Keep the live IDs used by selection and layout.
    pub fn prepend_local_history(
        &mut self,
        mut msgs: Vec<super::ChatMessage>,
        page: &LocalHistoryPageInfo,
    ) {
        let live_ids: std::collections::HashSet<_> = self.messages.iter()
            .filter(|message| !message.flags.is_history && message.channel == self.id)
            .map(|message| (message.id, message.timestamp))
            .collect();
        self.local_history_aliases.retain(|_, live| live_ids.contains(live));
        let matched_live_ids: std::collections::HashSet<_> =
            self.local_history_aliases.values().copied().collect();
        let mut unmatched_live_rows: std::collections::HashMap<_, _> = self.messages.iter()
            .filter(|message| !message.flags.is_history
                && message.channel == self.id
                && !matched_live_ids.contains(&(message.id, message.timestamp)))
            .map(|message| ((message.id, message.timestamp), message))
            .collect();
        let mut existing_rows: std::collections::HashSet<_> = self.messages.iter()
            .filter(|message| message.id.is_local_history())
            .map(|message| message.id)
            .chain(self.local_history_aliases.keys().copied())
            .collect();
        let original_ids: std::collections::HashMap<_, _> = page.identities.iter()
            .map(|identity| (identity.replay_id, identity.original_id))
            .collect();
        msgs.retain(|message| {
            if message.channel != self.id {
                return false;
            }
            if message.id.is_local_history() && !existing_rows.insert(message.id) {
                return false;
            }
            let Some(original_id) = original_ids.get(&message.id).copied() else {
                return true;
            };
            let live_key = (original_id, message.timestamp);
            let matches_live = unmatched_live_rows.get(&live_key).is_some_and(|live|
                live.sender.login == message.sender.login && live.raw_text == message.raw_text);
            if matches_live {
                unmatched_live_rows.remove(&live_key);
                self.local_history_aliases.insert(message.id, (original_id, message.timestamp));
                false
            } else {
                true
            }
        });
        self.merge_history(msgs, page.before.is_some());
    }

    fn merge_history(&mut self, mut msgs: Vec<super::ChatMessage>, expand: bool) {
        // Drop duplicates by server_id against what's already in the buffer.
        let mut existing_ids: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter_map(|m| m.server_id.clone().filter(|id| !id.is_empty()))
            .collect();
        msgs.retain(|m| {
            m.server_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(|id| existing_ids.insert(id.to_owned()))
                .unwrap_or(true)
        });
        if msgs.is_empty() {
            return;
        }

        if expand {
            self.message_limit = MAX_LOADED_HISTORY;
        }

        // Merge into a single Vec and sort chronologically (stable, so rows
        // within the same millisecond keep their arrival order).
        let total_cap = (msgs.len() + self.messages.len()).min(self.message_limit);
        let mut merged: Vec<super::ChatMessage> =
            Vec::with_capacity(msgs.len() + self.messages.len());
        merged.extend(msgs);
        merged.extend(self.messages.drain(..));
        merged.sort_by_key(|m| m.timestamp);

        // Keep the most recent messages when the merged buffer exceeds its cap.
        if merged.len() > self.message_limit {
            let excess = merged.len() - self.message_limit;
            merged.drain(0..excess);
        }

        let mut new_deque = std::collections::VecDeque::with_capacity(total_cap);
        new_deque.extend(merged);
        self.messages = new_deque;
    }
}

fn moderation_notice_origin(msg: &super::ChatMessage) -> Option<bool> {
    if msg.flags.is_history
        || !matches!(
            msg.msg_kind,
            super::MsgKind::Timeout { .. }
                | super::MsgKind::Ban { .. }
                | super::MsgKind::ChatCleared
        )
    {
        return None;
    }
    let id = msg.server_id.as_deref()?;
    if id.starts_with(EVENTSUB_MODERATION_ID_PREFIX) {
        Some(true)
    } else if id.starts_with(IRC_MODERATION_ID_PREFIX) {
        Some(false)
    } else {
        None
    }
}

fn same_moderation_action(left: &super::MsgKind, right: &super::MsgKind) -> bool {
    match (left, right) {
        (
            super::MsgKind::Timeout {
                login: left,
                seconds: left_seconds,
            },
            super::MsgKind::Timeout {
                login: right,
                seconds: right_seconds,
            },
        ) => {
            left.eq_ignore_ascii_case(right)
                && (*left_seconds == 0
                    || *right_seconds == 0
                    || left_seconds.abs_diff(*right_seconds) <= 2)
        }
        (super::MsgKind::Ban { login: left }, super::MsgKind::Ban { login: right }) => {
            left.eq_ignore_ascii_case(right)
        }
        (super::MsgKind::ChatCleared, super::MsgKind::ChatCleared) => true,
        _ => false,
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
        assert_eq!(
            ch.low_trust_status("Alice"),
            Some(LowTrustStatus::Restricted)
        );
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

    #[test]
    fn requested_older_history_survives_a_full_live_buffer_and_new_messages() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        let make = |id: u64| {
            let mut message = test_hype_message("train", "begin", 1, 0, 1);
            message.id = super::super::MessageId(id);
            message.server_id = Some(format!("message-{id}"));
            message.timestamp = Utc.timestamp_opt(id as i64, 0).unwrap();
            message
        };
        for id in 1000..2500 { room.push_message(make(id)); }
        room.prepend_older_history((200..1000).map(make).collect());
        assert_eq!(room.messages.len(), 2300);
        room.push_message(make(2500));
        assert_eq!(room.messages.front().unwrap().id.0, 200);
        assert_eq!(room.messages.back().unwrap().id.0, 2500);
        assert_eq!(room.message_limit, MAX_LOADED_HISTORY);
    }

    #[test]
    fn older_history_precedes_live_rows_at_equal_timestamps() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        let mut newer = test_hype_message("train", "begin", 1, 0, 1);
        newer.id = super::super::MessageId(2);
        newer.server_id = Some("newer".to_owned());
        let mut older = newer.clone();
        older.id = super::super::MessageId(1);
        older.server_id = Some("older".to_owned());
        room.push_message(newer);
        room.prepend_older_history(vec![older]);
        assert_eq!(room.messages.iter().map(|message| message.id.0).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn ordinary_history_keeps_the_live_limit_and_eviction_reopens_local_paging() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        let message = test_hype_message("train", "begin", 1, 0, 1);
        for _ in 0..MAX_MESSAGES { room.push_message(message.clone()); }
        room.local_history_exhausted = true;
        room.local_history_cursor = Some(HistoryCursor { timestamp_ms: 1000, row_id: 1 });
        room.push_message(message);
        assert_eq!(room.messages.len(), MAX_MESSAGES);
        assert!(!room.local_history_exhausted);
        assert_eq!(room.local_history_cursor, None);
    }

    fn local_history_test_message(id: u64) -> super::super::ChatMessage {
        let mut message = test_hype_message("train", "begin", 1, 0, 1);
        message.id = super::super::MessageId(id);
        message.server_id = None;
        message.raw_text = "same text".into();
        message.timestamp = Utc.timestamp_millis_opt(1000).unwrap();
        message.msg_kind = super::super::MsgKind::Chat;
        message
    }

    fn local_history_test_page(
        rows: &[(i64, u64)],
        older: bool,
    ) -> (Vec<super::super::ChatMessage>, LocalHistoryPageInfo) {
        let mut identities = Vec::new();
        let messages = rows.iter().map(|&(row_id, original_id)| {
            let mut message = local_history_test_message(original_id);
            let replay_id = super::super::MessageId::from_local_history_row(row_id).unwrap();
            identities.push(LocalHistoryMessageIdentity { replay_id, original_id: message.id });
            message.id = replay_id;
            message.flags.is_history = true;
            message
        }).collect();
        (messages, LocalHistoryPageInfo {
            before: older.then_some(HistoryCursor { timestamp_ms: 1000, row_id: i64::MAX }),
            next: rows.first().map(|&(row_id, _)| HistoryCursor { timestamp_ms: 1000, row_id }),
            has_more: true,
            identities,
        })
    }

    #[test]
    fn local_history_keeps_distinct_rows_and_live_ids_across_reloads() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        room.push_message(local_history_test_message(9));
        // A non-local history row is not a known copy of this SQLite row.
        let mut remote = local_history_test_message(10);
        remote.flags.is_history = true;
        room.push_message(remote);
        let (messages, page) = local_history_test_page(&[(101, 9), (102, 10), (103, 10)], false);
        for _ in 0..3 {
            room.prepend_local_history(messages.clone(), &page);
            assert_eq!(room.messages.len(), 4);
            assert_eq!(room.messages.iter().filter(|message| message.id == super::super::MessageId(9)).count(), 1);
        }
        // Even the same persisted original ID and timestamp can belong to a
        // different row. The first replay already matched the retained live row.
        let (next_messages, next_page) = local_history_test_page(&[(104, 9)], true);
        for _ in 0..3 {
            room.prepend_local_history(next_messages.clone(), &next_page);
            room.prepend_local_history(messages.clone(), &page);
            assert_eq!(room.messages.len(), 5);
        }
        assert!(room.messages.iter().all(|message| message.raw_text == "same text"));
        assert_eq!(room.messages.iter().filter(|message| message.id.is_local_history()).count(), 3);
    }

    #[test]
    fn local_history_fallback_after_eviction_does_not_duplicate_boundary_rows() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        for id in 1..=MAX_MESSAGES as u64 {
            room.push_message(local_history_test_message(id));
        }
        room.local_history_cursor = Some(HistoryCursor { timestamp_ms: 1000, row_id: 1 });
        room.push_message(local_history_test_message(MAX_MESSAGES as u64 + 1));
        assert_eq!(room.local_history_cursor, None);
        assert_eq!(room.messages.front().unwrap().id, super::super::MessageId(2));

        let (messages, page) = local_history_test_page(&[(1, 1), (2, 2), (3, 3)], true);
        for _ in 0..3 {
            room.prepend_local_history(messages.clone(), &page);
            assert_eq!(room.messages.len(), MAX_MESSAGES + 1);
            assert_eq!(room.messages.iter().filter(|message| message.id == super::super::MessageId(2)).count(), 1);
            assert_eq!(room.messages.iter().filter(|message| message.id == super::super::MessageId(3)).count(), 1);
        }
        assert_eq!(room.messages.front().unwrap().id,
            super::super::MessageId::from_local_history_row(1).unwrap());

        // Forget aliases as soon as their live rows have left the buffer.
        room.messages.pop_front();
        room.messages.pop_front();
        room.prepend_local_history(messages, &page);
        assert_eq!(room.messages.len(), MAX_MESSAGES + 1);
        assert!(room.messages.iter().any(|message|
            Some(message.id) == super::super::MessageId::from_local_history_row(2)));
        assert!(!room.local_history_aliases.contains_key(
            &super::super::MessageId::from_local_history_row(2).unwrap()));
    }

    #[test]
    fn history_deduplicates_within_batch_and_preserves_live_rows() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        let mut live = test_hype_message("train", "begin", 1, 0, 1);
        live.server_id = Some("live".to_owned());
        live.timestamp = Utc.timestamp_opt(30, 0).unwrap();
        room.push_message(live.clone());

        let mut older = live.clone();
        older.server_id = Some("older".to_owned());
        older.timestamp = Utc.timestamp_opt(10, 0).unwrap();
        older.flags.is_history = true;
        let mut duplicate_live = live.clone();
        duplicate_live.raw_text = "replayed live row".to_owned();
        duplicate_live.flags.is_history = true;
        let mut without_id = older.clone();
        without_id.server_id = None;
        room.prepend_history(vec![
            older.clone(),
            older,
            duplicate_live,
            without_id.clone(),
            without_id,
        ]);

        assert_eq!(room.messages.len(), 4);
        assert_eq!(
            room.messages
                .iter()
                .filter(|m| m.server_id.as_deref() == Some("older"))
                .count(),
            1
        );
        assert_eq!(
            room.messages
                .iter()
                .filter(|m| m.server_id.is_none())
                .count(),
            2
        );
        assert_eq!(room.messages.back().unwrap().raw_text, live.raw_text);
        assert!(!room.messages.back().unwrap().flags.is_history);
    }

    #[test]
    fn user_clear_deletes_chat_bits_and_suspicious_messages_only() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        for kind in [
            super::super::MsgKind::Chat,
            super::super::MsgKind::Bits { amount: 100 },
            super::super::MsgKind::SuspiciousUserMessage,
            super::super::MsgKind::SystemInfo,
        ] {
            let mut msg = test_hype_message("train", "begin", 1, 0, 1);
            msg.sender.login = "Alice".to_owned();
            msg.msg_kind = kind;
            room.push_message(msg);
        }
        let mut other = room.messages[0].clone();
        other.sender.login = "bob".to_owned();
        room.push_message(other);
        room.delete_messages_from("ALICE");
        let deleted: Vec<_> = room.messages.iter().map(|m| m.flags.is_deleted).collect();
        assert_eq!(deleted, [true, true, true, false, false]);
    }

    fn moderation_notice(
        id: u64,
        eventsub: bool,
        kind: super::super::MsgKind,
        seconds: i64,
    ) -> super::super::ChatMessage {
        let mut message = test_hype_message("train", "begin", 1, 0, 1);
        message.id = super::super::MessageId(id);
        message.timestamp = Utc.timestamp_opt(seconds, 0).unwrap();
        let prefix = if eventsub {
            EVENTSUB_MODERATION_ID_PREFIX
        } else {
            IRC_MODERATION_ID_PREFIX
        };
        message.server_id = Some(format!("{prefix}{id}"));
        message.raw_text = if eventsub {
            "moderator performed the action"
        } else {
            "action received from chat"
        }
        .to_owned();
        message.msg_kind = kind;
        message
    }

    #[test]
    fn moderation_duplicates_keep_eventsub_details_in_both_arrival_orders() {
        for kind in [
            super::super::MsgKind::Timeout {
                login: "alice".into(),
                seconds: 600,
            },
            super::super::MsgKind::Ban {
                login: "alice".into(),
            },
            super::super::MsgKind::ChatCleared,
        ] {
            for first_eventsub in [false, true] {
                let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
                room.push_message(moderation_notice(1, first_eventsub, kind.clone(), 10));
                assert!(room.absorb_moderation_notice(&moderation_notice(
                    2,
                    !first_eventsub,
                    kind.clone(),
                    11
                )));
                assert_eq!(room.messages.len(), 1);
                assert_eq!(room.messages[0].id, super::super::MessageId(1));
                assert_eq!(room.messages[0].raw_text, "moderator performed the action");
            }
        }
    }

    #[test]
    fn repeated_moderation_actions_from_one_source_remain_separate() {
        for source in [false, true] {
            let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
            let kind = super::super::MsgKind::Timeout {
                login: "alice".into(),
                seconds: 600,
            };
            room.push_message(moderation_notice(1, source, kind.clone(), 10));
            let second = moderation_notice(2, source, kind, 11);
            assert!(!room.absorb_moderation_notice(&second));
            room.push_message(second);
            assert_eq!(room.messages.len(), 2);
        }
    }

    #[test]
    fn each_moderation_action_pairs_at_most_once() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        let kind = super::super::MsgKind::Timeout {
            login: "alice".into(),
            seconds: 600,
        };
        for (index, source) in [false, false, true, true, false].into_iter().enumerate() {
            let notice = moderation_notice(index as u64 + 1, source, kind.clone(), 10);
            if !room.absorb_moderation_notice(&notice) {
                room.push_message(notice);
            }
        }
        assert_eq!(room.messages.len(), 3);
        assert_eq!(
            room.messages
                .iter()
                .filter(|message| message.raw_text.contains("moderator"))
                .count(),
            2
        );
    }

    #[test]
    fn unrelated_or_stale_moderation_actions_are_not_absorbed() {
        let kind = super::super::MsgKind::Timeout {
            login: "alice".into(),
            seconds: 600,
        };
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        room.push_message(moderation_notice(1, false, kind.clone(), 10));
        let cases = [
            moderation_notice(2, true, kind.clone(), 16),
            moderation_notice(
                3,
                true,
                super::super::MsgKind::Timeout {
                    login: "bob".into(),
                    seconds: 600,
                },
                10,
            ),
            moderation_notice(
                4,
                true,
                super::super::MsgKind::Timeout {
                    login: "alice".into(),
                    seconds: 30,
                },
                10,
            ),
            moderation_notice(
                5,
                true,
                super::super::MsgKind::Ban {
                    login: "alice".into(),
                },
                10,
            ),
        ];
        for message in cases {
            assert!(!room.absorb_moderation_notice(&message));
        }
        let mut history = moderation_notice(6, true, kind.clone(), 10);
        history.flags.is_history = true;
        assert!(!room.absorb_moderation_notice(&history));
        let mut other_channel = moderation_notice(7, true, kind, 10);
        other_channel.channel = super::super::ChannelId::new("other");
        assert!(!room.absorb_moderation_notice(&other_channel));
    }

    #[test]
    fn channel_clear_preserves_notices_for_cross_source_merging() {
        let mut room = ChannelState::new(super::super::ChannelId::new("rustlang"));
        for kind in [
            super::super::MsgKind::Chat,
            super::super::MsgKind::Bits { amount: 1 },
            super::super::MsgKind::SystemInfo,
        ] {
            let mut message = test_hype_message("train", "begin", 1, 0, 1);
            message.msg_kind = kind;
            room.push_message(message);
        }
        room.push_message(moderation_notice(
            10,
            true,
            super::super::MsgKind::ChatCleared,
            10,
        ));
        room.delete_all_chat_messages();
        assert_eq!(
            room.messages
                .iter()
                .map(|message| message.flags.is_deleted)
                .collect::<Vec<_>>(),
            [true, true, false, false]
        );
        assert!(room.absorb_moderation_notice(&moderation_notice(
            11,
            false,
            super::super::MsgKind::ChatCleared,
            11
        )));
        assert_eq!(room.messages.len(), 4);
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
                original_display_name: None,
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
