//! Ignored users + ignored phrases.
//!
//! [`IgnoredUser`] blocks a user; [`IgnoredPhrase`] applies an action to messages
//! that match a text pattern. The UI
//! manages both in a single settings tab.

use serde::{Deserialize, Serialize};

use crate::model::{ChatMessage, TwitchEmotePos};

fn bool_true() -> bool {
    true
}

/// A user whose messages should be suppressed locally.
///
/// Supports plain substring/exact login, regex patterns, and case sensitivity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoredUser {
    /// Login name (or regex pattern when `is_regex` is true).
    pub login: String,
    #[serde(default)]
    pub is_regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

impl IgnoredUser {
    pub fn new(login: impl Into<String>) -> Self {
        Self {
            login: login.into(),
            is_regex: false,
            case_sensitive: false,
            enabled: true,
        }
    }
}

/// What to do when a message matches an [`IgnoredPhrase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IgnoredPhraseAction {
    /// Drop the message entirely.  The UI never sees it.
    Block,
    /// Replace the matched text with [`IgnoredPhrase::replace_with`] (default `***`).
    Replace,
    /// Do not modify the message; just raise the highlight flag.
    HighlightOnly,
    /// Do not modify the message; just raise the mention flag (for notifications/toasts).
    MentionOnly,
}

impl Default for IgnoredPhraseAction {
    fn default() -> Self {
        Self::Block
    }
}

/// A text pattern combined with an action to apply when a message matches it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoredPhrase {
    pub pattern: String,
    #[serde(default)]
    pub is_regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub action: IgnoredPhraseAction,
    /// Replacement text.  Empty or unused unless `action == Replace`.
    #[serde(default = "default_replace_with")]
    pub replace_with: String,
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

fn default_replace_with() -> String {
    "***".to_owned()
}

impl IgnoredPhrase {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            is_regex: false,
            case_sensitive: false,
            action: IgnoredPhraseAction::Block,
            replace_with: default_replace_with(),
            enabled: true,
        }
    }

    /// Returns true when the pattern string is a valid regex (or when not in regex mode).
    pub fn is_regex_valid(&self) -> bool {
        if !self.is_regex {
            return true;
        }
        let mut b = regex::RegexBuilder::new(&self.pattern);
        b.case_insensitive(!self.case_sensitive);
        b.build().is_ok()
    }
}

// Compiled matchers

enum UserMatcher {
    Exact {
        needle: String,
        case_sensitive: bool,
    },
    Regex(regex::Regex),
}

/// Compiled form of an [`IgnoredUser`] list - precomputes regex/case folding
/// for fast per-message checks.
pub struct CompiledIgnoredUsers {
    matchers: Vec<UserMatcher>,
}

