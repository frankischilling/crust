use once_cell::sync::Lazy;
use regex::Regex;

#[allow(unused_imports)]
use super::predicate::{FlagKind, Predicate};

/// Outcome of parsing a query string.
///
/// Parsing is infallible for the whole input: unknown tags fall back to
/// substring matching, and invalid regex values are dropped but recorded in
/// `regex_error` so the UI can surface them.
#[derive(Debug, Default)]
pub struct ParseOutcome {
    pub predicates: Vec<Predicate>,
    pub regex_error: Option<String>,
}

static TOKEN_RE: Lazy<Regex> = Lazy::new(|| {
    // Mirrors chatterino's SearchPopup::parsePredicates regex. Captures an
    // optional negation, then either a `name:value` tag (value may be quoted)
    // OR a bare non-whitespace run used as a substring.
    Regex::new(r#"(?P<neg>[!\-])?(?:(?P<name>\w+):(?P<value>"[^"]*"|\S+))|\S+"#).unwrap()
});

pub fn parse(input: &str) -> ParseOutcome {
    let mut outcome = ParseOutcome::default();
    let input = input.trim();
    if input.is_empty() {
        return outcome;
    }

    for cap in TOKEN_RE.captures_iter(input) {
        let negated = cap.name("neg").is_some();
        let name = cap.name("name").map(|m| m.as_str());
        let value_raw = cap.name("value").map(|m| m.as_str());

        let predicate = match (name, value_raw) {
            (Some(tag), Some(val)) => {
                let val = strip_quotes(val);
                match dispatch_tag(tag, val) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        // Unknown tagwhole match falls back to substring.
                        let raw = cap.get(0).unwrap().as_str();
                        Predicate::Substring(raw.to_string())
                    }
                    Err(regex_err) => {
                        outcome.regex_error = Some(regex_err);
                        continue;
                    }
                }
            }
            _ => {
                let text = cap.get(0).unwrap().as_str();
                let text = text.trim_start_matches(['!', '-']);
                if text.is_empty() {
                    continue;
                }
                Predicate::Substring(text.to_string())
            }
        };

        let predicate = if negated && name.is_some() {
            Predicate::Negated(Box::new(predicate))
        } else {
            predicate
        };
        outcome.predicates.push(predicate);
    }

    outcome
}

