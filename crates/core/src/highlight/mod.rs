use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::filters::{self as dsl, build_message_context, parse as parse_expression, Expression};
use crate::model::ChatMessage;

/// RGB highlight tint color (red, green, blue).
pub type HighlightColor = [u8; 3];

/// How the `pattern` field should be interpreted.
///
/// `Substring` matches plain text; `Regex` treats `pattern` as a
/// [`regex::Regex`]; `Expression` parses `pattern` as a
/// filter DSL expression that must evaluate to truthy for the highlight to
/// fire (see [`crate::filters`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum HighlightRuleMode {
    #[default]
    Substring,
    Regex,
    Expression,
}

/// A highlight rule with its match pattern and notification settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HighlightRule {
    /// The pattern string (plain substring, regex, or expression - see `mode`).
    pub pattern: String,
    /// Treat `pattern` as a regular expression (legacy flag; kept for
    /// backward compatibility with old config files. When `mode` is unset
    /// and `is_regex` is true we interpret the rule in [`HighlightRuleMode::Regex`]).
    #[serde(default)]
    pub is_regex: bool,
    /// Match is case-sensitive (default: false).
    #[serde(default)]
    pub case_sensitive: bool,
    /// Rule is active; disabled rules are skipped entirely.
    #[serde(default = "bool_true")]
    pub enabled: bool,
    /// When true, trigger an OS/desktop notification when this rule fires.
    #[serde(default)]
    pub show_in_mentions: bool,
    /// Optional RGB tint applied to the matched message background.
    #[serde(default)]
    pub color: Option<HighlightColor>,
    /// Show a visual alert (flash/animation) when this rule fires.
    #[serde(default)]
    pub has_alert: bool,
    /// Play a sound notification when this rule fires.
    #[serde(default)]
    pub has_sound: bool,
    /// Optional custom sound file URL/path for this highlight.
    #[serde(default)]
    pub sound_url: Option<String>,
    /// How `pattern` is interpreted. Absent fields default to
    /// [`HighlightRuleMode::Substring`].
    #[serde(default)]
    pub mode: HighlightRuleMode,
}

fn bool_true() -> bool {
    true
}

impl HighlightRule {
    /// Create a simple case-insensitive substring rule.
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            is_regex: false,
            case_sensitive: false,
            enabled: true,
            show_in_mentions: false,
            color: None,
            has_alert: false,
            has_sound: false,
            sound_url: None,
            mode: HighlightRuleMode::Substring,
        }
    }

    /// Create a regex rule.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            is_regex: true,
            case_sensitive: false,
            enabled: true,
            show_in_mentions: false,
            color: None,
            has_alert: false,
            has_sound: false,
            sound_url: None,
            mode: HighlightRuleMode::Regex,
        }
    }

    /// Create a rule that evaluates a filter expression.
    pub fn expression(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            is_regex: false,
            case_sensitive: false,
            enabled: true,
            show_in_mentions: false,
            color: None,
            has_alert: false,
            has_sound: false,
            sound_url: None,
            mode: HighlightRuleMode::Expression,
        }
    }

    /// Resolve `mode` accounting for legacy config files that only set
    /// `is_regex`.
    pub fn effective_mode(&self) -> HighlightRuleMode {
        match self.mode {
            HighlightRuleMode::Substring if self.is_regex => HighlightRuleMode::Regex,
            _ => self.mode.clone(),
        }
    }

    /// Check if this rule should play any sound notification.
    pub fn should_play_sound(&self) -> bool {
        self.has_sound
    }

    /// Check if this rule has a custom sound configured.
    pub fn has_custom_sound(&self) -> bool {
        self.sound_url.is_some() && !self.sound_url.as_ref().unwrap().is_empty()
    }

    /// Check if this rule should show a visual alert.
    pub fn should_show_alert(&self) -> bool {
        self.has_alert
    }

    /// Returns `true` if this (non-regex) rule matches `text`.
    ///
    /// Regex rules must be evaluated via [`HighlightMatch`]; this path is
    /// kept for simple substring lookups in unit tests and internal helpers.
    pub fn matches_substring(&self, text: &str) -> bool {
        if !self.enabled
            || self.pattern.is_empty()
            || self.effective_mode() != HighlightRuleMode::Substring
        {
            return false;
        }
        if self.case_sensitive {
            text.contains(&self.pattern)
        } else if self.pattern.is_ascii() && text.is_ascii() {
            contains_ascii_case_insensitive(text, &self.pattern)
        } else {
            text.to_lowercase().contains(&self.pattern.to_lowercase())
        }
    }
}

