use crate::model::{ChannelId, ChatMessage};

/// Flag-style predicates (`is:highlighted`, `is:sub`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    Highlighted,
    Announcement,
    Sub,
    Reply,
    Action,
    FirstMsg,
    Pinned,
    Deleted,
    SelfMsg,
    System,
    Timeout,
    Cheer,
    Redemption,
    Shared,
}

/// A single compiled predicate in a search query.
///
/// All predicates in a query are AND-combined when evaluated via [`matches`].
#[derive(Debug, Clone)]
pub enum Predicate {
    /// Case-insensitive substring match on the complete message text.
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
    /// Complete message text matches the compiled regex.
    Regex(regex::Regex),
    /// Sender badge `name` matches any entry (case-insensitive).
    Badge(Vec<String>),
    /// Subscriber tier matches any entry; plain month counts indicate tier 1.
    Subtier(Vec<char>),
    /// Message-flags / kind predicate.
    Flag(FlagKind),
    /// Any one of the listed message flags or kinds matches.
    AnyFlag(Vec<FlagKind>),
    /// Negates the inner predicate.
    Negated(Box<Predicate>),
}

/// Evaluate `preds` against `msg` with AND semantics.
///
/// Returns `true` only when every predicate matches.
pub fn matches(preds: &[Predicate], msg: &ChatMessage, channel: &ChannelId) -> bool {
    preds.iter().all(|p| p.matches(msg, channel))
}

/// Compare ASCII chat text without allocating lowercase copies. Unicode uses
/// the same lowercase mapping as the search parser.
pub fn contains_ignore_case(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if text.is_ascii() && needle.is_ascii() {
        text.as_bytes()
            .windows(needle.len())
            .any(|part| part.eq_ignore_ascii_case(needle.as_bytes()))
    } else {
        text.to_lowercase().contains(&needle.to_lowercase())
    }
}

fn equals_ignore_case(left: &str, right: &str) -> bool {
    if left.is_ascii() && right.is_ascii() {
        left.eq_ignore_ascii_case(right)
    } else {
        left.to_lowercase() == right.to_lowercase()
    }
}

