use crate::model::{ChannelId, ChatMessage};

/// Flag-style predicates (`is:highlighted`, `is:sub`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    Highlighted,
    Sub,
    Reply,
    Action,
    FirstMsg,
    Pinned,
    Deleted,
    SelfMsg,
    System,
}

/// A single compiled predicate in a search query.
///
/// All predicates in a query are AND-combined when evaluated via [`matches`].
#[derive(Debug, Clone)]
pub enum Predicate {
    /// Case-insensitive substring match on `raw_text`.
    Substring(String),
    /// Sender login OR display name matches any entry (case-insensitive).
    Author(Vec<String>),
    /// Channel display name matches any entry (case-insensitive).
    Channel(Vec<String>),
    /// Message contains at least one `Span::Url`.
    Link,
    /// Message contains at least one `Span::Mention` or `flags.is_mention`.
    Mention,
    /// Message contains at least one `Span::Emote` or `Span::Emoji`.
    Emote,
    /// Message `raw_text` matches compiled regex.
    Regex(regex::Regex),
    /// Sender badge `name` matches any entry (case-insensitive).
    Badge(Vec<String>),
    /// First char of `subscriber` badge `version` matches any entry.
    Subtier(Vec<char>),
    /// Message-flags / kind predicate.
    Flag(FlagKind),
    /// Negates the inner predicate.
    Negated(Box<Predicate>),
}

/// Evaluate `preds` against `msg` with AND semantics.
///
/// Returns `true` only when every predicate matches.
pub fn matches(preds: &[Predicate], msg: &ChatMessage, channel: &ChannelId) -> bool {
    preds.iter().all(|p| p.matches(msg, channel))
}

