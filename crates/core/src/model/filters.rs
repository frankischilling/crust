use crate::model::ChannelId;
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
    /// Filter username/sender rather than message content.
    #[serde(default)]
    pub filter_sender: bool,
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
        }
    }

    /// Test whether this filter matches a given text string.
    pub fn matches_text(&self, text: &str) -> bool {
        if !self.enabled || self.pattern.is_empty() {
            return false;
        }

        if self.is_regex {
            // Regex path: compile on-the-fly for testing.
            let mut builder = RegexBuilder::new(&self.pattern);
            builder.case_insensitive(!self.case_sensitive);
            if let Ok(re) = builder.build() {
                return re.is_match(text);
            }
            false
        } else {
            // Substring path
            if self.case_sensitive {
                text.contains(&self.pattern)
            } else {
                text.to_lowercase().contains(&self.pattern.to_lowercase())
            }
        }
    }
}

/// The runtime-compiled version of a `FilterRecord` ready for fast matching.
#[derive(Clone)]
pub struct CompiledFilter {
    pub name: String,
    pub pattern: regex::Regex,
    pub scope: FilterScope,
    pub action: FilterAction,
    pub filter_sender: bool,
}

pub fn compile_filters(records: &[FilterRecord]) -> Vec<CompiledFilter> {
    records
        .iter()
        .filter(|r| r.enabled && !r.pattern.is_empty())
        .filter_map(|r| {
            let pattern = if r.is_regex {
                r.pattern.clone()
            } else {
                regex::escape(&r.pattern)
            };

            let mut builder = regex::RegexBuilder::new(&pattern);
            builder.case_insensitive(!r.case_sensitive);

            match builder.build() {
                Ok(re) => Some(CompiledFilter {
                    name: r.name.clone(),
                    pattern: re,
                    scope: r.scope.clone(),
                    action: r.action.clone(),
                    filter_sender: r.filter_sender,
                }),
                Err(_) => None,
            }
        })
        .collect()
}

/// Check if a message should be filtered. Returns `Some(action)` if filtered, `None` otherwise.
pub fn check_filters(
    compiled: &[CompiledFilter],
    channel_id: Option<&ChannelId>,
    message_text: &str,
    sender_name: &str,
) -> Option<FilterAction> {
    for filter in compiled {
        // Check scope
        let scope_matches = match &filter.scope {
            FilterScope::Global => true,
            FilterScope::Channel(ch_id) => channel_id == Some(ch_id),
        };
        if !scope_matches {
            continue;
        }

        // Check pattern
        let text = if filter.filter_sender {
            sender_name
        } else {
            message_text
        };

        if filter.pattern.is_match(text) {
            return Some(filter.action.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_substring_case_insensitive() {
        let mut filter = FilterRecord::new("test", "SPAM", FilterScope::Global);
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
}
