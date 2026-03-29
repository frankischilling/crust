//! Comprehensive spell checker with frequency-weighted ranking, Soundex
//! phonetic matching, and QWERTY keyboard-proximity scoring.
//!
//! **Dictionary** – dwyl/english-words (~220 K lowercase alpha words, embedded
//! at compile time via `include_str!`).
//!
//! **Frequency** – Top 50 K English words from the OpenSubtitles corpus
//! (hermitdave/FrequencyWords).  Common words like "the" rank far above
//! obscure entries.
//!
//! **Algorithm** – Norvig edit-distance 1 + 2 (complete – no arbitrary cap),
//! augmented by Soundex phonetic lookup to catch silent-letter and
//! sound-alike errors that may exceed distance 2
//! (e.g. "fonetic" → "phonetic").
//!
//! **Scoring** – candidates are ranked by a combined metric:
//!   1. Edit distance (primary)
//!   2. Word frequency (strongly favours common words)
//!   3. Soundex phonetic match (bonus for same-sounding words)
//!   4. QWERTY keyboard proximity (bonus for adjacent-key typos)

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ── Lazy-initialised data ──────────────────────────────────────────────────

static SPELL: OnceLock<SpellData> = OnceLock::new();

struct SpellData {
    /// All known dictionary words (lowercase).
    words: HashSet<&'static str>,
    /// Word → frequency rank (0 = most common).  Only the top-50 K words
    /// appear here; the rest are treated as rare.
    freq_rank: HashMap<&'static str, u32>,
    /// Soundex code → words sharing that code.
    soundex_groups: HashMap<[u8; 4], Vec<&'static str>>,
}

fn data() -> &'static SpellData {
    SPELL.get_or_init(|| {
        let dict_src: &'static str = include_str!("words_alpha_filtered.txt");
        let freq_src: &'static str = include_str!("word_frequencies.txt");

        // ── Word set + Soundex index ───────────────────────────────────
        let mut words: HashSet<&'static str> = HashSet::with_capacity(230_000);
        let mut soundex_groups: HashMap<[u8; 4], Vec<&'static str>> =
            HashMap::with_capacity(60_000);

        for w in dict_src.lines() {
            if w.is_empty() {
                continue;
            }
            words.insert(w);
            let code = soundex_code(w);
            soundex_groups.entry(code).or_default().push(w);
        }

        // ── Frequency map ──────────────────────────────────────────────
        // Format: "word count" per line, most-common first.
        let mut freq_rank: HashMap<&'static str, u32> = HashMap::with_capacity(50_000);
        for (rank, line) in freq_src.lines().enumerate() {
            let w = match line.split_once(|c: char| c.is_whitespace()) {
                Some((word, _)) => word,
                None => line,
            };
            if let Some(&dict_ref) = words.get(w) {
                freq_rank.insert(dict_ref, rank as u32);
            }
        }

        SpellData {
            words,
            freq_rank,
            soundex_groups,
        }
    })
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Eagerly initialise the dictionary data structures.  Call this once at
/// startup so the first right-click doesn't pay the parsing cost.
pub fn init() {
    let _ = data();
}

/// Returns `true` when the word is known **or** should be skipped
/// (non-alpha, single char, etc.).
pub fn is_correct(word: &str) -> bool {
    if word.len() < 2 || !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return true; // skip non-alpha / very short tokens
    }
    let lower = word.to_ascii_lowercase();
    data().words.contains(lower.as_str())
}