impl Predicate {
    pub fn matches(&self, msg: &ChatMessage, channel: &ChannelId) -> bool {
        match self {
            Self::Substring(needle) => {
                let needle = needle.to_lowercase();
                msg.raw_text.to_lowercase().contains(&needle)
            }
            Self::Author(names) => {
                let login = msg.sender.login.to_lowercase();
                let display = msg.sender.display_name.to_lowercase();
                names.iter().any(|n| {
                    let n = n.to_lowercase();
                    login == n || display == n
                })
            }
            Self::Channel(names) => {
                let display = channel.display_name().to_lowercase();
                names.iter().any(|n| n.to_lowercase() == display)
            }
            Self::Link => msg
                .spans
                .iter()
                .any(|s| matches!(s, crate::model::Span::Url { .. })),
            Self::Mention => {
                if msg.flags.is_mention {
                    return true;
                }
                msg.spans
                    .iter()
                    .any(|s| matches!(s, crate::model::Span::Mention { .. }))
            }
            Self::Emote => msg.spans.iter().any(|s| {
                matches!(
                    s,
                    crate::model::Span::Emote { .. } | crate::model::Span::Emoji { .. }
                )
            }),
            Self::Regex(re) => re.is_match(&msg.raw_text),
            Self::Negated(inner) => !inner.matches(msg, channel),
            Self::Badge(names) => msg.sender.badges.iter().any(|b| {
                let b_name = b.name.to_lowercase();
                names.iter().any(|n| n.to_lowercase() == b_name)
            }),
            Self::Subtier(tiers) => msg
                .sender
                .badges
                .iter()
                .find(|b| b.name.eq_ignore_ascii_case("subscriber"))
                .and_then(|b| b.version.chars().next())
                .map(|c| tiers.contains(&c))
                .unwrap_or(false),
            Self::Flag(kind) => match kind {
                FlagKind::Highlighted => msg.flags.is_highlighted,
                FlagKind::Action => msg.flags.is_action,
                FlagKind::FirstMsg => msg.flags.is_first_msg,
                FlagKind::Pinned => msg.flags.is_pinned,
                FlagKind::Deleted => msg.flags.is_deleted,
                FlagKind::SelfMsg => msg.flags.is_self,
                FlagKind::Reply => msg.reply.is_some(),
                FlagKind::Sub => matches!(msg.msg_kind, crate::model::MsgKind::Sub { .. }),
                FlagKind::System => matches!(
                    msg.msg_kind,
                    crate::model::MsgKind::SystemInfo
                        | crate::model::MsgKind::Timeout { .. }
                        | crate::model::MsgKind::Ban { .. }
                        | crate::model::MsgKind::ChatCleared
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};
    use chrono::Utc;
    use smallvec::SmallVec;

    fn make_channel(name: &str) -> ChannelId {
        ChannelId::new(name)
    }

    fn make_msg(login: &str, display: &str, text: &str) -> ChatMessage {
        ChatMessage {
            id: MessageId(1),
            server_id: None,
            timestamp: Utc::now(),
            channel: ChannelId::new("testchannel"),
            sender: Sender {
                user_id: UserId("1".to_string()),
                login: login.to_string(),
                display_name: display.to_string(),
                color: None,
                name_paint: None,
                badges: vec![],
            },
            raw_text: text.to_string(),
            spans: SmallVec::new(),
            twitch_emotes: vec![],
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn substring_matches_case_insensitively() {
        let msg = make_msg("alice", "Alice", "Hello World");
        let ch = make_channel("foo");
        assert!(Predicate::Substring("hello".into()).matches(&msg, &ch));
        assert!(Predicate::Substring("WORLD".into()).matches(&msg, &ch));
        assert!(!Predicate::Substring("xyz".into()).matches(&msg, &ch));
    }

    #[test]
    fn author_matches_login_or_display_case_insensitive() {
        let msg = make_msg("alice", "AliceDisplay", "hi");
        let ch = make_channel("foo");
        assert!(Predicate::Author(vec!["ALICE".into()]).matches(&msg, &ch));
        assert!(Predicate::Author(vec!["alicedisplay".into()]).matches(&msg, &ch));
        assert!(!Predicate::Author(vec!["bob".into()]).matches(&msg, &ch));
        // multi-value OR
        assert!(Predicate::Author(vec!["bob".into(), "alice".into()]).matches(&msg, &ch));
    }

    #[test]
    fn channel_matches_display_name_case_insensitive() {
        let msg = make_msg("u", "U", "t");
        let ch = make_channel("MyChannel");
        assert!(Predicate::Channel(vec!["mychannel".into()]).matches(&msg, &ch));
        assert!(!Predicate::Channel(vec!["other".into()]).matches(&msg, &ch));
    }

    use crate::model::Span;

    #[test]
    fn link_matches_when_span_url_present() {
        let ch = make_channel("foo");
        let msg_no = make_msg("u", "U", "no url");
        let mut msg_yes = make_msg("u", "U", "see http://x");
        msg_yes.spans.push(Span::Url {
            text: "http://x".into(),
            url: "http://x".into(),
        });
        assert!(!Predicate::Link.matches(&msg_no, &ch));
        assert!(Predicate::Link.matches(&msg_yes, &ch));
    }

    #[test]
    fn mention_matches_span_or_flag() {
        let ch = make_channel("foo");
        let mut via_span = make_msg("u", "U", "@bob hi");
        via_span.spans.push(Span::Mention {
            login: "bob".into(),
        });
        let mut via_flag = make_msg("u", "U", "hi");
        via_flag.flags.is_mention = true;
        let no_mention = make_msg("u", "U", "nope");
        assert!(Predicate::Mention.matches(&via_span, &ch));
        assert!(Predicate::Mention.matches(&via_flag, &ch));
        assert!(!Predicate::Mention.matches(&no_mention, &ch));
    }

    #[test]
    fn emote_matches_twitch_or_third_party_or_emoji() {
        let ch = make_channel("foo");
        let mut with_emote = make_msg("u", "U", "Kappa");
        with_emote.spans.push(Span::Emote {
            id: "25".into(),
            code: "Kappa".into(),
            url: "https://x".into(),
            url_hd: None,
            provider: "twitch".into(),
        });
        let mut with_emoji = make_msg("u", "U", "😀");
        with_emoji.spans.push(Span::Emoji {
            text: "😀".into(),
            url: "https://twemoji".into(),
        });
        let plain = make_msg("u", "U", "no emote");
        assert!(Predicate::Emote.matches(&with_emote, &ch));
        assert!(Predicate::Emote.matches(&with_emoji, &ch));
        assert!(!Predicate::Emote.matches(&plain, &ch));
    }

    #[test]
    fn regex_matches_raw_text() {
        let ch = make_channel("foo");
        let msg = make_msg("u", "U", "!ban alice 10m");
        let re = regex::Regex::new(r"^!ban\s+\w+").unwrap();
        assert!(Predicate::Regex(re).matches(&msg, &ch));
        let re2 = regex::Regex::new(r"^!timeout").unwrap();
        assert!(!Predicate::Regex(re2).matches(&msg, &ch));
    }

    #[test]
    fn negated_inverts_inner() {
        let ch = make_channel("foo");
        let msg = make_msg("alice", "A", "hi");
        let inner = Predicate::Author(vec!["alice".into()]);
        let negated = Predicate::Negated(Box::new(inner));
        assert!(!negated.matches(&msg, &ch));

        let inner2 = Predicate::Author(vec!["bob".into()]);
        let negated2 = Predicate::Negated(Box::new(inner2));
        assert!(negated2.matches(&msg, &ch));
    }

    use crate::model::Badge;

    #[test]
    fn badge_matches_by_name_case_insensitive() {
        let ch = make_channel("foo");
        let mut m = make_msg("u", "U", "t");
        m.sender.badges = vec![
            Badge {
                name: "moderator".into(),
                version: "1".into(),
                url: None,
            },
            Badge {
                name: "subscriber".into(),
                version: "12".into(),
                url: None,
            },
        ];
        assert!(Predicate::Badge(vec!["moderator".into()]).matches(&m, &ch));
        assert!(Predicate::Badge(vec!["MODERATOR".into()]).matches(&m, &ch));
        assert!(Predicate::Badge(vec!["subscriber".into()]).matches(&m, &ch));
        assert!(!Predicate::Badge(vec!["vip".into()]).matches(&m, &ch));
    }

    #[test]
    fn subtier_matches_first_char_of_subscriber_version() {
        let ch = make_channel("foo");
        let mut tier3 = make_msg("u", "U", "t");
        tier3.sender.badges = vec![Badge {
            name: "subscriber".into(),
            version: "3012".into(),
            url: None,
        }];
        let mut tier1 = make_msg("u", "U", "t");
        tier1.sender.badges = vec![Badge {
            name: "subscriber".into(),
            version: "12".into(),
            url: None,
        }];
        let no_sub = make_msg("u", "U", "t");
        assert!(Predicate::Subtier(vec!['3']).matches(&tier3, &ch));
        assert!(!Predicate::Subtier(vec!['1']).matches(&tier3, &ch));
        assert!(Predicate::Subtier(vec!['1']).matches(&tier1, &ch));
        assert!(!Predicate::Subtier(vec!['1']).matches(&no_sub, &ch));
    }

    #[test]
    fn flag_highlighted_matches_is_highlighted() {
        let ch = make_channel("foo");
        let mut m = make_msg("u", "U", "t");
        m.flags.is_highlighted = true;
        assert!(Predicate::Flag(FlagKind::Highlighted).matches(&m, &ch));
        m.flags.is_highlighted = false;
        assert!(!Predicate::Flag(FlagKind::Highlighted).matches(&m, &ch));
    }

    #[test]
    fn flag_reply_matches_when_reply_present() {
        use crate::model::ReplyInfo;
        let ch = make_channel("foo");
        let mut m = make_msg("u", "U", "t");
        assert!(!Predicate::Flag(FlagKind::Reply).matches(&m, &ch));
        m.reply = Some(ReplyInfo {
            parent_msg_id: "x".into(),
            parent_user_login: "y".into(),
            parent_display_name: "Y".into(),
            parent_msg_body: "hello".into(),
        });
        assert!(Predicate::Flag(FlagKind::Reply).matches(&m, &ch));
    }

    #[test]
    fn flag_sub_matches_sub_kind() {
        let ch = make_channel("foo");
        let mut m = make_msg("u", "U", "t");
        m.msg_kind = MsgKind::Sub {
            display_name: "u".into(),
            months: 1,
            plan: "Tier 1".into(),
            is_gift: false,
            sub_msg: String::new(),
        };
        assert!(Predicate::Flag(FlagKind::Sub).matches(&m, &ch));
        let plain = make_msg("u", "U", "t");
        assert!(!Predicate::Flag(FlagKind::Sub).matches(&plain, &ch));
    }
}
