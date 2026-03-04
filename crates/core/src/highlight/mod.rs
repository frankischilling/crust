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
        } else if self.pattern.is_ascii() && text.is_ascii() {
            contains_ascii_case_insensitive(text, &self.pattern)
        } else {
            text.to_lowercase().contains(&self.pattern.to_lowercase())
        }
    }
}

/// Returns true if any rule matches the message text.
pub fn is_highlighted(rules: &[HighlightRule], text: &str) -> bool {
    let mut lowered_text: Option<String> = None;

    for rule in rules {
        if rule.case_sensitive {
            if text.contains(&rule.pattern) {
                return true;
            }
            continue;
        }

        if rule.pattern.is_ascii() && text.is_ascii() {
            if contains_ascii_case_insensitive(text, &rule.pattern) {
                return true;
            }
            continue;
        }

        let lower = lowered_text.get_or_insert_with(|| text.to_lowercase());
        if lower.contains(&rule.pattern.to_lowercase()) {
            return true;
        }
    }

    false
}

/// ASCII-only case-insensitive substring search without allocations.
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

    for start in 0..=last_start {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }

        let mut all_match = true;
        for i in 1..n.len() {
            if h[start + i].to_ascii_lowercase() != n[i].to_ascii_lowercase() {
                all_match = false;
                break;
            }
        }

        if all_match {
            return true;
        }
    }

    false
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