/// Return up to `max` suggestions for a misspelled `word`, ranked by a
/// combined metric of edit distance, word frequency, phonetic similarity
/// and keyboard proximity.
pub fn suggestions(word: &str, max: usize) -> Vec<String> {
    let lower = word.to_ascii_lowercase();
    let d = data();

    // Maps candidate → minimum edit distance found.
    let mut candidates: HashMap<String, u8> = HashMap::new();

    // ── Edit-distance 1 ────────────────────────────────────────────────
    let edit1_list = edits1(&lower);
    for c in &edit1_list {
        if d.words.contains(c.as_str()) {
            candidates.entry(c.clone()).or_insert(1);
        }
    }

    // ── Edit-distance 2 (complete – no cap) ────────────────────────────
    for e1 in &edit1_list {
        for c in edits1(e1) {
            if d.words.contains(c.as_str()) {
                candidates.entry(c).or_insert(2);
            }
        }
    }

    // ── Soundex / phonetic matches ─────────────────────────────────────
    // Catches silent-letter errors (e.g. "fonetic" → "phonetic") that may
    // exceed edit-distance 2.
    let query_sx = soundex_code(&lower);
    if let Some(group) = d.soundex_groups.get(&query_sx) {
        for &pm in group {
            if !candidates.contains_key(pm) {
                let ed = edit_distance(&lower, pm);
                if ed <= 3 {
                    candidates.entry(pm.to_owned()).or_insert(ed as u8);
                }
            }
        }
    }

    // ── Score, sort, truncate ──────────────────────────────────────────
    let mut scored: Vec<(String, f64)> = candidates
        .into_iter()
        .map(|(cand, dist)| {
            let s = score(&lower, &cand, dist, &query_sx, d);
            (cand, s)
        })
        .collect();

    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(max);
    scored.into_iter().map(|(w, _)| w).collect()
}

/// Find the word (purely alphabetic) surrounding character position
/// `char_pos` in `buf`.
///
/// Returns `(word_slice, byte_start, byte_end)`.
pub fn word_at_cursor(buf: &str, char_pos: usize) -> (&str, usize, usize) {
    if buf.is_empty() {
        return ("", 0, 0);
    }

    let chars: Vec<char> = buf.chars().collect();
    let n = chars.len();
    let pos = char_pos.min(n);

    let mut start = pos;
    while start > 0 && chars[start - 1].is_ascii_alphabetic() {
        start -= 1;
    }

    let mut end = pos;
    while end < n && chars[end].is_ascii_alphabetic() {
        end += 1;
    }

    if start == end {
        return ("", char_pos, char_pos);
    }

    let byte_start = buf
        .char_indices()
        .nth(start)
        .map(|(i, _)| i)
        .unwrap_or(buf.len());
    let byte_end = buf
        .char_indices()
        .nth(end)
        .map(|(i, _)| i)
        .unwrap_or(buf.len());
    (&buf[byte_start..byte_end], byte_start, byte_end)
}

// ── Scoring ────────────────────────────────────────────────────────────────

/// Compute a single score for `candidate` (lower = better).
fn score(query: &str, candidate: &str, edit_dist: u8, query_sx: &[u8; 4], d: &SpellData) -> f64 {
    // Base: edit distance dominates.
    let mut s = edit_dist as f64 * 1000.0;

    // Frequency bonus – common words get up to 500 pts off.
    // rank 0 → bonus 500, rank 50 000 → bonus 0, unknown → no bonus.
    if let Some(&rank) = d.freq_rank.get(candidate) {
        let bonus = 500.0 * (1.0 - (rank as f64 / 50_000.0).min(1.0));
        s -= bonus;
    }

    // Phonetic bonus – same Soundex code as the query.
    if soundex_code(candidate) == *query_sx {
        s -= 200.0;
    }

    // Keyboard-proximity bonus for substitution-type typos.
    s -= keyboard_bonus(query, candidate) * 100.0;

    // Length-similarity penalty – prefer same-length corrections.
    let len_diff = (query.len() as f64 - candidate.len() as f64).abs();
    s += len_diff * 30.0;

    s
}

// ── Edit-distance machinery ────────────────────────────────────────────────

/// Generate all strings one edit (delete / transpose / replace / insert) away
/// from `word`.  All results are lowercase ASCII.
fn edits1(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    let mut out = Vec::with_capacity(54 * n + 26);

    // Deletions
    for i in 0..n {
        let mut s = String::with_capacity(n - 1);
        for (j, &c) in chars.iter().enumerate() {
            if j != i {
                s.push(c);
            }
        }
        out.push(s);
    }

    // Transpositions
    for i in 0..n.saturating_sub(1) {
        let mut v = chars.clone();
        v.swap(i, i + 1);
        out.push(v.into_iter().collect());
    }

    // Replacements
    for i in 0..n {
        for c in b'a'..=b'z' {
            let c = c as char;
            if c != chars[i] {
                let mut v = chars.clone();
                v[i] = c;
                out.push(v.into_iter().collect());
            }
        }
    }

    // Insertions
    for i in 0..=n {
        for c in b'a'..=b'z' {
            let mut v = Vec::with_capacity(n + 1);
            v.extend_from_slice(&chars[..i]);
            v.push(c as char);
            v.extend_from_slice(&chars[i..]);
            out.push(v.into_iter().collect());
        }
    }

    out
}