// Compiled match helper

/// Pre-compiled form of a [`HighlightRule`] used for efficient per-message
/// evaluation.  Build once via [`compile_rules`] and reuse across frames.
pub enum CompiledMatcher {
    Substring(String),
    Regex(regex::Regex),
    Expression(Arc<Expression>),
}

pub struct HighlightMatch {
    pub rule: HighlightRule,
    pub matcher: CompiledMatcher,
    context_matcher: ContextMatcher,
}

enum ContextMatcher {
    Text,
    Mention,
    Regex(regex::Regex),
    Scoped {
        scope: ScopedRule,
        value: ScopeMatcher,
        content: Option<CompiledMatcher>,
    },
}

enum ScopeMatcher {
    Exact(String),
    Regex(regex::Regex),
}

impl ScopeMatcher {
    fn is_match(&self, text: &str, case_sensitive: bool) -> bool {
        match self {
            Self::Exact(value) if case_sensitive => text == value,
            Self::Exact(value) if text.is_ascii() && value.is_ascii() => {
                text.eq_ignore_ascii_case(value)
            }
            Self::Exact(value) => text.to_lowercase() == *value,
            Self::Regex(regex) => regex.is_match(text),
        }
    }
}

impl CompiledMatcher {
    fn matches_text(&self, text: &str, case_sensitive: bool) -> bool {
        match self {
            Self::Substring(s) => {
                if case_sensitive {
                    text.contains(s.as_str())
                } else if s.is_ascii() && text.is_ascii() {
                    contains_ascii_case_insensitive(text, s)
                } else {
                    text.to_lowercase().contains(s.as_str())
                }
            }
            Self::Regex(re) => re.is_match(text),
            Self::Expression(_) => false,
        }
    }
}

impl HighlightMatch {
    /// Test whether this match fires for the given message text.
    ///
    /// Expression-mode rules need a full [`ChatMessage`] and return `false`
    /// here. Use [`first_match_context_rule_message`] for those rules.
    pub fn is_match(&self, text: &str) -> bool {
        self.rule.enabled && self.matcher.matches_text(text, self.rule.case_sensitive)
    }

    /// Test this highlight against a full message context.
    ///
    /// Supported rule prefixes (case-insensitive):
    /// - `mention` => fires when `is_mention` is true.
    /// - `from=` / `from:` / `user=` / `user:` => sender match.
    /// - `channel=` / `channel:` => channel match.
    /// - `re:` => regex content match for non-regex rules.
    pub fn is_match_context(
        &self,
        text: &str,
        sender_login: &str,
        sender_display_name: &str,
        channel_login: &str,
        is_mention: bool,
    ) -> bool {
        if !self.rule.enabled {
            return false;
        }

        // Expression-mode rules need a ChatMessage context which isn't
        // available here; defer to the `_message` helpers for those.
        if matches!(self.matcher, CompiledMatcher::Expression(_)) {
            return false;
        }

        match &self.context_matcher {
            ContextMatcher::Text => self.is_match(text),
            ContextMatcher::Mention => is_mention,
            ContextMatcher::Regex(regex) => regex.is_match(text),
            ContextMatcher::Scoped {
                scope,
                value,
                content,
            } => {
                let scope_matches = match scope {
                    ScopedRule::Sender => {
                        value.is_match(sender_login, self.rule.case_sensitive)
                            || value.is_match(sender_display_name, self.rule.case_sensitive)
                    }
                    ScopedRule::Channel => value.is_match(channel_login, self.rule.case_sensitive),
                };
                scope_matches
                    && content
                        .as_ref()
                        .is_none_or(|matcher| matcher.matches_text(text, self.rule.case_sensitive))
            }
        }
    }
}

