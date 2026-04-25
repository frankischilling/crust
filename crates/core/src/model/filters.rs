use std::sync::Arc;

use crate::filters::{self as dsl, build_message_context, parse, Expression};
use crate::model::{ChannelId, ChatMessage};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilterScope {
    Global,
    Channel(ChannelId),
}

impl Default for FilterScope {
    fn default() -> Self {
        Self::Global
    }
}

/// Filter action to take when a message matches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilterAction {
    /// Hide the message completely.
    Hide,
    /// Show the message but dim/gray it out.
    Dim,
}

impl Default for FilterAction {
    fn default() -> Self {
        Self::Hide
    }
}

/// How the `pattern` field should be interpreted at compile time.
///
/// `Substring` is the historical default (plain text substring). `Regex`
/// treats `pattern` as a [`regex::Regex`]. `Expression` treats `pattern` as
/// a Chatterino-style filter DSL expression (see [`crate::filters`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum FilterMode {
    #[default]
    Substring,
    Regex,
    Expression,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct FilterRecord {
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub is_regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "bool_true")]
    pub enabled: bool,
    #[serde(default)]
    pub scope: FilterScope,
    /// Action to take when this filter matches (hide vs dim).
    #[serde(default)]
    pub action: FilterAction,
    /// Filter username/sender rather than message content (legacy mode only).
    #[serde(default)]
    pub filter_sender: bool,
    /// How `pattern` is interpreted.
    ///
    /// When absent from persisted data, falls back to [`FilterMode::Substring`]
    /// (or `Regex` if the legacy `is_regex` field was `true`, handled at
    /// compile time).
    #[serde(default)]
    pub mode: FilterMode,
}

fn bool_true() -> bool {
    true
}

impl FilterRecord {
    pub fn new(name: impl Into<String>, pattern: impl Into<String>, scope: FilterScope) -> Self {
        Self {
            name: name.into(),
            pattern: pattern.into(),
            is_regex: false,
            case_sensitive: false,
            enabled: true,
            scope,
            action: FilterAction::Hide,
            filter_sender: false,
            mode: FilterMode::Substring,
        }
    }

    /// Effective mode after legacy `is_regex` migration.
    pub fn effective_mode(&self) -> FilterMode {
        match self.mode {
            FilterMode::Substring if self.is_regex => FilterMode::Regex,
            _ => self.mode.clone(),
        }
    }

    /// Test whether this filter matches a given text string (legacy modes only).
    ///
    /// `Expression` mode always returns `false` here; use
    /// [`check_filters_message`] for expression filters.
    pub fn matches_text(&self, text: &str) -> bool {
        if !self.enabled || self.pattern.is_empty() {
            return false;
        }

        match self.effective_mode() {
            FilterMode::Expression => false,
            FilterMode::Regex => {
                let mut builder = RegexBuilder::new(&self.pattern);
                builder.case_insensitive(!self.case_sensitive);
                if let Ok(re) = builder.build() {
                    return re.is_match(text);
                }
                false
            }
            FilterMode::Substring => {
                if self.case_sensitive {
                    text.contains(&self.pattern)
                } else {
                    text.to_lowercase().contains(&self.pattern.to_lowercase())
                }
            }
        }
    }
}

/// The runtime-compiled variant of a [`FilterRecord`].
#[derive(Clone)]
pub struct CompiledFilter {
    pub name: String,
    pub scope: FilterScope,
    pub action: FilterAction,
    pub kind: CompiledFilterKind,
}

/// Compiled filter body: either a legacy regex match or a DSL expression.
#[derive(Clone)]
pub enum CompiledFilterKind {
    Legacy {
        pattern: regex::Regex,
        filter_sender: bool,
    },
    Expression(Arc<Expression>),
}