/// Levenshtein edit distance.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut prev = (0..=m).collect::<Vec<_>>();
    let mut curr = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

// ── Soundex ────────────────────────────────────────────────────────────────

/// Compute the 4-character Soundex code for `word`.
fn soundex_code(word: &str) -> [u8; 4] {
    let mut code = [b'0'; 4];
    let mut ci = 0usize;
    let mut last_digit = b'0';

    for (i, ch) in word.bytes().enumerate() {
        let ch = ch.to_ascii_lowercase();
        if i == 0 {
            code[0] = ch.to_ascii_uppercase();
            ci = 1;
            last_digit = soundex_digit(ch);
            continue;
        }
        let d = soundex_digit(ch);
        if d != b'0' && d != last_digit {
            if ci < 4 {
                code[ci] = d;
                ci += 1;
            }
        }
        if d != b'0' {
            last_digit = d;
        }
    }

    code
}

fn soundex_digit(c: u8) -> u8 {
    match c {
        b'b' | b'f' | b'p' | b'v' => b'1',
        b'c' | b'g' | b'j' | b'k' | b'q' | b's' | b'x' | b'z' => b'2',
        b'd' | b't' => b'3',
        b'l' => b'4',
        b'm' | b'n' => b'5',
        b'r' => b'6',
        _ => b'0', // vowels, h, w, y
    }
}

// ── Keyboard proximity ─────────────────────────────────────────────────────

/// Returns a bonus ∈ [0, 1] indicating how close the substitution errors
/// are on a QWERTY keyboard.  1 = every differing character is an adjacent
/// key (strong evidence of a fat-finger typo).
fn keyboard_bonus(query: &str, candidate: &str) -> f64 {
    let qc: Vec<char> = query.chars().collect();
    let cc: Vec<char> = candidate.chars().collect();
    if qc.len() != cc.len() {
        return 0.0;
    }
    let mut proximity_sum = 0.0f64;
    let mut diffs = 0u32;
    for (&q, &c) in qc.iter().zip(cc.iter()) {
        if q != c {
            diffs += 1;
            if let (Some(qp), Some(cp)) = (key_pos(q), key_pos(c)) {
                let dist = ((qp.0 - cp.0).powi(2) + (qp.1 - cp.1).powi(2)).sqrt();
                if dist <= 1.6 {
                    proximity_sum += 1.0; // adjacent key
                } else if dist <= 2.5 {
                    proximity_sum += 0.4; // nearby key
                }
            }
        }
    }
    if diffs == 0 {
        0.0
    } else {
        proximity_sum / diffs as f64
    }
}

/// Approximate QWERTY key positions as `(row, col)` with row-stagger
/// offsets so that adjacent keys on different rows are geometrically close.
fn key_pos(c: char) -> Option<(f64, f64)> {
    match c.to_ascii_lowercase() {
        // Row 0
        'q' => Some((0.0, 0.0)),
        'w' => Some((0.0, 1.0)),
        'e' => Some((0.0, 2.0)),
        'r' => Some((0.0, 3.0)),
        't' => Some((0.0, 4.0)),
        'y' => Some((0.0, 5.0)),
        'u' => Some((0.0, 6.0)),
        'i' => Some((0.0, 7.0)),
        'o' => Some((0.0, 8.0)),
        'p' => Some((0.0, 9.0)),
        // Row 1 (stagger +0.25)
        'a' => Some((1.0, 0.25)),
        's' => Some((1.0, 1.25)),
        'd' => Some((1.0, 2.25)),
        'f' => Some((1.0, 3.25)),
        'g' => Some((1.0, 4.25)),
        'h' => Some((1.0, 5.25)),
        'j' => Some((1.0, 6.25)),
        'k' => Some((1.0, 7.25)),
        'l' => Some((1.0, 8.25)),
        // Row 2 (stagger +0.75)
        'z' => Some((2.0, 0.75)),
        'x' => Some((2.0, 1.75)),
        'c' => Some((2.0, 2.75)),
        'v' => Some((2.0, 3.75)),
        'b' => Some((2.0, 4.75)),
        'n' => Some((2.0, 5.75)),
        'm' => Some((2.0, 6.75)),
        _ => None,
    }
}