fn strip_quotes(v: &str) -> &str {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// Dispatches a single `name:value` tag.
///
/// - `Ok(Some(p))`tag recognised.
/// - `Ok(None)`   unknown tag (caller falls back to substring).
/// - `Err(msg)`   recognised tag but value invalid (e.g. bad regex).
fn dispatch_tag(name: &str, value: &str) -> Result<Option<Predicate>, String> {
    let name = name.to_lowercase();
    match name.as_str() {
        "from" => Ok(Some(Predicate::Author(split_csv_lower(value)))),
        "in" => Ok(Some(Predicate::Channel(split_csv_lower(value)))),
        "has" => match value.to_lowercase().as_str() {
            "link" => Ok(Some(Predicate::Link)),
            "mention" => Ok(Some(Predicate::Mention)),
            "emote" => Ok(Some(Predicate::Emote)),
            _ => Ok(None),
        },
        "is" => Ok(flag_kind(value).map(Predicate::Flag)),
        "regex" => match Regex::new(value) {
            Ok(re) => Ok(Some(Predicate::Regex(re))),
            Err(e) => Err(format!("regex error: {e}")),
        },
        "badge" => Ok(Some(Predicate::Badge(split_badges(value)))),
        "subtier" => Ok(Some(Predicate::Subtier(split_subtiers(value)))),
        _ => Ok(None),
    }
}

fn split_csv_lower(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn split_badges(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.to_lowercase().as_str() {
            "mod" => "moderator".to_string(),
            "sub" => "subscriber".to_string(),
            "prime" => "premium".to_string(),
            _ => s.to_lowercase(),
        })
        .collect()
}

fn split_subtiers(v: &str) -> Vec<char> {
    v.split(',')
        .map(str::trim)
        .filter_map(|s| s.chars().next())
        .collect()
}

fn flag_kind(v: &str) -> Option<FlagKind> {
    match v.to_lowercase().as_str() {
        "highlighted" | "highlight" => Some(FlagKind::Highlighted),
        "sub" | "subscription" => Some(FlagKind::Sub),
        "reply" => Some(FlagKind::Reply),
        "action" | "me" => Some(FlagKind::Action),
        "first" | "firstmsg" => Some(FlagKind::FirstMsg),
        "pinned" | "pin" => Some(FlagKind::Pinned),
        "deleted" => Some(FlagKind::Deleted),
        "self" => Some(FlagKind::SelfMsg),
        "system" => Some(FlagKind::System),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_produces_no_predicates() {
        let out = parse("");
        assert!(out.predicates.is_empty());
        assert!(out.regex_error.is_none());

        let out = parse("   ");
        assert!(out.predicates.is_empty());
    }

    #[test]
    fn bare_word_becomes_substring() {
        let out = parse("hello");
        assert_eq!(out.predicates.len(), 1);
        assert!(matches!(&out.predicates[0], Predicate::Substring(s) if s == "hello"));
    }

    #[test]
    fn multiple_bare_words_become_multiple_substrings() {
        let out = parse("hello world");
        assert_eq!(out.predicates.len(), 2);
        assert!(matches!(&out.predicates[0], Predicate::Substring(s) if s == "hello"));
        assert!(matches!(&out.predicates[1], Predicate::Substring(s) if s == "world"));
    }

    #[test]
    fn from_tag_produces_author() {
        let out = parse("from:alice");
        assert_eq!(out.predicates.len(), 1);
        let Predicate::Author(names) = &out.predicates[0] else {
            panic!("expected Author, got {:?}", out.predicates[0]);
        };
        assert_eq!(names, &vec!["alice".to_string()]);
    }

    #[test]
    fn from_tag_splits_commas() {
        let out = parse("from:a,b,c");
        let Predicate::Author(names) = &out.predicates[0] else {
            panic!()
        };
        assert_eq!(
            names,
            &vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn has_link_produces_link_predicate() {
        let out = parse("has:link");
        assert!(matches!(&out.predicates[0], Predicate::Link));
    }

    #[test]
    fn has_mention_and_emote_produce_respective_predicates() {
        assert!(matches!(
            parse("has:mention").predicates[0],
            Predicate::Mention
        ));
        assert!(matches!(parse("has:emote").predicates[0], Predicate::Emote));
    }

    #[test]
    fn in_tag_produces_channel() {
        let out = parse("in:angel");
        let Predicate::Channel(names) = &out.predicates[0] else {
            panic!()
        };
        assert_eq!(names, &vec!["angel".to_string()]);
    }

    #[test]
    fn is_tag_produces_flag() {
        assert!(matches!(
            parse("is:highlighted").predicates[0],
            Predicate::Flag(FlagKind::Highlighted)
        ));
        assert!(matches!(
            parse("is:reply").predicates[0],
            Predicate::Flag(FlagKind::Reply)
        ));
        assert!(matches!(
            parse("is:sub").predicates[0],
            Predicate::Flag(FlagKind::Sub)
        ));
    }

    #[test]
    fn regex_tag_compiles_pattern() {
        let out = parse(r#"regex:^!ban"#);
        let Predicate::Regex(re) = &out.predicates[0] else {
            panic!()
        };
        assert!(re.is_match("!ban alice"));
        assert!(out.regex_error.is_none());
    }

    #[test]
    fn regex_tag_with_bad_pattern_captures_error() {
        let out = parse(r#"regex:[bad("#);
        assert!(out.predicates.is_empty());
        assert!(out.regex_error.is_some());
    }

    #[test]
    fn badge_tag_expands_aliases() {
        let out = parse("badge:mod,sub,prime");
        let Predicate::Badge(names) = &out.predicates[0] else {
            panic!()
        };
        assert_eq!(
            names,
            &vec![
                "moderator".to_string(),
                "subscriber".to_string(),
                "premium".to_string()
            ]
        );
    }

    #[test]
    fn subtier_tag_parses_chars() {
        let out = parse("subtier:1,3");
        let Predicate::Subtier(tiers) = &out.predicates[0] else {
            panic!()
        };
        assert_eq!(tiers, &vec!['1', '3']);
    }

    #[test]
    fn unknown_tag_falls_back_to_substring() {
        let out = parse("foo:bar");
        assert_eq!(out.predicates.len(), 1);
        let Predicate::Substring(s) = &out.predicates[0] else {
            panic!()
        };
        assert_eq!(s, "foo:bar");
    }

    #[test]
    fn negation_wraps_predicate() {
        let out = parse("!from:bot");
        assert_eq!(out.predicates.len(), 1);
        let Predicate::Negated(inner) = &out.predicates[0] else {
            panic!()
        };
        assert!(matches!(&**inner, Predicate::Author(_)));

        let out2 = parse("-from:bot");
        assert!(matches!(&out2.predicates[0], Predicate::Negated(_)));
    }

    #[test]
    fn quoted_regex_preserves_whitespace() {
        let out = parse(r#"regex:"a b c""#);
        let Predicate::Regex(re) = &out.predicates[0] else {
            panic!()
        };
        assert!(re.is_match("a b c"));
        assert!(!re.is_match("abc"));
    }

    #[test]
    fn fuzz_random_bytes_never_panics() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut seed: u64 = 0x1234567890abcdef;
        for i in 0..1000 {
            let mut hasher = DefaultHasher::new();
            (seed, i).hash(&mut hasher);
            seed = hasher.finish();
            let len = (seed as usize) % 40;
            let bytes: Vec<u8> = (0..len)
                .map(|j| {
                    let mut h = DefaultHasher::new();
                    (seed, j).hash(&mut h);
                    (h.finish() & 0x7f) as u8
                })
                .collect();
            let s = String::from_utf8_lossy(&bytes).to_string();
            let _ = parse(&s); // must not panic
        }
    }
}