/// Compile a slice of [`FilterRecord`]s into their runtime form.
///
/// Rules with invalid regex/expression are silently skipped and logged.
pub fn compile_filters(records: &[FilterRecord]) -> Vec<CompiledFilter> {
    records
        .iter()
        .filter(|r| r.enabled && !r.pattern.is_empty())
        .filter_map(|r| match r.effective_mode() {
            FilterMode::Expression => match parse(&r.pattern) {
                Ok(expr) => Some(CompiledFilter {
                    name: r.name.clone(),
                    scope: r.scope.clone(),
                    action: r.action.clone(),
                    kind: CompiledFilterKind::Expression(Arc::new(expr)),
                }),
                Err(e) => {
                    tracing::warn!(
                        "filter `{}`: invalid expression at {}..{}: {}",
                        r.name,
                        e.span().start,
                        e.span().end,
                        e
                    );
                    None
                }
            },
            mode => {
                let pattern = if matches!(mode, FilterMode::Regex) {
                    r.pattern.clone()
                } else {
                    regex::escape(&r.pattern)
                };
                let mut builder = RegexBuilder::new(&pattern);
                builder.case_insensitive(!r.case_sensitive);
                match builder.build() {
                    Ok(re) => Some(CompiledFilter {
                        name: r.name.clone(),
                        scope: r.scope.clone(),
                        action: r.action.clone(),
                        kind: CompiledFilterKind::Legacy {
                            pattern: re,
                            filter_sender: r.filter_sender,
                        },
                    }),
                    Err(e) => {
                        tracing::warn!("filter `{}`: invalid regex: {}", r.name, e);
                        None
                    }
                }
            }
        })
        .collect()
}

/// Legacy `check_filters` entry point preserved for callers that don't yet
/// have a full [`ChatMessage`] available. Expression filters are skipped.
pub fn check_filters(
    compiled: &[CompiledFilter],
    channel_id: Option<&ChannelId>,
    message_text: &str,
    sender_name: &str,
) -> Option<FilterAction> {
    for filter in compiled {
        if !scope_matches(&filter.scope, channel_id) {
            continue;
        }
        match &filter.kind {
            CompiledFilterKind::Legacy {
                pattern,
                filter_sender,
            } => {
                let hay = if *filter_sender {
                    sender_name
                } else {
                    message_text
                };
                if pattern.is_match(hay) {
                    return Some(filter.action.clone());
                }
            }
            CompiledFilterKind::Expression(_) => continue,
        }
    }
    None
}

/// Preferred entry point for runtime filtering.
///
/// Builds a [`crate::filters::Context`] once from `msg` and evaluates every
/// active filter (legacy or expression) against it, returning the first
/// matching [`FilterAction`].
pub fn check_filters_message(
    compiled: &[CompiledFilter],
    channel_id: Option<&ChannelId>,
    msg: &ChatMessage,
    channel_display_name: &str,
    channel_live: Option<bool>,
    watching: bool,
) -> Option<FilterAction> {
    if compiled.is_empty() {
        return None;
    }

    // Lazily build the expression context only if needed.
    let mut expr_ctx: Option<dsl::Context> = None;

    for filter in compiled {
        if !scope_matches(&filter.scope, channel_id) {
            continue;
        }
        match &filter.kind {
            CompiledFilterKind::Legacy {
                pattern,
                filter_sender,
            } => {
                let hay = if *filter_sender {
                    msg.sender.login.as_str()
                } else {
                    msg.raw_text.as_str()
                };
                if pattern.is_match(hay) {
                    return Some(filter.action.clone());
                }
            }
            CompiledFilterKind::Expression(expr) => {
                let ctx = expr_ctx.get_or_insert_with(|| {
                    build_message_context(msg, channel_display_name, channel_live, watching)
                });
                let v = dsl::evaluate(expr, ctx);
                if v.truthy() {
                    return Some(filter.action.clone());
                }
            }
        }
    }
    None
}

