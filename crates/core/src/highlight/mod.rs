/// A single highlight rule.
#[derive(Debug, Clone)]
pub struct HighlightRule {
    pub pattern: String,
    pub case_sensitive: bool,
}

impl HighlightRule {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            case_sensitive: false,
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        if self.case_sensitive {
            text.contains(&self.pattern)
        } else {
            text.to_lowercase().contains(&self.pattern.to_lowercase())
        }
    }
}

/// Returns true if any rule matches the message text.
pub fn is_highlighted(rules: &[HighlightRule], text: &str) -> bool {
    rules.iter().any(|r| r.matches(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_match() {
        let rules = vec![HighlightRule::new("hello")];
        assert!(is_highlighted(&rules, "HeLLo world"));
    }

    #[test]
    fn no_match() {
        let rules = vec![HighlightRule::new("goodbye")];
        assert!(!is_highlighted(&rules, "hello world"));
    }
}
