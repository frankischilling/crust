//! Ignored users + ignored phrases.
//!
//! Mirrors the chatterino split between [`IgnoredUser`] (full-user block list)
//! and [`IgnoredPhrase`] (per-message text pattern with an action).  The UI
//! manages both in a single settings tab.

use serde::{Deserialize, Serialize};

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

// -- Compiled matchers --------------------------------------------------------

enum UserMatcher {
    Exact { needle: String, case_sensitive: bool },
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
        for u in users.iter().filter(|u| u.enabled && !u.login.trim().is_empty()) {
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

// -- Phrase matching ----------------------------------------------------------

enum PhraseMatcher {
    Substring {
        needle: String,
        case_sensitive: bool,
    },
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
            } else {
                PhraseMatcher::Substring {
                    needle: p.pattern.clone(),
                    case_sensitive: p.case_sensitive,
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
        let mut out = PhraseApplyOutcome::default();
        for p in &self.phrases {
            let matched = match &p.matcher {
                PhraseMatcher::Substring {
                    needle,
                    case_sensitive,
                } => {
                    if *case_sensitive {
                        text.contains(needle.as_str())
                    } else if needle.is_ascii() && text.is_ascii() {
                        ascii_contains_insensitive(text, needle)
                    } else {
                        text.to_lowercase().contains(&needle.to_lowercase())
                    }
                }
                PhraseMatcher::Regex(re) => re.is_match(text),
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
                    let replaced = match &p.matcher {
                        PhraseMatcher::Substring {
                            needle,
                            case_sensitive,
                        } => {
                            if *case_sensitive {
                                text.replace(needle.as_str(), &p.replace_with)
                            } else {
                                replace_ascii_insensitive(text, needle, &p.replace_with)
                            }
                        }
                        PhraseMatcher::Regex(re) => {
                            re.replace_all(text, p.replace_with.as_str()).into_owned()
                        }
                    };
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

fn ascii_contains_insensitive(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    let first = n[0].to_ascii_lowercase();
    let last_start = h.len() - n.len();
    'outer: for start in 0..=last_start {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }
        for i in 1..n.len() {
            if h[start + i].to_ascii_lowercase() != n[i].to_ascii_lowercase() {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

fn replace_ascii_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_owned();
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < h.len() {
        if i + n.len() <= h.len()
            && h[i..i + n.len()]
                .iter()
                .zip(n.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            out.push_str(replacement);
            i += n.len();
        } else {
            out.push(h[i] as char);
            i += 1;
        }
    }
    out
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