fn scope_matches(scope: &FilterScope, channel_id: Option<&ChannelId>) -> bool {
    match scope {
        FilterScope::Global => true,
        FilterScope::Channel(ch_id) => channel_id == Some(ch_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use smallvec::smallvec;

    use crate::model::{Badge, MessageId, MsgKind, Sender, Span, UserId};

    #[test]
    fn filter_matches_substring_case_insensitive() {
        let filter = FilterRecord::new("test", "SPAM", FilterScope::Global);
        assert!(filter.matches_text("this is spam text"));
    }

    #[test]
    fn filter_matches_regex() {
        let mut filter = FilterRecord::new("test", r"\d{3,}", FilterScope::Global);
        filter.is_regex = true;
        assert!(filter.matches_text("code: 12345"));
        assert!(!filter.matches_text("code: 12"));
    }

    #[test]
    fn filter_respects_case_sensitive() {
        let mut filter = FilterRecord::new("test", "Spam", FilterScope::Global);
        filter.case_sensitive = true;
        assert!(filter.matches_text("Spam"));
        assert!(!filter.matches_text("spam"));
    }

    #[test]
    fn check_filters_returns_hide_action() {
        let records = vec![FilterRecord::new("test", "spam", FilterScope::Global)];
        let compiled = compile_filters(&records);
        let result = check_filters(&compiled, None, "this is spam", "testuser");
        assert_eq!(result, Some(FilterAction::Hide));
    }

    #[test]
    fn check_filters_respects_channel_scope() {
        let channel_a = ChannelId("123".to_owned());
        let channel_b = ChannelId("456".to_owned());

        let records = vec![FilterRecord::new(
            "test",
            "banned",
            FilterScope::Channel(channel_a.clone()),
        )];
        let compiled = compile_filters(&records);

        assert!(check_filters(&compiled, Some(&channel_a), "banned word", "user").is_some());
        assert!(check_filters(&compiled, Some(&channel_b), "banned word", "user").is_none());
    }

    #[test]
    fn filter_sender_checks_username_instead_of_message() {
        let mut filter = FilterRecord::new("block_user", "trolluser", FilterScope::Global);
        filter.filter_sender = true;

        let records = vec![filter];
        let compiled = compile_filters(&records);

        let result = check_filters(&compiled, None, "innocent message", "trolluser");
        assert_eq!(result, Some(FilterAction::Hide));

        let result2 = check_filters(&compiled, None, "message from trolluser", "gooduser");
        assert_eq!(result2, None);
    }

    fn make_subbed_message(text: &str) -> ChatMessage {
        ChatMessage {
            id: MessageId(7),
            server_id: None,
            timestamp: Utc::now(),
            channel: ChannelId("somech".into()),
            sender: Sender {
                user_id: UserId("99".into()),
                login: "alice".into(),
                display_name: "Alice".into(),
                color: None,
                name_paint: None,
                badges: vec![Badge {
                    name: "subscriber".into(),
                    version: "6".into(),
                    url: None,
                }],
            },
            raw_text: text.to_string(),
            spans: smallvec![Span::Text {
                text: text.to_string(),
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
    fn expression_mode_filter_hides_subscriber_gg() {
        let mut rec = FilterRecord::new(
            "sub_gg",
            "author.subscriber && message.content contains \"gg\"",
            FilterScope::Global,
        );
        rec.mode = FilterMode::Expression;
        rec.action = FilterAction::Hide;
        let compiled = compile_filters(&[rec]);
        let msg = make_subbed_message("gg ez");
        let r = check_filters_message(&compiled, None, &msg, "somech", Some(true), false);
        assert_eq!(r, Some(FilterAction::Hide));
    }

    #[test]
    fn expression_mode_respects_per_channel_scope() {
        let ch_a = ChannelId("a".into());
        let ch_b = ChannelId("b".into());
        let mut rec = FilterRecord::new(
            "subgg_b",
            "author.subscriber && message.content contains \"gg\"",
            FilterScope::Channel(ch_b.clone()),
        );
        rec.mode = FilterMode::Expression;
        let compiled = compile_filters(&[rec]);
        let msg = make_subbed_message("gg ez");
        assert!(check_filters_message(&compiled, Some(&ch_a), &msg, "a", None, false).is_none());
        assert!(check_filters_message(&compiled, Some(&ch_b), &msg, "b", None, false).is_some());
    }

    #[test]
    fn invalid_expression_silently_dropped_on_compile() {
        let mut rec = FilterRecord::new("bad", "author.subscriber &&", FilterScope::Global);
        rec.mode = FilterMode::Expression;
        let compiled = compile_filters(&[rec]);
        assert!(compiled.is_empty());
    }

    #[test]
    fn legacy_filters_still_run_via_message_entry_point() {
        let rec = FilterRecord::new("nope", "spam", FilterScope::Global);
        let compiled = compile_filters(&[rec]);
        let msg = make_subbed_message("this is spam");
        assert!(
            check_filters_message(&compiled, None, &msg, "somech", None, false).is_some()
        );
    }
}