enum ScopedRule {
    Sender,
    Channel,
}

fn parse_scoped_pattern(pattern: &str) -> Option<(ScopedRule, &str, Option<&str>)> {
    for key in ["from=", "from:", "user=", "user:"] {
        if let Some(rest) = strip_prefix_ascii_case_insensitive(pattern, key) {
            let (value, trailing) = split_first_token(rest.trim());
            return Some((ScopedRule::Sender, value, trailing));
        }
    }
    for key in ["channel=", "channel:"] {
        if let Some(rest) = strip_prefix_ascii_case_insensitive(pattern, key) {
            let (value, trailing) = split_first_token(rest.trim());
            return Some((ScopedRule::Channel, value, trailing));
        }
    }
    None
}

fn strip_prefix_ascii_case_insensitive<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

fn split_first_token(input: &str) -> (&str, Option<&str>) {
    if let Some(space) = input.find(char::is_whitespace) {
        let value = input[..space].trim();
        let rest = input[space..].trim();
        if rest.is_empty() {
            (value, None)
        } else {
            (value, Some(rest))
        }
    } else {
        (input.trim(), None)
    }
}

fn compile_text_matcher(
    pattern: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Option<CompiledMatcher> {
    if pattern.is_empty() {
        return None;
    }

    if is_regex {
        compile_regex(pattern, case_sensitive).map(CompiledMatcher::Regex)
    } else {
        Some(CompiledMatcher::Substring(if case_sensitive {
            pattern.to_owned()
        } else {
            pattern.to_lowercase()
        }))
    }
}

fn compile_context_matcher(rule: &HighlightRule) -> Option<ContextMatcher> {
    if rule.effective_mode() == HighlightRuleMode::Expression {
        return Some(ContextMatcher::Text);
    }
    let pattern = rule.pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    if pattern.eq_ignore_ascii_case("mention") {
        return Some(ContextMatcher::Mention);
    }
    let is_regex = rule.effective_mode() == HighlightRuleMode::Regex;
    if let Some((scope, value, rest)) = parse_scoped_pattern(pattern) {
        if value.is_empty() {
            return None;
        }
        let value = if is_regex && rest.is_none() {
            ScopeMatcher::Regex(compile_regex(value, rule.case_sensitive)?)
        } else {
            ScopeMatcher::Exact(if rule.case_sensitive {
                value.to_owned()
            } else {
                value.to_lowercase()
            })
        };
        let content = match rest {
            Some(pattern) => Some(compile_text_matcher(
                pattern,
                rule.case_sensitive,
                is_regex,
            )?),
            None => None,
        };
        return Some(ContextMatcher::Scoped {
            scope,
            value,
            content,
        });
    }
    if !is_regex {
        if let Some(pattern) = strip_prefix_ascii_case_insensitive(pattern, "re:") {
            return compile_regex(pattern, rule.case_sensitive).map(ContextMatcher::Regex);
        }
    }
    Some(ContextMatcher::Text)
}

fn compile_regex(pattern: &str, case_sensitive: bool) -> Option<regex::Regex> {
    if pattern.is_empty() {
        return None;
    }
    regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .ok()
}

/// Compile a slice of [`HighlightRule`]s into a vec of [`HighlightMatch`]
/// entries, silently skipping any rules with invalid regex or expression
/// patterns (a warning is logged for troubleshooting).
pub fn compile_rules(rules: &[HighlightRule]) -> Vec<HighlightMatch> {
    rules
        .iter()
        .filter(|r| r.enabled && !r.pattern.is_empty())
        .filter_map(|rule| {
            let context_matcher = compile_context_matcher(rule)?;
            let matcher = match rule.effective_mode() {
                HighlightRuleMode::Expression => match parse_expression(&rule.pattern) {
                    Ok(expr) => CompiledMatcher::Expression(Arc::new(expr)),
                    Err(e) => {
                        tracing::warn!(
                            "highlight rule `{}`: invalid expression at {}..{}: {}",
                            rule.pattern,
                            e.span().start,
                            e.span().end,
                            e
                        );
                        return None;
                    }
                },
                mode => compile_text_matcher(
                    &rule.pattern,
                    rule.case_sensitive,
                    mode == HighlightRuleMode::Regex,
                )?,
            };
            Some(HighlightMatch {
                rule: rule.clone(),
                matcher,
                context_matcher,
            })
        })
        .collect()
}

/// Evaluate compiled matchers against `text`.  Returns `Some((color, show_in_mentions, has_alert, has_sound))`
/// for the **first** matching rule, or `None` if nothing matches.
pub fn first_match<'a>(
    compiled: &'a [HighlightMatch],
    text: &str,
) -> Option<(Option<HighlightColor>, bool, bool, bool)> {
    for m in compiled {
        if m.is_match(text) {
            return Some((
                m.rule.color,
                m.rule.show_in_mentions,
                m.rule.has_alert,
                m.rule.has_sound,
            ));
        }
    }
    None
}