impl CompiledIgnoredUsers {
    pub fn new(users: &[IgnoredUser]) -> Self {
        let mut matchers = Vec::with_capacity(users.len());
        for u in users
            .iter()
            .filter(|u| u.enabled && !u.login.trim().is_empty())
        {
            if u.is_regex {
                let mut b = regex::RegexBuilder::new(&u.login);
                b.case_insensitive(!u.case_sensitive);
                if let Ok(re) = b.build() {
                    matchers.push(UserMatcher::Regex(re));
                }
            } else {
                matchers.push(UserMatcher::Exact {
                    needle: u.login.clone(),
                    case_sensitive: u.case_sensitive,
                });
            }
        }
        Self { matchers }
    }

    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// Returns true when `login` matches any compiled entry.
    pub fn is_ignored(&self, login: &str) -> bool {
        for m in &self.matchers {
            match m {
                UserMatcher::Exact {
                    needle,
                    case_sensitive,
                } => {
                    let hit = if *case_sensitive {
                        login == needle
                    } else {
                        login.eq_ignore_ascii_case(needle)
                    };
                    if hit {
                        return true;
                    }
                }
                UserMatcher::Regex(re) => {
                    if re.is_match(login) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

// Phrase matching

enum PhraseMatcher {
    Substring(String),
    CaseInsensitiveSubstring(regex::Regex),
    Regex(regex::Regex),
}

struct CompiledPhrase {
    matcher: PhraseMatcher,
    action: IgnoredPhraseAction,
    replace_with: String,
}

/// Compiled form of an [`IgnoredPhrase`] list.  Build once and reuse.
pub struct CompiledIgnoredPhrases {
    phrases: Vec<CompiledPhrase>,
}

/// Summary of phrase actions that fired against a single message.
#[derive(Debug, Clone, Default)]
pub struct PhraseApplyOutcome {
    /// A `Block` rule matched - the caller should drop the message.
    pub blocked: bool,
    /// A `HighlightOnly` rule matched.
    pub highlight_only: bool,
    /// A `MentionOnly` rule matched.
    pub mention_only: bool,
    /// True when text was rewritten by a `Replace` rule.
    pub replaced: bool,
}

impl CompiledIgnoredPhrases {
    pub fn new(phrases: &[IgnoredPhrase]) -> Self {
        let mut out = Vec::with_capacity(phrases.len());
        for p in phrases
            .iter()
            .filter(|p| p.enabled && !p.pattern.trim().is_empty())
        {
            let matcher = if p.is_regex {
                let mut b = regex::RegexBuilder::new(&p.pattern);
                b.case_insensitive(!p.case_sensitive);
                match b.build() {
                    Ok(re) => PhraseMatcher::Regex(re),
                    Err(_) => continue, // silently skip invalid regex
                }
            } else if p.case_sensitive {
                PhraseMatcher::Substring(p.pattern.clone())
            } else {
                let mut builder = regex::RegexBuilder::new(&regex::escape(&p.pattern));
                builder.case_insensitive(true);
                match builder.build() {
                    Ok(re) => PhraseMatcher::CaseInsensitiveSubstring(re),
                    Err(_) => continue,
                }
            };
            out.push(CompiledPhrase {
                matcher,
                action: p.action,
                replace_with: p.replace_with.clone(),
            });
        }
        Self { phrases: out }
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }

    /// Apply all compiled phrases to `text`.
    ///
    /// Returns a [`PhraseApplyOutcome`] describing which rule kinds fired.
    /// `text` is mutated in place when a `Replace` action matches.
    pub fn apply(&self, text: &mut String) -> PhraseApplyOutcome {
        self.apply_with_emotes(text, &mut Vec::new())
    }

    /// Apply rules to original message text while retaining native emote identities.
    pub fn apply_to_message(&self, message: &mut ChatMessage) -> PhraseApplyOutcome {
        let outcome = self.apply_with_emotes(&mut message.raw_text, &mut message.twitch_emotes);
        message.flags.is_highlighted |= outcome.highlight_only;
        message.flags.is_mention |= outcome.mention_only;
        outcome
    }

    /// Shift native emote character ranges through replacements. An edit inside
    /// an emote removes that occurrence; unrelated emotes retain their IDs.
    pub fn apply_with_emotes(
        &self,
        text: &mut String,
        emotes: &mut Vec<TwitchEmotePos>,
    ) -> PhraseApplyOutcome {
        let mut out = PhraseApplyOutcome::default();
        for p in &self.phrases {
            let matched = match &p.matcher {
                PhraseMatcher::Substring(needle) => text.contains(needle.as_str()),
                PhraseMatcher::CaseInsensitiveSubstring(re) | PhraseMatcher::Regex(re) => {
                    re.is_match(text)
                }
            };
            if !matched {
                continue;
            }
            match p.action {
                IgnoredPhraseAction::Block => {
                    out.blocked = true;
                    return out; // no point continuing - message is dropped
                }
                IgnoredPhraseAction::HighlightOnly => {
                    out.highlight_only = true;
                }
                IgnoredPhraseAction::MentionOnly => {
                    out.mention_only = true;
                }
                IgnoredPhraseAction::Replace => {
                    let mut writer = PhraseReplacement::new(text, emotes);
                    match &p.matcher {
                        PhraseMatcher::Substring(needle) => {
                            for (start, _) in text.match_indices(needle.as_str()) {
                                writer.replace(start..start + needle.len(), &p.replace_with);
                            }
                        }
                        PhraseMatcher::CaseInsensitiveSubstring(re) => {
                            for matched in re.find_iter(text) {
                                writer.replace(matched.range(), &p.replace_with);
                            }
                        }
                        PhraseMatcher::Regex(re) => {
                            let mut replacement = String::new();
                            for captures in re.captures_iter(text) {
                                let matched = captures.get(0).expect("whole regex match");
                                replacement.clear();
                                captures.expand(&p.replace_with, &mut replacement);
                                writer.replace(matched.range(), &replacement);
                            }
                        }
                    }
                    let replaced = writer.finish();
                    if replaced != *text {
                        *text = replaced;
                        out.replaced = true;
                    }
                }
            }
        }
        out
    }
}

struct PhraseReplacement<'a> {
    original: &'a str,
    output: String,
    byte_cursor: usize,
    character_cursor: usize,
    emotes: &'a mut Vec<TwitchEmotePos>,
}

impl<'a> PhraseReplacement<'a> {
    fn new(original: &'a str, emotes: &'a mut Vec<TwitchEmotePos>) -> Self {
        Self {
            original,
            output: String::with_capacity(original.len()),
            byte_cursor: 0,
            character_cursor: 0,
            emotes,
        }
    }

    fn replace(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let unchanged = &self.original[self.byte_cursor..range.start];
        self.output.push_str(unchanged);
        self.character_cursor += unchanged.chars().count();

        let matched = &self.original[range.clone()];
        let removed = matched.chars().count();
        let inserted = replacement.chars().count();
        if matched != replacement {
            let start = self.character_cursor;
            let end = start + removed;
            self.emotes.retain_mut(|emote| {
                let Some(emote_end) = emote.end.checked_add(1) else {
                    return false;
                };
                if emote.start >= emote_end {
                    return false;
                }
                let overlaps = if removed == 0 {
                    emote.start < start && start < emote_end
                } else {
                    emote.start < end && start < emote_end
                };
                if overlaps {
                    return false;
                }
                if emote.start >= end {
                    let shifted_start = emote
                        .start
                        .checked_sub(removed)
                        .and_then(|value| value.checked_add(inserted));
                    let shifted_end = emote
                        .end
                        .checked_sub(removed)
                        .and_then(|value| value.checked_add(inserted));
                    let (Some(shifted_start), Some(shifted_end)) = (shifted_start, shifted_end)
                    else {
                        return false;
                    };
                    emote.start = shifted_start;
                    emote.end = shifted_end;
                }
                true
            });
        }
        self.output.push_str(replacement);
        self.character_cursor += inserted;
        self.byte_cursor = range.end;
    }

    fn finish(mut self) -> String {
        self.output.push_str(&self.original[self.byte_cursor..]);
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_user_ignore() {
        let compiled = CompiledIgnoredUsers::new(&[IgnoredUser::new("Spammer")]);
        assert!(compiled.is_ignored("spammer"));
        assert!(!compiled.is_ignored("nicer"));
    }

    #[test]
    fn regex_user_ignore() {
        let user = IgnoredUser {
            login: "^bot_.+".into(),
            is_regex: true,
            case_sensitive: false,
            enabled: true,
        };
        let compiled = CompiledIgnoredUsers::new(&[user]);
        assert!(compiled.is_ignored("bot_spammy"));
        assert!(!compiled.is_ignored("spammy_bot"));
    }

    #[test]
    fn disabled_user_not_matched() {
        let mut u = IgnoredUser::new("someone");
        u.enabled = false;
        let compiled = CompiledIgnoredUsers::new(&[u]);
        assert!(!compiled.is_ignored("someone"));
    }

    #[test]
    fn phrase_block_sets_blocked() {
        let phrases = vec![IgnoredPhrase::new("spam")];
        let compiled = CompiledIgnoredPhrases::new(&phrases);
        let mut text = "this is SPAM".to_owned();
        let out = compiled.apply(&mut text);
        assert!(out.blocked);
    }

    #[test]
    fn phrase_replace_rewrites_text() {
        let mut p = IgnoredPhrase::new("badword");
        p.action = IgnoredPhraseAction::Replace;
        let compiled = CompiledIgnoredPhrases::new(&[p]);
        let mut text = "hello BADWORD world".to_owned();
        let out = compiled.apply(&mut text);
        assert!(out.replaced);
        assert_eq!(text, "hello *** world");
    }

    #[test]
    fn phrase_replace_custom_token() {
        let mut p = IgnoredPhrase::new("foo");
        p.action = IgnoredPhraseAction::Replace;
        p.replace_with = "[redacted]".into();
        let compiled = CompiledIgnoredPhrases::new(&[p]);
        let mut text = "foo bar foo".to_owned();
        compiled.apply(&mut text);
        assert_eq!(text, "[redacted] bar [redacted]");
    }

    #[test]
    fn literal_replacement_preserves_surrounding_unicode() {
        let mut phrase = IgnoredPhrase::new("badword");
        phrase.action = IgnoredPhraseAction::Replace;
        let compiled = CompiledIgnoredPhrases::new(&[phrase]);
        let mut text = "🦀 BADWORD café 世界 badword 🎉".to_owned();
        assert!(compiled.apply(&mut text).replaced);
        assert_eq!(text, "🦀 *** café 世界 *** 🎉");
    }

    #[test]
    fn literal_replacement_matches_unicode_case_without_changing_other_text() {
        for (pattern, text, expected) in [
            ("CAFÉ", "café CAFÉ 🦀", "*** *** 🦀"),
            ("ß", "ẞ ß café", "*** *** café"),
            ("k", "K K k 世界", "*** *** *** 世界"),
        ] {
            let mut phrase = IgnoredPhrase::new(pattern);
            phrase.action = IgnoredPhraseAction::Replace;
            let compiled = CompiledIgnoredPhrases::new(&[phrase]);
            let mut actual = text.to_owned();
            assert!(compiled.apply(&mut actual).replaced, "{pattern}");
            assert_eq!(actual, expected, "{pattern}");
        }
    }

    #[test]
    fn literal_replacement_keeps_metacharacters_and_capture_syntax_literal() {
        let mut phrase = IgnoredPhrase::new("a.b");
        phrase.action = IgnoredPhraseAction::Replace;
        phrase.replace_with = "$1🦀".into();
        let compiled = CompiledIgnoredPhrases::new(&[phrase]);
        let mut text = "axb A.B a.b".to_owned();
        assert!(compiled.apply(&mut text).replaced);
        assert_eq!(text, "axb $1🦀 $1🦀");
    }

    #[test]
    fn literal_replacement_uses_nonoverlapping_matches() {
        let mut phrase = IgnoredPhrase::new("aba");
        phrase.action = IgnoredPhraseAction::Replace;
        phrase.replace_with = "x".into();
        let compiled = CompiledIgnoredPhrases::new(&[phrase]);
        let mut text = "ABABA AbA aba".to_owned();
        assert!(compiled.apply(&mut text).replaced);
        assert_eq!(text, "xBA x x");
    }

    #[test]
    fn case_sensitive_literal_replacement_preserves_unicode_and_case() {
        let mut phrase = IgnoredPhrase::new("café");
        phrase.case_sensitive = true;
        phrase.action = IgnoredPhraseAction::Replace;
        let compiled = CompiledIgnoredPhrases::new(&[phrase]);
        let mut text = "CAFÉ café 🦀".to_owned();
        assert!(compiled.apply(&mut text).replaced);
        assert_eq!(text, "CAFÉ *** 🦀");
    }

    #[test]
    fn empty_disabled_and_unmatched_replacement_rules_leave_text_intact() {
        let mut empty = IgnoredPhrase::new("");
        empty.action = IgnoredPhraseAction::Replace;
        let mut disabled = IgnoredPhrase::new("café");
        disabled.action = IgnoredPhraseAction::Replace;
        disabled.enabled = false;
        let mut unmatched = IgnoredPhrase::new("missing");
        unmatched.action = IgnoredPhraseAction::Replace;
        let compiled = CompiledIgnoredPhrases::new(&[empty, disabled, unmatched]);
        let mut text = "café 🦀".to_owned();
        assert!(!compiled.apply(&mut text).replaced);
        assert_eq!(text, "café 🦀");
    }

    fn replacement(pattern: &str, value: &str, regex: bool) -> IgnoredPhrase {
        let mut phrase = IgnoredPhrase::new(pattern);
        phrase.action = IgnoredPhraseAction::Replace;
        phrase.replace_with = value.into();
        phrase.is_regex = regex;
        phrase
    }

    fn native_emote(id: &str, start: usize, end: usize) -> TwitchEmotePos {
        TwitchEmotePos {
            id: id.into(),
            start,
            end,
        }
    }

    fn emote_ranges(emotes: &[TwitchEmotePos]) -> Vec<(&str, usize, usize)> {
        emotes
            .iter()
            .map(|emote| (emote.id.as_str(), emote.start, emote.end))
            .collect()
    }

    #[test]
    fn replacements_shift_native_emotes_by_characters_and_preserve_unknown_ids() {
        let compiled = CompiledIgnoredPhrases::new(&[
            replacement("bad", "éé", false),
            replacement("fin", "世界", false),
        ]);
        let mut text = "🦀 bad Native bad Other fin".to_owned();
        let mut emotes = vec![
            native_emote("unowned-1", 6, 11),
            native_emote("unowned-2", 17, 21),
        ];
        assert!(compiled.apply_with_emotes(&mut text, &mut emotes).replaced);
        assert_eq!(text, "🦀 éé Native éé Other 世界");
        assert_eq!(
            emote_ranges(&emotes),
            [("unowned-1", 5, 10), ("unowned-2", 15, 19)]
        );

        let spans = crate::format::tokenize_with_flags(&text, false, &emotes, &|_| None);
        let rendered: Vec<_> = spans
            .iter()
            .filter_map(|span| match span {
                crate::model::Span::Emote { id, code, .. } => Some((id.as_str(), code.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, [("unowned-1", "Native"), ("unowned-2", "Other")]);
    }

    #[test]
    fn replacement_drops_only_overlapping_native_emotes() {
        let compiled = CompiledIgnoredPhrases::new(&[replacement("app", "oo", false)]);
        let mut text = "Before Kappa After".to_owned();
        let mut emotes = vec![
            native_emote("before", 0, 5),
            native_emote("overlap", 7, 11),
            native_emote("after", 13, 17),
        ];
        compiled.apply_with_emotes(&mut text, &mut emotes);
        assert_eq!(text, "Before Kooa After");
        assert_eq!(emote_ranges(&emotes), [("before", 0, 5), ("after", 12, 16)]);
    }

    #[test]
    fn sequential_unicode_replacements_keep_emote_offsets_consistent() {
        let compiled = CompiledIgnoredPhrases::new(&[
            replacement("ß", "ss", false),
            replacement("ss", "", false),
        ]);
        let mut text = "ẞ Native".to_owned();
        let mut emotes = vec![native_emote("native", 2, 7)];
        compiled.apply_with_emotes(&mut text, &mut emotes);
        assert_eq!(text, " Native");
        assert_eq!(emote_ranges(&emotes), [("native", 1, 6)]);
    }

    #[test]
    fn regex_capture_replacements_shift_emotes_using_expanded_text() {
        let compiled = CompiledIgnoredPhrases::new(&[replacement(r"[xy](\d+)", "[$1]🦀", true)]);
        let mut text = "x12 Native y345".to_owned();
        let mut emotes = vec![native_emote("native", 4, 9)];
        compiled.apply_with_emotes(&mut text, &mut emotes);
        assert_eq!(text, "[12]🦀 Native [345]🦀");
        assert_eq!(emote_ranges(&emotes), [("native", 6, 11)]);
    }

    #[test]
    fn empty_regex_matches_preserve_boundary_emotes_and_remove_split_codes() {
        let mut text = "Native".to_owned();
        let mut emotes = vec![native_emote("native", 0, 5)];
        CompiledIgnoredPhrases::new(&[replacement("^|$", "🦀", true)])
            .apply_with_emotes(&mut text, &mut emotes);
        assert_eq!(text, "🦀Native🦀");
        assert_eq!(emote_ranges(&emotes), [("native", 1, 6)]);

        let mut text = "Native".to_owned();
        let mut emotes = vec![native_emote("native", 0, 5)];
        CompiledIgnoredPhrases::new(&[replacement("x*", ".", true)])
            .apply_with_emotes(&mut text, &mut emotes);
        assert_eq!(text, ".N.a.t.i.v.e.");
        assert!(emotes.is_empty());
    }

    #[test]
    fn unchanged_replacement_keeps_native_emote_metadata() {
        let compiled = CompiledIgnoredPhrases::new(&[replacement("Native", "Native", false)]);
        let mut text = "Native".to_owned();
        let mut emotes = vec![native_emote("native", 0, 5)];
        assert!(!compiled.apply_with_emotes(&mut text, &mut emotes).replaced);
        assert_eq!(emote_ranges(&emotes), [("native", 0, 5)]);
    }

    #[test]
    fn phrase_regex_replace() {
        let mut p = IgnoredPhrase::new(r"\d+");
        p.is_regex = true;
        p.action = IgnoredPhraseAction::Replace;
        p.replace_with = "#".into();
        let compiled = CompiledIgnoredPhrases::new(&[p]);
        let mut text = "abc 123 def 45".to_owned();
        compiled.apply(&mut text);
        assert_eq!(text, "abc # def #");
    }

    #[test]
    fn phrase_highlight_only_sets_flag() {
        let mut p = IgnoredPhrase::new("hype");
        p.action = IgnoredPhraseAction::HighlightOnly;
        let compiled = CompiledIgnoredPhrases::new(&[p]);
        let mut text = "HYPE train".to_owned();
        let out = compiled.apply(&mut text);
        assert!(out.highlight_only);
        assert_eq!(text, "HYPE train");
    }

    #[test]
    fn phrase_mention_only_sets_flag() {
        let mut p = IgnoredPhrase::new("urgent");
        p.action = IgnoredPhraseAction::MentionOnly;
        let compiled = CompiledIgnoredPhrases::new(&[p]);
        let mut text = "urgent: please ack".to_owned();
        let out = compiled.apply(&mut text);
        assert!(out.mention_only);
    }

    #[test]
    fn invalid_regex_phrase_silently_skipped() {
        let mut p = IgnoredPhrase::new("[unclosed");
        p.is_regex = true;
        assert!(!p.is_regex_valid());
        let compiled = CompiledIgnoredPhrases::new(&[p]);
        assert!(compiled.is_empty());
    }

    #[test]
    fn multiple_phrases_first_block_wins() {
        let phrases = vec![IgnoredPhrase::new("notspam"), IgnoredPhrase::new("spam")];
        let compiled = CompiledIgnoredPhrases::new(&phrases);
        let mut text = "some spam here".to_owned();
        let out = compiled.apply(&mut text);
        assert!(out.blocked);
    }
}
