//! Lightweight spell-checker backed by an embedded English word list.
//!
//! * [`is_correct`] returns `true` when the word is known (or exempt).
//! * [`suggest`]     returns up to *n* corrections sorted by edit-distance.
//! * [`add_word`]    lets the user teach the checker new words at runtime.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

// Dictionary

/// Base dictionary loaded once from the embedded word list.
static BASE_DICT: OnceLock<HashSet<String>> = OnceLock::new();

/// Session-local words the user has added via "Add to dictionary".
static USER_DICT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn base_dictionary() -> &'static HashSet<String> {
    BASE_DICT.get_or_init(|| {
        let raw = include_str!("../words.txt");
        raw.lines()
            .map(|w| w.trim().to_ascii_lowercase())
            .filter(|w| !w.is_empty() && !w.starts_with('#'))
            .collect()
    })
}

fn user_dictionary() -> &'static Mutex<HashSet<String>> {
    USER_DICT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Add a word to the session dictionary so it is no longer flagged.
pub fn add_word(word: &str) {
    if let Ok(mut set) = user_dictionary().lock() {
        set.insert(word.to_ascii_lowercase());
    }
}

fn in_dictionary(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if base_dictionary().contains(&lower) {
        return true;
    }
    if let Ok(set) = user_dictionary().lock() {
        if set.contains(&lower) {
            return true;
        }
    }
    false
}

// Public API

/// A word is "correct" if it is in the dictionary **or** is exempt from
/// checking (very short, contains digits, looks like a URL / mention / emote).
pub fn is_correct(word: &str) -> bool {
    // Strip surrounding punctuation for the check
    let trimmed = word
        .trim_matches(|c: char| c.is_ascii_punctuation() && c != '\'' && c != '-');

    if trimmed.len() <= 2 {
        return true;
    }

    // Exempt patterns common in chat
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("www.")
    {
        return true;
    }
    if trimmed.starts_with('@')
        || trimmed.starts_with('/')
        || trimmed.starts_with(':')
        || trimmed.starts_with('#')
    {
        return true;
    }
    // Contains digits -> likely a number / username / emote code
    if trimmed.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    // Mixed-case interior (e.g. "PogChamp", "catJAM") -> likely emote
    if looks_like_emote(trimmed) {
        return true;
    }

    in_dictionary(trimmed)
}

/// Generate up to `max` spelling suggestions for `word`, ordered by
/// edit-distance (then alphabetically).
pub fn suggest(word: &str, max: usize) -> Vec<String> {
    let trimmed = word
        .trim_matches(|c: char| c.is_ascii_punctuation() && c != '\'' && c != '-');
    let lower = trimmed.to_ascii_lowercase();
    let dict = base_dictionary();

    let mut scored: Vec<(&str, usize)> = Vec::new();

    for dict_word in dict.iter() {
        // Quick length pre-filter: edit-distance can't be ≤ 2 when lengths
        // differ by more than 2.
        if dict_word.len().abs_diff(lower.len()) > 2 {
            continue;
        }
        let dist = levenshtein(&lower, dict_word);
        if dist > 0 && dist <= 2 {
            scored.push((dict_word.as_str(), dist));
        }
    }

    scored.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.0.len().cmp(&b.0.len()))
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(max);
    scored
        .into_iter()
        .map(|(w, _)| match_case(trimmed, w))
        .collect()
}

// Helpers

/// Levenshtein edit-distance between two ASCII-lowercase strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    // Single-row DP to save memory
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Heuristic: a token with an uppercase letter *after* a lowercase one is
/// probably an emote code (e.g. "catJAM", "PogChamp").
fn looks_like_emote(s: &str) -> bool {
    let mut saw_lower = false;
    for c in s.chars() {
        if c.is_lowercase() {
            saw_lower = true;
        } else if c.is_uppercase() && saw_lower {
            return true;
        }
    }
    false
}

/// Re-case `corrected` to match the casing pattern of `original`.
fn match_case(original: &str, corrected: &str) -> String {
    if original.chars().all(|c| c.is_uppercase() || !c.is_alphabetic()) {
        corrected.to_uppercase()
    } else if original
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        let mut chars = corrected.chars();
        match chars.next() {
            Some(first) => {
                let mut s = first.to_uppercase().to_string();
                s.push_str(chars.as_str());
                s
            }
            None => String::new(),
        }
    } else {
        corrected.to_string()
    }
}