/// Evaluate compiled matchers against a full message context.
pub fn first_match_context<'a>(
    compiled: &'a [HighlightMatch],
    text: &str,
    sender_login: &str,
    sender_display_name: &str,
    channel_login: &str,
    is_mention: bool,
) -> Option<(Option<HighlightColor>, bool, bool, bool)> {
    first_match_context_rule(
        compiled,
        text,
        sender_login,
        sender_display_name,
        channel_login,
        is_mention,
    )
    .map(|rule| {
        (
            rule.color,
            rule.show_in_mentions,
            rule.has_alert,
            rule.has_sound,
        )
    })
}

/// Variant of [`first_match_context`] that returns the fully matched
/// [`HighlightRule`] (borrowed) so callers that need the `sound_url`
/// override or other per-rule metadata can read it without walking the
/// compiled list again.
pub fn first_match_context_rule<'a>(
    compiled: &'a [HighlightMatch],
    text: &str,
    sender_login: &str,
    sender_display_name: &str,
    channel_login: &str,
    is_mention: bool,
) -> Option<&'a HighlightRule> {
    compiled
        .iter()
        .find(|m| {
            m.is_match_context(
                text,
                sender_login,
                sender_display_name,
                channel_login,
                is_mention,
            )
        })
        .map(|m| &m.rule)
}

/// Convenience: returns `true` if any compiled rule matches.
pub fn is_highlighted(compiled: &[HighlightMatch], text: &str) -> bool {
    first_match(compiled, text).is_some()
}

/// Legacy slice-of-rules variant kept for backward compat with old call sites.
pub fn is_highlighted_rules(rules: &[HighlightRule], text: &str) -> bool {
    let compiled = compile_rules(rules);
    is_highlighted(&compiled, text)
}

/// Expression-aware first match.
///
/// Iterates all compiled rules. For legacy matchers, falls back to
/// [`HighlightMatch::is_match_context`] using `msg.raw_text`,
/// `msg.sender.*`, `channel_display_name`, and `msg.flags.is_mention`.
/// For expression matchers, builds a [`crate::filters::Context`] once and
/// evaluates each expression against it.
pub fn first_match_context_rule_message<'a>(
    compiled: &'a [HighlightMatch],
    msg: &ChatMessage,
    channel_display_name: &str,
    channel_live: Option<bool>,
    watching: bool,
) -> Option<&'a HighlightRule> {
    if compiled.is_empty() {
        return None;
    }
    let mut expr_ctx: Option<dsl::Context> = None;

    for m in compiled {
        if !m.rule.enabled {
            continue;
        }
        match &m.matcher {
            CompiledMatcher::Expression(expr) => {
                let ctx = expr_ctx.get_or_insert_with(|| {
                    build_message_context(msg, channel_display_name, channel_live, watching)
                });
                if dsl::evaluate(expr, ctx).truthy() {
                    return Some(&m.rule);
                }
            }
            _ => {
                if m.is_match_context(
                    &msg.raw_text,
                    &msg.sender.login,
                    &msg.sender.display_name,
                    channel_display_name,
                    msg.flags.is_mention,
                ) {
                    return Some(&m.rule);
                }
            }
        }
    }
    None
}

