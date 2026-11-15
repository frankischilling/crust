use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

use super::{ChannelId, ChatMessage, MsgKind};

pub const MODERATION_RULE_LIMIT: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModerationTarget {
    Message(String),
    User {
        login: String,
        user_id: Option<String>,
    },
    Channel,
}

/// A source timestamp limits a timeout or clear to messages sent before it.
/// Without one, only messages already retained when the action arrives are
/// affected; a delayed action must not invent a cutoff for newer history.
#[derive(Debug, Clone)]
pub struct ModerationAction {
    pub channel: ChannelId,
    pub target: ModerationTarget,
    pub timestamp: Option<DateTime<Utc>>,
}

impl ModerationAction {
    pub fn message(channel: ChannelId, server_id: String) -> Self {
        Self {
            channel,
            target: ModerationTarget::Message(server_id),
            timestamp: None,
        }
    }

    pub fn user(
        channel: ChannelId,
        login: String,
        user_id: Option<String>,
        timestamp: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            channel,
            target: ModerationTarget::User {
                login: login.trim().to_ascii_lowercase(),
                user_id: user_id
                    .map(|id| id.trim().to_owned())
                    .filter(|id| !id.is_empty()),
            },
            timestamp,
        }
    }

    pub fn channel(channel: ChannelId, timestamp: Option<DateTime<Utc>>) -> Self {
        Self {
            channel,
            target: ModerationTarget::Channel,
            timestamp,
        }
    }

    pub fn is_channel_action(&self) -> bool {
        !self.channel.as_str().starts_with("whisper:") && self.channel != ChannelId::mentions()
    }

    pub fn matches(&self, message: &ChatMessage) -> bool {
        if !self.is_channel_action() || message.channel != self.channel {
            return false;
        }
        if let ModerationTarget::Message(server_id) = &self.target {
            return !server_id.is_empty() && message.server_id.as_ref() == Some(server_id);
        }
        if !is_chat_message(message)
            || self.timestamp.is_some_and(|cutoff| {
                message.timestamp.timestamp_millis() > cutoff.timestamp_millis()
            })
        {
            return false;
        }
        match &self.target {
            ModerationTarget::User { login, user_id } => {
                if let Some(user_id) = user_id
                    .as_ref()
                    .filter(|_| !message.sender.user_id.0.is_empty())
                {
                    message.sender.user_id.0 == *user_id
                } else {
                    !login.is_empty() && message.sender.login.eq_ignore_ascii_case(login)
                }
            }
            ModerationTarget::Channel => true,
            ModerationTarget::Message(_) => unreachable!(),
        }
    }
}