impl Predicate {
    pub fn matches(&self, msg: &ChatMessage, channel: &ChannelId) -> bool {
        match self {
            Self::Substring(needle) => contains_ignore_case(&msg.plain_text(), needle),
            Self::Author(names) => names.iter().any(|name| {
                equals_ignore_case(&msg.sender.login, name)
                    || equals_ignore_case(&msg.sender.display_name, name)
            }),
            Self::Channel(names) => names
                .iter()
                .any(|name| equals_ignore_case(channel.display_name(), name)),
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
            Self::Regex(re) => re.is_match(&msg.plain_text()),
            Self::Negated(inner) => !inner.matches(msg, channel),
            Self::AnyFlag(flags) => flags
                .iter()
                .any(|flag| Self::Flag(*flag).matches(msg, channel)),
            Self::Badge(names) => msg.sender.badges.iter().any(|badge| {
                names.iter().any(|name| equals_ignore_case(&badge.name, name))
            }),
            Self::Subtier(tiers) => msg
                .sender
                .badges
                .iter()
                .find(|b| b.name.eq_ignore_ascii_case("subscriber"))
                .map(|b| {
                    if b.version.len() > 3 {
                        b.version.chars().next().unwrap_or('1')
                    } else {
                        '1'
                    }
                })
                .map(|c| tiers.contains(&c))
                .unwrap_or(false),
            Self::Flag(kind) => match kind {
                FlagKind::Highlighted => msg.flags.is_highlighted,
                FlagKind::Announcement => matches!(msg.msg_kind, crate::model::MsgKind::Announcement { .. }),
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
                        | crate::model::MsgKind::ChatCleared
                        | crate::model::MsgKind::Raid { .. }
                        | crate::model::MsgKind::HypeTrain { .. }
                ),
                FlagKind::Timeout => matches!(
                    msg.msg_kind,
                    crate::model::MsgKind::Timeout { .. } | crate::model::MsgKind::Ban { .. }
                ),
                FlagKind::Cheer => matches!(msg.msg_kind, crate::model::MsgKind::Bits { .. }),
                FlagKind::Redemption => {
                    msg.flags.custom_reward_id.is_some()
                        || matches!(
                            msg.msg_kind,
                            crate::model::MsgKind::ChannelPointsReward { .. }
                        )
                }
                FlagKind::Shared => msg.shared.is_some(),
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
                original_display_name: None,
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
    fn case_insensitive_comparisons_preserve_unicode_lowercase_semantics() {
        for text in ["", "A short CHAT line", "ALICE", "Straße", "İSTANBUL", "Kelvin", "ΟΣ", "猫 😀"] {
            for needle in ["", "chat", "Alice", "ß", "ss", "i", "i\u{307}", "k", "ος", "οσ", "😀", "longer than any message here"] {
                assert_eq!(contains_ignore_case(text, needle),
                    text.to_lowercase().contains(&needle.to_lowercase()),
                    "contains({text:?}, {needle:?})");
                assert_eq!(equals_ignore_case(text, needle),
                    text.to_lowercase() == needle.to_lowercase(),
                    "equals({text:?}, {needle:?})");
            }
        }
    }

    #[test]
    #[ignore = "controlled search comparison benchmark"]
    fn benchmark_case_insensitive_search() {
        use std::hint::black_box;
        use std::time::Instant;

        let lines: Vec<_> = (0..20_000).map(|index| {
            format!("viewer{index}: That was a great stream today! {} See you next time.",
                if index % 5 == 0 { "Needle Kappa" } else { "Thanks everyone" })
        }).collect();
        let mut baseline = Vec::new();
        let mut candidate = Vec::new();
        for _ in 0..11 {
            let started = Instant::now();
            let before = lines.iter().filter(|text| {
                black_box(text.as_str()).to_lowercase().contains(&black_box("NEEDLE").to_lowercase())
            }).count();
            baseline.push(started.elapsed());
            let started = Instant::now();
            let after = lines.iter().filter(|text| {
                contains_ignore_case(black_box(text.as_str()), black_box("NEEDLE"))
            }).count();
            candidate.push(started.elapsed());
            assert_eq!(before, after);
            assert_eq!(after, 4_000);
        }
        baseline.sort();
        candidate.sort();
        eprintln!("20,000 ASCII chat lines: lowercase median {:?}, comparison median {:?}",
            baseline[5], candidate[5]);
    }

    #[test]
    fn subscription_search_matches_notices_and_bodies_from_new_and_legacy_logs() {
        let notice = "Alice resubscribed with Tier 1! (2 months)";
        let body = "Thank you café 🙂";
        let joined = format!("{notice}\n{body}");
        let channel = make_channel("room");
        for text in [notice, body, joined.as_str()] {
            let mut message = make_msg("alice", "Alice", text);
            message.msg_kind = MsgKind::Sub {
                display_name: "Alice".into(), months: 2, plan: "Tier 1".into(),
                is_gift: false, sub_msg: body.into(),
            };
            assert!(matches(&[
                Predicate::Flag(FlagKind::Sub),
                Predicate::Author(vec!["ALICE".into()]),
                Predicate::Substring("RESUBSCRIBED".into()),
                Predicate::Substring("CAFÉ".into()),
                Predicate::Regex(regex::Regex::new(r"Thank you café 🙂$").unwrap()),
            ], &message, &channel));
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
            zero_width: false,
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
    fn ordinary_subscriber_badges_always_match_tier_one() {
        let channel = make_channel("foo");
        let mut msg = make_msg("alice", "Alice", "hello");
        for version in ["0", "1", "2", "3", "6", "24", "36", "999"] {
            msg.sender.badges = vec![Badge {
                name: "subscriber".into(),
                version: version.into(),
                url: None,
            }];
            for tier in ['1', '2', '3'] {
                assert_eq!(
                    Predicate::Subtier(vec![tier]).matches(&msg, &channel),
                    tier == '1',
                    "badge {version}, tier {tier}"
                );
            }
        }
    }

    #[test]
    fn parsed_literal_commands_do_not_match_messages_without_the_prefix() {
        let channel = make_channel("foo");
        for literal in ["!uptime", "-42", "!foo:bar"] {
            let parsed = crate::search::parse(literal);
            assert!(matches(
                &parsed.predicates,
                &make_msg("alice", "Alice", literal),
                &channel
            ));
            assert!(!matches(
                &parsed.predicates,
                &make_msg("alice", "Alice", &literal[1..]),
                &channel
            ));
        }
    }

    #[test]
    fn parsed_flag_groups_use_or_and_negate_the_complete_group() {
        let channel = make_channel("foo");
        let mut msg = make_msg("alice", "Alice", "hello");
        let positive = crate::search::parse("is:deleted,highlighted");
        let negative = crate::search::parse("!is:deleted,highlighted");
        for (deleted, highlighted) in [(false, false), (true, false), (false, true), (true, true)] {
            msg.flags.is_deleted = deleted;
            msg.flags.is_highlighted = highlighted;
            assert_eq!(
                matches(&positive.predicates, &msg, &channel),
                deleted || highlighted
            );
            assert_eq!(
                matches(&negative.predicates, &msg, &channel),
                !(deleted || highlighted)
            );
        }
    }

    #[test]
    fn system_search_excludes_timeouts_unless_requested() {
        let channel = make_channel("foo");
        let mut msg = make_msg("alice", "Alice", "alice timed out");
        msg.msg_kind = MsgKind::Timeout {
            login: "alice".into(),
            seconds: 60,
        };
        assert!(!matches(
            &crate::search::parse("is:system").predicates,
            &msg,
            &channel
        ));
        assert!(matches(
            &crate::search::parse("is:system,timeout").predicates,
            &msg,
            &channel
        ));
        msg.msg_kind = MsgKind::SystemInfo;
        assert!(matches(
            &crate::search::parse("is:system,timeout").predicates,
            &msg,
            &channel
        ));
        assert!(!matches(
            &crate::search::parse("is:timeout").predicates,
            &msg,
            &channel
        ));
    }

    #[test]
    fn supported_event_flags_and_aliases_match_message_metadata() {
        let channel = make_channel("foo");
        let mut msg = make_msg("alice", "Alice", "hello");
        for query in [
            "is:cheer-msg",
            "is:redemption",
            "is:shared",
            "is:first-msg",
            "is:disabled",
        ] {
            assert!(
                !matches(&crate::search::parse(query).predicates, &msg, &channel),
                "{query}"
            );
        }
        msg.msg_kind = MsgKind::Bits { amount: 100 };
        msg.flags.custom_reward_id = Some("reward".into());
        msg.shared = Some(Default::default());
        msg.flags.is_first_msg = true;
        msg.flags.is_deleted = true;
        for query in [
            "is:cheer-msg",
            "is:redemption",
            "is:shared",
            "is:first-msg",
            "is:disabled",
        ] {
            assert!(
                matches(&crate::search::parse(query).predicates, &msg, &channel),
                "{query}"
            );
        }
    }

    #[test]
    fn announcement_search_supports_negation_author_and_flag_alternatives() {
        let channel = make_channel("foo");
        let mut msg = make_msg("alice", "Alice", "a colorful notice");
        for announcement in [false, true] {
            msg.msg_kind = if announcement {
                MsgKind::Announcement { color: "blue".into() }
            } else { MsgKind::Chat };
            for query in ["is:announcement", "from:alice is:announcement colorful", "is:sub,announcement"] {
                assert_eq!(matches(&crate::search::parse(query).predicates, &msg, &channel), announcement, "{query}");
            }
            assert_eq!(matches(&crate::search::parse("!is:announcement").predicates, &msg, &channel), !announcement);
            assert!(!matches(&crate::search::parse("is:system").predicates, &msg, &channel));
        }
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
            ..Default::default()
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