/// Tuple-returning sibling of [`first_match_context_rule_message`], matching
/// the shape of [`first_match_context`].
pub fn first_match_context_message(
    compiled: &[HighlightMatch],
    msg: &ChatMessage,
    channel_display_name: &str,
    channel_live: Option<bool>,
    watching: bool,
) -> Option<(Option<HighlightColor>, bool, bool, bool)> {
    first_match_context_rule_message(compiled, msg, channel_display_name, channel_live, watching)
        .map(|rule| {
            (
                rule.color,
                rule.show_in_mentions,
                rule.has_alert,
                rule.has_sound,
            )
        })
}

// ASCII case-insensitive search

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
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
            if !h[start + i].eq_ignore_ascii_case(&n[i]) {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rule(pattern: &str) -> HighlightRule {
        HighlightRule::new(pattern)
    }

    // substring matching

    #[test]
    fn case_insensitive_match() {
        let rules = vec![make_rule("hello")];
        let compiled = compile_rules(&rules);
        assert!(is_highlighted(&compiled, "HeLLo world"));
    }

    #[test]
    fn no_match() {
        let rules = vec![make_rule("goodbye")];
        let compiled = compile_rules(&rules);
        assert!(!is_highlighted(&compiled, "hello world"));
    }

    // regex matching

    #[test]
    fn regex_rule_matches() {
        let rule = HighlightRule::regex("hell.+");
        let compiled = compile_rules(&[rule]);
        assert!(is_highlighted(&compiled, "hello world"));
    }

    #[test]
    fn regex_rule_no_match() {
        let rule = HighlightRule::regex("^world$");
        let compiled = compile_rules(&[rule]);
        assert!(!is_highlighted(&compiled, "hello world"));
    }

    #[test]
    fn invalid_regex_silently_skipped() {
        let mut rule = HighlightRule::regex("[unclosed");
        rule.enabled = true;
        let compiled = compile_rules(&[rule]);
        // Invalid regex -> compiled to 0 entries; no panic
        assert_eq!(compiled.len(), 0);
    }

    // disabled rules

    #[test]
    fn disabled_rule_skipped() {
        let mut rule = make_rule("hello");
        rule.enabled = false;
        let compiled = compile_rules(&[rule]);
        // disabled rule -> not included in compiled set
        assert_eq!(compiled.len(), 0);
    }

    // color propagation

    #[test]
    fn color_returned_on_match() {
        let mut rule = make_rule("ping");
        rule.color = Some([255, 80, 80]);
        let compiled = compile_rules(&[rule]);
        let result = first_match(&compiled, "ping me please");
        assert_eq!(result, Some((Some([255, 80, 80]), false, false, false)));
    }

    #[test]
    fn show_in_mentions_propagated() {
        let mut rule = make_rule("@me");
        rule.show_in_mentions = true;
        let compiled = compile_rules(&[rule]);
        let result = first_match(&compiled, "hey @me");
        assert_eq!(result, Some((None, true, false, false)));
    }

    // case sensitivity

    #[test]
    fn case_sensitive_no_match() {
        let mut rule = make_rule("Hello");
        rule.case_sensitive = true;
        let compiled = compile_rules(&[rule]);
        assert!(!is_highlighted(&compiled, "hello world"));
    }

    #[test]
    fn case_sensitive_match() {
        let mut rule = make_rule("Hello");
        rule.case_sensitive = true;
        let compiled = compile_rules(&[rule]);
        assert!(is_highlighted(&compiled, "Hello world"));
    }

    #[test]
    fn context_mention_rule_matches() {
        let rule = make_rule("mention");
        let compiled = compile_rules(&[rule]);
        let matched = first_match_context(&compiled, "hello", "alice", "Alice", "chan", true);
        assert!(matched.is_some());
    }

    #[test]
    fn context_sender_rule_matches_login() {
        let rule = make_rule("from=alice");
        let compiled = compile_rules(&[rule]);
        let matched = first_match_context(&compiled, "hello", "alice", "Alice", "chan", false);
        assert!(matched.is_some());
    }

    #[test]
    fn context_channel_and_content_rule_matches() {
        let rule = make_rule("channel=somechan hello");
        let compiled = compile_rules(&[rule]);
        let matched =
            first_match_context(&compiled, "say hello", "alice", "Alice", "somechan", false);
        assert!(matched.is_some());
    }

    #[test]
    fn context_re_prefix_works_for_non_regex_rule() {
        let rule = make_rule("re:hel+o");
        let compiled = compile_rules(&[rule]);
        let matched = first_match_context(
            &compiled,
            "hello there",
            "alice",
            "Alice",
            "somechan",
            false,
        );
        assert!(matched.is_some());
    }

    #[test]
    fn scoped_regex_rules_use_explicit_and_legacy_modes_consistently() {
        for legacy in [false, true] {
            let mut rule = make_rule(r"from:alice ^hello\s+\d+$");
            if legacy {
                rule.is_regex = true;
            } else {
                rule.mode = HighlightRuleMode::Regex;
            }
            let compiled = compile_rules(&[rule]);
            assert_eq!(compiled.len(), 1);
            let matcher = &compiled[0];
            assert!(matcher.is_match_context("HELLO 42", "alice", "Alice", "room", false));
            assert!(!matcher.is_match_context("hello 42", "bob", "Bob", "room", false));
            assert!(!matcher.is_match_context("hello world", "alice", "Alice", "room", false));
        }
    }

    #[test]
    fn terminal_scope_regex_matches_the_sender_or_channel() {
        for pattern in [r"from:^ali.e$", r"user=^ali.e$"] {
            let compiled = compile_rules(&[HighlightRule::regex(pattern)]);
            assert!(compiled[0].is_match_context("irrelevant", "alice", "Alice", "room", false));
            assert!(!compiled[0].is_match_context("alice", "bob", "Bob", "room", false));
        }
        let compiled = compile_rules(&[HighlightRule::regex(r"channel=^room\d+$")]);
        assert!(compiled[0].is_match_context("hello", "alice", "Alice", "room42", false));
        assert!(!compiled[0].is_match_context("room42", "alice", "Alice", "other", false));
    }

    #[test]
    fn scope_keywords_ignore_case_but_values_and_content_follow_the_rule() {
        for prefix in ["FROM=", "From:", "USER=", "User:"] {
            let mut rule = make_rule(&format!("{prefix}Alice Hello"));
            rule.case_sensitive = true;
            let compiled = compile_rules(&[rule]);
            let matcher = &compiled[0];
            assert!(matcher.is_match_context("Hello", "alice", "Alice", "room", false));
            assert!(!matcher.is_match_context("hello", "alice", "Alice", "room", false));
            assert!(!matcher.is_match_context("Hello", "alice", "ALICE", "room", false));
        }
        let compiled = compile_rules(&[make_rule("CHANNEL:Room hello")]);
        assert!(compiled[0].is_match_context("HELLO", "alice", "Alice", "room", false));
        let compiled = compile_rules(&[make_rule("RE:^hello$")]);
        assert!(compiled[0].is_match_context("HELLO", "alice", "Alice", "room", false));
    }

    #[test]
    fn scoped_plain_content_stays_literal_and_unicode_names_fold_once() {
        let compiled = compile_rules(&[make_rule("from:ÉLODIE a+b")]);
        assert!(compiled[0].is_match_context("A+B", "elodie", "Élodie", "room", false));
        assert!(!compiled[0].is_match_context("aaab", "elodie", "Élodie", "room", false));
        let compiled = compile_rules(&[make_rule("ÉLODIE")]);
        assert!(compiled[0].is_match("bonjour Élodie"));
    }

    #[test]
    fn invalid_context_rules_are_rejected_during_compilation() {
        for rule in [
            make_rule("re:["),
            make_rule("re:"),
            make_rule("from:"),
            make_rule("   "),
            HighlightRule::regex("from:alice ["),
        ] {
            assert!(compile_rules(&[rule]).is_empty());
        }
    }

    #[test]
    #[ignore = "isolated scoped highlight benchmark"]
    fn perf_scoped_highlight_matching() {
        use std::hint::black_box;
        use std::time::Instant;

        let pattern = r"^hello\s+\d+$";
        let compiled = compile_rules(&[HighlightRule::regex(format!("from:alice {pattern}"))]);
        let iterations = 10_000;
        let mut previous = Vec::new();
        let mut revised = Vec::new();
        for round in 0..5 {
            for old_path in if round % 2 == 0 {
                [true, false]
            } else {
                [false, true]
            } {
                let started = Instant::now();
                let mut matches = 0;
                for index in 0..iterations {
                    let text = black_box(if index % 2 == 0 {
                        "hello 42"
                    } else {
                        "hello world"
                    });
                    let matched = if old_path {
                        regex::RegexBuilder::new(black_box(pattern))
                            .case_insensitive(true)
                            .build()
                            .unwrap()
                            .is_match(text)
                    } else {
                        compiled[0].is_match_context(text, "alice", "Alice", "room", false)
                    };
                    matches += usize::from(matched);
                }
                assert_eq!(matches, iterations / 2);
                if old_path {
                    previous.push(started.elapsed());
                } else {
                    revised.push(started.elapsed());
                }
            }
        }
        previous.sort_unstable();
        revised.sort_unstable();
        eprintln!(
            "{iterations} scoped highlight checks: per-message compilation {:?}; compiled matching {:?}",
            previous[previous.len() / 2],
            revised[revised.len() / 2]
        );
    }

    // expression mode

    fn fake_subbed_message(text: &str) -> crate::model::ChatMessage {
        use crate::model::{
            Badge, ChannelId, ChatMessage, MessageId, MsgKind, Sender, Span as MsgSpan, UserId,
        };
        use chrono::Utc;
        use smallvec::smallvec;
        ChatMessage {
            id: MessageId(1),
            server_id: None,
            timestamp: Utc::now(),
            channel: ChannelId("somechan".into()),
            sender: Sender {
                original_display_name: None,
                user_id: UserId("1".into()),
                login: "alice".into(),
                display_name: "Alice".into(),
                color: None,
                name_paint: None,
                badges: vec![Badge {
                    name: "subscriber".into(),
                    version: "3".into(),
                    url: None,
                }],
            },
            raw_text: text.into(),
            spans: smallvec![MsgSpan::Text {
                text: text.into(),
                is_action: false
            }],
            twitch_emotes: Vec::new(),
            flags: Default::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn expression_mode_highlight_fires_for_subscriber_gg() {
        let mut rule =
            HighlightRule::expression("author.subscriber && message.content contains \"gg\"");
        rule.color = Some([255, 200, 100]);
        let compiled = compile_rules(&[rule]);
        let msg = fake_subbed_message("gg ez");
        let hit = first_match_context_rule_message(&compiled, &msg, "somechan", None, false);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().color, Some([255, 200, 100]));
    }

    #[test]
    fn expression_mode_highlight_skips_non_subscribers() {
        let rule =
            HighlightRule::expression("author.subscriber && message.content contains \"gg\"");
        let compiled = compile_rules(&[rule]);
        let mut msg = fake_subbed_message("gg ez");
        msg.sender.badges.clear();
        let hit = first_match_context_rule_message(&compiled, &msg, "somechan", None, false);
        assert!(hit.is_none());
    }

    #[test]
    fn invalid_expression_rule_dropped_on_compile() {
        let rule = HighlightRule::expression("author.subscriber &&");
        let compiled = compile_rules(&[rule]);
        assert!(compiled.is_empty());
    }
}