fn is_chat_message(message: &ChatMessage) -> bool {
    matches!(
        message.msg_kind,
        MsgKind::Chat | MsgKind::Bits { .. } | MsgKind::SuspiciousUserMessage
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RuleKey {
    Message(ChannelId, String),
    User(ChannelId, String),
    Login(ChannelId, String),
    UnknownUserLogin(ChannelId, String),
    Channel(ChannelId),
    Local {
        channel: ChannelId,
        timestamp_ms: i64,
        user_id: String,
        login: String,
        text: String,
    },
}

impl RuleKey {
    fn message(message: &ChatMessage) -> Self {
        if let Some(server_id) = message.server_id.as_ref().filter(|id| !id.is_empty()) {
            Self::Message(message.channel.clone(), server_id.clone())
        } else {
            Self::Local {
                channel: message.channel.clone(),
                timestamp_ms: message.timestamp.timestamp_millis(),
                user_id: message.sender.user_id.0.clone(),
                login: message.sender.login.to_ascii_lowercase(),
                text: message.raw_text.clone(),
            }
        }
    }
}

#[derive(Debug, Default)]
struct Rules {
    entries: HashMap<RuleKey, Option<i64>>,
    order: VecDeque<RuleKey>,
}

impl Rules {
    fn insert(&mut self, key: RuleKey, cutoff: Option<i64>) {
        if let Some(previous) = self.entries.get(&key) {
            if previous.is_none()
                || cutoff.is_some_and(|cutoff| previous.is_some_and(|old| old >= cutoff))
            {
                return;
            }
            self.order.retain(|existing| existing != &key);
        }
        self.entries.insert(key.clone(), cutoff);
        self.order.push_back(key);
        while self.entries.len() > MODERATION_RULE_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn remember(&mut self, action: &ModerationAction) {
        if !action.is_channel_action() {
            return;
        }
        match &action.target {
            ModerationTarget::Message(id) if !id.is_empty() => {
                self.insert(RuleKey::Message(action.channel.clone(), id.clone()), None);
            }
            ModerationTarget::User { login, user_id } => {
                let Some(cutoff) = action.timestamp.map(|time| time.timestamp_millis()) else {
                    return;
                };
                if let Some(user_id) = user_id {
                    self.insert(
                        RuleKey::User(action.channel.clone(), user_id.clone()),
                        Some(cutoff),
                    );
                    if !login.is_empty() {
                        self.insert(
                            RuleKey::UnknownUserLogin(action.channel.clone(), login.clone()),
                            Some(cutoff),
                        );
                    }
                } else if !login.is_empty() {
                    self.insert(
                        RuleKey::Login(action.channel.clone(), login.clone()),
                        Some(cutoff),
                    );
                }
            }
            ModerationTarget::Channel => {
                if let Some(cutoff) = action.timestamp {
                    self.insert(
                        RuleKey::Channel(action.channel.clone()),
                        Some(cutoff.timestamp_millis()),
                    );
                }
            }
            _ => {}
        }
    }

    fn deletes(&self, message: &ChatMessage) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        if self.entries.contains_key(&RuleKey::message(message)) {
            return true;
        }
        if !is_chat_message(message) {
            return false;
        }
        let before = |key: RuleKey| {
            self.entries.get(&key).is_some_and(|cutoff| {
                cutoff.is_some_and(|cutoff| message.timestamp.timestamp_millis() <= cutoff)
            })
        };
        before(RuleKey::Channel(message.channel.clone()))
            || before(RuleKey::User(
                message.channel.clone(),
                message.sender.user_id.0.clone(),
            ))
            || before(RuleKey::Login(
                message.channel.clone(),
                message.sender.login.to_ascii_lowercase(),
            ))
            || (message.sender.user_id.0.is_empty()
                && before(RuleKey::UnknownUserLogin(
                    message.channel.clone(),
                    message.sender.login.to_ascii_lowercase(),
                )))
    }
}

/// Shared by live rooms and Mentions so late history retains moderation state.
/// The bound covers all open and closed channels, including untimed identities.
#[derive(Debug, Clone, Default)]
pub struct ModerationState(Arc<RwLock<Rules>>);

impl ModerationState {
    pub fn remember(&self, actions: &[ModerationAction]) {
        let mut rules = self.0.write().unwrap();
        for action in actions {
            rules.remember(action);
        }
    }

    pub fn is_deleted(&self, message: &ChatMessage) -> bool {
        self.0.read().unwrap().deletes(message)
    }

    pub fn apply(&self, message: &mut ChatMessage) {
        if self.is_deleted(message) {
            message.flags.is_deleted = true;
        }
    }

    pub fn apply_all<'a>(&self, messages: impl IntoIterator<Item = &'a mut ChatMessage>) {
        let rules = self.0.read().unwrap();
        for message in messages {
            if rules.deletes(message) {
                message.flags.is_deleted = true;
            }
        }
    }

    pub fn observe_deleted(&self, message: &ChatMessage) {
        if message.flags.is_deleted && !message.channel.as_str().starts_with("whisper:") {
            let mut rules = self.0.write().unwrap();
            if !rules.deletes(message) {
                rules.insert(RuleKey::message(message), None);
            }
        }
    }

    pub fn apply_to_retained<'a>(
        &self,
        action: &ModerationAction,
        messages: impl IntoIterator<Item = &'a mut ChatMessage>,
    ) {
        let mut rules = self.0.write().unwrap();
        rules.remember(action);
        for message in messages {
            if action.matches(message) {
                message.flags.is_deleted = true;
                if action.timestamp.is_none() && !rules.deletes(message) {
                    rules.insert(RuleKey::message(message), None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageFlags, MessageId, Sender, UserId};
    use chrono::TimeZone;

    fn message(id: &str, channel: &str, user: &str, timestamp_ms: i64) -> ChatMessage {
        ChatMessage {
            id: MessageId(1),
            server_id: Some(id.into()),
            channel: ChannelId::new(channel),
            timestamp: Utc.timestamp_millis_opt(timestamp_ms).unwrap(),
            sender: Sender {
                user_id: UserId(user.into()),
                login: "alice".into(),
                display_name: "Alice".into(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: "original text".into(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn timed_user_rules_match_identity_channel_and_source_chronology() {
        let state = ModerationState::default();
        let action = ModerationAction::user(
            ChannelId::new("first"),
            "ALICE".into(),
            Some("10".into()),
            Some(Utc.timestamp_millis_opt(1000).unwrap()),
        );
        state.apply_to_retained(&action, std::iter::empty());
        for (channel, user, time, deleted) in [
            ("first", "10", 999, true),
            ("first", "10", 1000, true),
            ("first", "10", 1001, false),
            ("second", "10", 999, false),
            ("first", "20", 999, false),
            ("first", "", 999, true),
        ] {
            let mut row = message("same-id", channel, user, time);
            state.apply(&mut row);
            assert_eq!(row.flags.is_deleted, deleted, "{channel}/{user}/{time}");
            assert_eq!(row.raw_text, "original text");
        }
        let mut notice = message("notice", "first", "10", 999);
        notice.msg_kind = MsgKind::SystemInfo;
        state.apply(&mut notice);
        assert!(!notice.flags.is_deleted);
    }

    #[test]
    fn untimed_actions_remember_only_retained_identities() {
        let state = ModerationState::default();
        let mut retained = message("old", "room", "10", 1000);
        state.apply_to_retained(
            &ModerationAction::user(retained.channel.clone(), "alice".into(), None, None),
            [&mut retained],
        );
        let mut replay = message("old", "room", "10", 1000);
        replay.flags.is_history = true;
        state.apply(&mut replay);
        assert!(replay.flags.is_deleted);
        let mut newer = message("new", "room", "10", 1001);
        state.apply(&mut newer);
        assert!(!newer.flags.is_deleted);
        let mut other_channel = message("old", "elsewhere", "10", 1000);
        state.apply(&mut other_channel);
        assert!(!other_channel.flags.is_deleted);
    }

    #[test]
    fn older_cutoffs_never_replace_newer_actions_and_rules_stay_bounded() {
        let state = ModerationState::default();
        for cutoff in [1000, 500] {
            state.apply_to_retained(
                &ModerationAction::channel(
                    ChannelId::new("room"),
                    Some(Utc.timestamp_millis_opt(cutoff).unwrap()),
                ),
                std::iter::empty(),
            );
        }
        let mut row = message("late", "room", "10", 750);
        state.apply(&mut row);
        assert!(row.flags.is_deleted);
        for id in 0..MODERATION_RULE_LIMIT + 10 {
            state.apply_to_retained(
                &ModerationAction::message(ChannelId::new("room"), id.to_string()),
                std::iter::empty(),
            );
        }
        let rules = state.0.read().unwrap();
        assert_eq!(rules.entries.len(), MODERATION_RULE_LIMIT);
        assert_eq!(rules.order.len(), MODERATION_RULE_LIMIT);
    }

    #[test]
    fn covered_history_rows_do_not_evict_the_source_cutoff() {
        let state = ModerationState::default();
        state.apply_to_retained(
            &ModerationAction::channel(
                ChannelId::new("room"),
                Some(Utc.timestamp_millis_opt(1000).unwrap()),
            ),
            std::iter::empty(),
        );
        for id in 0..MODERATION_RULE_LIMIT + 100 {
            let mut row = message(&id.to_string(), "room", "10", 999);
            row.flags.is_deleted = true;
            state.observe_deleted(&row);
        }
        assert_eq!(state.0.read().unwrap().entries.len(), 1);
        assert!(state.is_deleted(&message("later-page", "room", "10", 999)));
        assert!(!state.is_deleted(&message("new-message", "room", "10", 1001)));
    }

    #[test]
    fn channel_actions_never_mutate_private_history() {
        let mut row = message("private", "room", "10", 1);
        row.channel = ChannelId("whisper:uid:1:alice".into());
        let state = ModerationState::default();
        state.apply_to_retained(
            &ModerationAction::channel(row.channel.clone(), None),
            [&mut row],
        );
        assert!(!row.flags.is_deleted);
        state.apply_to_retained(
            &ModerationAction::message(row.channel.clone(), "private".into()),
            [&mut row],
        );
        assert!(!row.flags.is_deleted);
    }
}
