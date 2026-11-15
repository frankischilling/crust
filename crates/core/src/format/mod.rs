//! Tokenizer: turns raw chat text into pre-parsed [`Span`]s for the UI.
//!
//! The pipeline is:
//! 1. Mark Twitch-native emote ranges (from the `emotes` IRC tag).
//! 2. Walk remaining text word-by-word:
//!    a. Check third-party emote index (BTTV / FFZ / 7TV).
//!    b. Detect emoji -> produce Twemoji image URL.
//!    c. Detect URLs.
//!    d. Detect @mentions.
//!    e. Otherwise plain text.

use smallvec::SmallVec;

use crate::emoji;
use crate::model::{Span, TwitchEmotePos};

mod emote_layers;
pub use emote_layers::{display_spans, DisplaySpan, DisplaySpans};

/// Full tokenization entry-point.
///
/// The `emote_lookup` callback returns `(id, code, url_1x, provider, url_hd)`
/// where `url_hd` is an optional higher-resolution URL (4x/2x) for tooltips.
pub fn tokenize<F>(
    text: &str,
    is_action: bool,
    twitch_emotes: &[TwitchEmotePos],
    emote_lookup: &F,
) -> SmallVec<[Span; 8]>
where
    F: Fn(&str) -> Option<(String, String, String, String, Option<String>)>,
{
    tokenize_with_flags(text, is_action, twitch_emotes, &|code| {
        emote_lookup(code).map(|(id, code, url, provider, url_hd)| {
            (id, code, url, provider, url_hd, false)
        })
    })
}

/// Tokenize with active-set overlay metadata. The final lookup field is `zero_width`.
pub fn tokenize_with_flags<F>(
    text: &str,
    is_action: bool,
    twitch_emotes: &[TwitchEmotePos],
    emote_lookup: &F,
) -> SmallVec<[Span; 8]>
where
    F: Fn(&str) -> Option<(String, String, String, String, Option<String>, bool)>,
{
    if text.is_empty() {
        return SmallVec::new();
    }

    // Step 1: Build a sorted list of Twitch-native emote ranges
    let mut tw_sorted: Vec<&TwitchEmotePos> = twitch_emotes.iter().collect();
    tw_sorted.sort_by_key(|e| e.start);

    // Step 2: Split text into segments: twitch-emote vs. free-text
    let chars: Vec<char> = text.chars().collect();
    let segments = split_by_twitch_emotes(&chars, &tw_sorted);

    // Step 3: Process each segment
    let mut spans: SmallVec<[Span; 8]> = SmallVec::new();

    for seg in segments {
        match seg {
            Segment::TwitchEmote { id, code } => {
                let (url, url_hd) = emote_lookup(&code)
                    .filter(|(known_id, _, _, provider, _, _)| provider == "twitch" && known_id == &id)
                    .map(|(_, _, url, _, url_hd, _)| (url, url_hd))
                    .unwrap_or_else(|| (twitch_emote_url(&id), Some(twitch_emote_url_hd(&id))));
                spans.push(Span::Emote {
                    id,
                    code,
                    url,
                    url_hd,
                    provider: "twitch".into(),
                    zero_width: false,
                });
            }
            Segment::FreeText(s) => {
                tokenize_free_text(&s, is_action, emote_lookup, &mut spans);
            }
        }
    }

    spans
}

// Internal helpers: utility functions for tokenization

enum Segment {
    TwitchEmote { id: String, code: String },
    FreeText(String),
}

/// Split the full text into alternating FreeText / TwitchEmote segments
/// based on the byte-offset ranges from the `emotes` IRC tag.
///
/// NOTE: Twitch reports *character* offsets in the emotes tag (code-point
/// indices, not byte offsets). The `chars` slice is already exploded.
fn split_by_twitch_emotes(chars: &[char], emotes: &[&TwitchEmotePos]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut cursor: usize = 0;

    for e in emotes {
        if e.end < e.start || e.start < cursor || e.start > chars.len() || e.end >= chars.len() {
            continue; // malformed
        }
        // Free text before this emote
        if cursor < e.start {
            let s: String = chars[cursor..e.start].iter().collect();
            if !s.is_empty() {
                segments.push(Segment::FreeText(s));
            }
        }
        let code: String = chars[e.start..=e.end].iter().collect();
        segments.push(Segment::TwitchEmote {
            id: e.id.clone(),
            code,
        });
        cursor = e.end + 1;
    }

    // Trailing free text
    if cursor < chars.len() {
        let s: String = chars[cursor..].iter().collect();
        if !s.is_empty() {
            segments.push(Segment::FreeText(s));
        }
    }

    segments
}

/// Tokenize a free-text segment (no Twitch-native emotes inside).
fn tokenize_free_text<F>(
    text: &str,
    is_action: bool,
    emote_lookup: &F,
    out: &mut SmallVec<[Span; 8]>,
) where
    F: Fn(&str) -> Option<(String, String, String, String, Option<String>, bool)>,
{
    // Split preserving whitespace so that spaces are kept in output
    for word in text.split_inclusive(' ') {
        let without_leading = word.trim_start();
        let trimmed = without_leading.trim_end();
        if trimmed.is_empty() {
            // pure whitespace
            out.push(Span::Text {
                text: word.to_owned(),
                is_action,
            });
            continue;
        }
        let leading = &word[..word.len() - without_leading.len()];

        // Third-party emote?
        if let Some((id, code, url, provider, url_hd, zero_width)) = emote_lookup(trimmed) {
            if !leading.is_empty() {
                out.push(Span::Text { text: leading.to_owned(), is_action });
            }
            out.push(Span::Emote {
                id,
                code,
                url,
                url_hd,
                provider,
                zero_width,
            });
            // trailing space
            let trail = &without_leading[trimmed.len()..];
            if !trail.is_empty() {
                out.push(Span::Text {
                    text: trail.to_owned(),
                    is_action,
                });
            }
            continue;
        }

        // URL?
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let trail = &without_leading[trimmed.len()..];
            if !leading.is_empty() {
                out.push(Span::Text { text: leading.to_owned(), is_action });
            }
            // Channel-points rewards often ship only a link to a 7TV emote
            // page (e.g. https://7tv.app/emotes/<id>). Render those as the
            // actual emote image inline so the reward is visible, instead
            // of a bare link that hides what was redeemed.
            if let Some(id) = parse_seventv_emote_link(trimmed) {
                out.push(Span::Emote {
                    id: id.clone(),
                    code: id.clone(),
                    url: format!("https://cdn.7tv.app/emote/{id}/2x.webp"),
                    url_hd: Some(format!("https://cdn.7tv.app/emote/{id}/4x.webp")),
                    provider: "7tv".to_owned(),
                    zero_width: false,
                });
            } else {
                out.push(Span::Url {
                    text: trimmed.to_owned(),
                    url: trimmed.to_owned(),
                });
            }
            if !trail.is_empty() {
                out.push(Span::Text {
                    text: trail.to_owned(),
                    is_action,
                });
            }
            continue;
        }

        // @mention?
        if trimmed.starts_with('@') && trimmed.len() > 1 {
            let login = trimmed[1..].trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !login.is_empty() {
                if !leading.is_empty() {
                    out.push(Span::Text { text: leading.to_owned(), is_action });
                }
                out.push(Span::Mention {
                    login: login.to_owned(),
                });
                let trail = &without_leading[1 + login.len()..];
                if !trail.is_empty() {
                    out.push(Span::Text {
                        text: trail.to_owned(),
                        is_action,
                    });
                }
                continue;
            }
        }

        // Check for emoji within the word
        if tokenize_with_emoji(word, is_action, out) {
            continue;
        }

        // Plain text
        out.push(Span::Text {
            text: word.to_owned(),
            is_action,
        });
    }
}

/// Scan a word for emoji. Returns true if it contained any (and pushed spans).
fn tokenize_with_emoji(word: &str, is_action: bool, out: &mut SmallVec<[Span; 8]>) -> bool {
    let mut found_emoji = false;
    let mut text_buf = String::new();

    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if emoji::is_emoji_start(c) {
            // Collect the full grapheme cluster (ZWJ sequences, variation selectors,
            // tone modifiers). Adjacent independent emoji must NOT merge.
            let mut codepoints = vec![c as u32];
            loop {
                let Some(&next) = chars.peek() else { break };
                let ncp = next as u32;

                if emoji::is_emoji_combiner(ncp) {
                    codepoints.push(ncp);
                    chars.next();
                    continue;
                }

                if ncp == 0x200D {
                    // ZWJ only extends if another emoji follows.
                    let mut lookahead = chars.clone();
                    lookahead.next();
                    if matches!(lookahead.peek(), Some(&after) if emoji::is_emoji_start(after)) {
                        codepoints.push(0x200D);
                        chars.next();
                        continue;
                    }
                    break;
                }

                if emoji::is_regional_indicator(ncp)
                    && emoji::is_regional_indicator(*codepoints.last().unwrap())
                    && codepoints
                        .iter()
                        .filter(|&&cp| emoji::is_regional_indicator(cp))
                        .count()
                        < 2
                {
                    // Complete a single flag pair; another RI beyond that starts a new flag.
                    codepoints.push(ncp);
                    chars.next();
                    continue;
                }

                // Sequence started with a ZWJ just absorbed: accept the next base.
                if matches!(codepoints.last(), Some(&0x200D)) && emoji::is_emoji_start(next) {
                    codepoints.push(ncp);
                    chars.next();
                    continue;
                }

                break;
            }

            if emoji::is_definitely_emoji(&codepoints) {
                // Flush buffered text before the emoji span
                if !text_buf.is_empty() {
                    out.push(Span::Text {
                        text: std::mem::take(&mut text_buf),
                        is_action,
                    });
                }

                let emoji_text: String = codepoints
                    .iter()
                    .filter_map(|&cp| char::from_u32(cp))
                    .collect();
                let url = emoji::twemoji_url(&codepoints);
                out.push(Span::Emoji {
                    text: emoji_text,
                    url,
                });
                found_emoji = true;
            } else {
                // Ambiguous BMP symbol (geometric shape, arrow, etc.) - plain text
                for cp in codepoints {
                    if let Some(ch) = char::from_u32(cp) {
                        text_buf.push(ch);
                    }
                }
            }
        } else {
            text_buf.push(c);
        }
    }

    if found_emoji {
        if !text_buf.is_empty() {
            out.push(Span::Text {
                text: text_buf,
                is_action,
            });
        }
        true
    } else {
        false
    }
}

/// Extract the emote id from a 7TV emote page URL
/// (`https://7tv.app/emotes/<id>`). Returns `None` for anything else.
/// Trailing `/`, query, or fragment is tolerated.
fn parse_seventv_emote_link(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://7tv.app/emotes/")
        .or_else(|| url.strip_prefix("http://7tv.app/emotes/"))
        .or_else(|| url.strip_prefix("https://www.7tv.app/emotes/"))
        .or_else(|| url.strip_prefix("http://www.7tv.app/emotes/"))?;
    let id_end = rest
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let id = &rest[..id_end];
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(id.to_owned())
}

/// Build Twitch-native emote CDN URL (2x).
pub fn twitch_emote_url(id: &str) -> String {
    // Use `static` format to guarantee PNG responses and match the URLs
    // prefetched by TwitchGlobalProvider (avoids duplicate fetches and
    // ensures emote_bytes cache hits).
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/static/dark/1.0")
}

/// Build Twitch-native emote CDN URL at 4x scale for HD tooltips.
pub fn twitch_emote_url_hd(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/static/dark/3.0")
}

/// Parse the raw `emotes` IRC tag value into structured positions.
///
/// Format: `<id>:<start>-<end>,<start>-<end>/<id>:<start>-<end>`
pub fn parse_twitch_emotes_tag(tag: &str) -> Vec<TwitchEmotePos> {
    if tag.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for group in tag.split('/') {
        let mut parts = group.splitn(2, ':');
        let id = match parts.next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let ranges = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        for range in ranges.split(',') {
            let mut bounds = range.splitn(2, '-');
            let start: usize = bounds.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let end: usize = bounds.next().and_then(|s| s.parse().ok()).unwrap_or(start);
            out.push(TwitchEmotePos {
                id: id.to_owned(),
                start,
                end,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        let spans = tokenize("hello world", false, &[], &|_| None);
        assert_eq!(spans.len(), 2);
        match &spans[0] {
            Span::Text { text, .. } => assert_eq!(text, "hello "),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn emote_lookup() {
        let spans = tokenize("hey Kappa lol", false, &[], &|w| {
            if w == "Kappa" {
                Some((
                    "25".into(),
                    "Kappa".into(),
                    "https://example.com/kappa.png".into(),
                    "bttv".into(),
                    Some("https://example.com/kappa-4x.png".into()),
                ))
            } else {
                None
            }
        });
        let emote_count = spans
            .iter()
            .filter(|s| matches!(s, Span::Emote { .. }))
            .count();
        assert_eq!(emote_count, 1);
    }

    #[test]
    fn unicode_tokens_keep_leading_and_trailing_whitespace_at_character_boundaries() {
        let text = "\tÉ \u{2003}https://example.test/é\u{a0} \n@名,\t 😀😙\u{2002}";
        let spans = tokenize(text, true, &[], &|code| (code == "É").then(|| (
            "unicode".into(), code.into(), "https://example.test/emote.png".into(),
            "7tv".into(), None,
        )));
        let reconstructed: String = spans.iter().map(|span| match span {
            Span::Text { text, is_action } => {
                assert!(*is_action);
                text.clone()
            }
            Span::Emote { code, .. } => code.clone(),
            Span::Url { text, url } => {
                assert_eq!(url, "https://example.test/é");
                text.clone()
            }
            Span::Mention { login } => format!("@{login}"),
            Span::Emoji { text, .. } => text.clone(),
            other => panic!("unexpected span: {other:?}"),
        }).collect();
        assert_eq!(reconstructed, text);
        assert!(spans.iter().any(|span| matches!(span, Span::Emote { id, .. } if id == "unicode")));
        assert!(spans.iter().any(|span| matches!(span, Span::Mention { login } if login == "名")));
        assert_eq!(spans.iter().filter(|span| matches!(span, Span::Emoji { .. })).count(), 2);
    }

    #[test]
    fn whitespace_around_native_ranges_and_emote_links_is_preserved() {
        let positions = [TwitchEmotePos { id: "25".into(), start: 1, end: 5 }];
        let spans = tokenize("\tKappa\n", false, &positions, &|_| None);
        assert!(matches!(&spans[0], Span::Text { text, is_action: false } if text == "\t"));
        assert!(matches!(&spans[1], Span::Emote { id, code, .. } if id == "25" && code == "Kappa"));
        assert!(matches!(&spans[2], Span::Text { text, is_action: false } if text == "\n"));

        let spans = tokenize("\thttps://7tv.app/emotes/abc123\u{a0}", false, &[], &|_| None);
        assert!(matches!(&spans[0], Span::Text { text, .. } if text == "\t"));
        assert!(matches!(&spans[1], Span::Emote { id, provider, .. } if id == "abc123" && provider == "7tv"));
        assert!(matches!(&spans[2], Span::Text { text, .. } if text == "\u{a0}"));
    }

    #[test]
    fn twitch_emotes_tag_parse() {
        let tag = "25:0-4,12-16/1902:6-10";
        let positions = parse_twitch_emotes_tag(tag);
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].id, "25");
        assert_eq!(positions[0].start, 0);
        assert_eq!(positions[0].end, 4);
    }

    #[test]
    fn seventv_link_parse() {
        assert_eq!(
            parse_seventv_emote_link("https://7tv.app/emotes/01K2AN1RWND0043X61B48HNQFA"),
            Some("01K2AN1RWND0043X61B48HNQFA".to_owned())
        );
        assert_eq!(
            parse_seventv_emote_link("https://7tv.app/emotes/abc123/"),
            Some("abc123".to_owned())
        );
        assert_eq!(
            parse_seventv_emote_link("https://7tv.app/emotes/abc?ref=x"),
            Some("abc".to_owned())
        );
        assert_eq!(parse_seventv_emote_link("https://7tv.app/emotes/"), None);
        assert_eq!(parse_seventv_emote_link("https://example.com/x"), None);
        assert_eq!(parse_seventv_emote_link("https://7tv.app/users/abc"), None);
    }

    #[test]
    fn seventv_link_becomes_emote_span() {
        let spans = tokenize(
            "look https://7tv.app/emotes/01K2AN1RWND0043X61B48HNQFA nice",
            false,
            &[],
            &|_| None,
        );
        let emote = spans.iter().find_map(|s| match s {
            Span::Emote {
                id, url, provider, ..
            } if provider == "7tv" => Some((id.clone(), url.clone())),
            _ => None,
        });
        assert_eq!(
            emote,
            Some((
                "01K2AN1RWND0043X61B48HNQFA".to_owned(),
                "https://cdn.7tv.app/emote/01K2AN1RWND0043X61B48HNQFA/2x.webp".to_owned()
            ))
        );
    }

    #[test]
    fn adjacent_emoji_do_not_merge() {
        // 😙 (1F619) + 👌🏻 (1F44C + 1F3FB) + 💨 (1F4A8) -> 3 distinct spans.
        let spans = tokenize("\u{1F619}\u{1F44C}\u{1F3FB}\u{1F4A8}", false, &[], &|_| None);
        let emoji_urls: Vec<&str> = spans
            .iter()
            .filter_map(|s| match s {
                Span::Emoji { url, .. } => Some(url.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(emoji_urls.len(), 3);
        assert!(emoji_urls[0].ends_with("1f619.png"));
        assert!(emoji_urls[1].ends_with("1f44c-1f3fb.png"));
        assert!(emoji_urls[2].ends_with("1f4a8.png"));
    }

    #[test]
    fn zwj_sequence_merges() {
        // 👨‍💻 man + ZWJ + laptop -> one span.
        let spans = tokenize("\u{1F468}\u{200D}\u{1F4BB}", false, &[], &|_| None);
        let emoji_count = spans
            .iter()
            .filter(|s| matches!(s, Span::Emoji { .. }))
            .count();
        assert_eq!(emoji_count, 1);
    }

    #[test]
    fn two_flags_do_not_merge() {
        // 🇺🇸🇯🇵 -> two flag spans, not one super-flag.
        let spans = tokenize(
            "\u{1F1FA}\u{1F1F8}\u{1F1EF}\u{1F1F5}",
            false,
            &[],
            &|_| None,
        );
        let emoji_count = spans
            .iter()
            .filter(|s| matches!(s, Span::Emoji { .. }))
            .count();
        assert_eq!(emoji_count, 2);
    }

    #[test]
    fn native_ranges_use_matching_catalog_assets_without_accepting_code_collisions() {
        let positions = [TwitchEmotePos { id: "native".into(), start: 0, end: 3 }];
        for (id, provider, expected) in [
            ("native", "twitch", "https://example.test/animated.gif"),
            ("other", "twitch", "https://static-cdn.jtvnw.net/emoticons/v2/native/static/dark/1.0"),
            ("native", "7tv", "https://static-cdn.jtvnw.net/emoticons/v2/native/static/dark/1.0"),
        ] {
            let spans = tokenize("Wave", false, &positions, &|_| Some((
                id.into(), "Wave".into(), "https://example.test/animated.gif".into(),
                provider.into(), None,
            )));
            let Span::Emote { url, id, provider, .. } = &spans[0] else { panic!("expected native emote"); };
            assert_eq!(url, expected);
            assert_eq!(id, "native");
            assert_eq!(provider, "twitch");
        }
    }

    #[test]
    fn reversed_native_ranges_are_ignored_without_changing_valid_unicode_ranges() {
        for (ranges, expected) in [
            (vec![(0, 0), (1, 2), (3, 4)], vec!["🙂", "漢字", "ab"]),
            (vec![(0, 2), (1, 3)], vec!["🙂漢字"]),
        ] {
            let mut positions: Vec<TwitchEmotePos> = ranges.into_iter().enumerate()
                .map(|(id, (start, end))| TwitchEmotePos { id: id.to_string(), start, end })
                .collect();
            positions.push(TwitchEmotePos { id: "malformed".into(), start: 4, end: 1 });
            let spans = tokenize("🙂漢字ab", false, &positions, &|_| None);
            let codes: Vec<&str> = spans.iter().filter_map(|span| match span {
                Span::Emote { id, code, provider, .. } => {
                    assert_ne!(id, "malformed");
                    assert_eq!(provider, "twitch");
                    Some(code.as_str())
                }
                _ => None,
            }).collect();
            assert_eq!(codes, expected);
        }
    }

    #[test]
    fn overlapping_native_emote_ranges_do_not_duplicate_or_rewind_text() {
        let text = "🙂Kappa tail";
        for positions in [
            vec![(1, 5), (1, 5)],
            vec![(1, 5), (2, 3)],
            vec![(1, 5), (3, 7)],
            vec![(3, 7), (1, 5)],
        ] {
            let positions: Vec<_> = positions.into_iter().enumerate().map(|(id, (start, end))| {
                TwitchEmotePos { id: id.to_string(), start, end }
            }).collect();
            let spans = tokenize(text, false, &positions, &|_| None);
            let restored: String = spans.iter().map(|span| match span {
                Span::Emote { code, .. } => code.as_str(),
                Span::Text { text, .. } | Span::Emoji { text, .. } => text.as_str(),
                other => panic!("unexpected span: {other:?}"),
            }).collect();
            assert_eq!(restored, text);
            assert_eq!(spans.iter().filter(|span| matches!(span, Span::Emote { .. })).count(), 1);
        }
    }

    #[test]
    fn twitch_emotes_inline() {
        let text = "Kappa test Kappa";
        let emotes = vec![
            TwitchEmotePos {
                id: "25".into(),
                start: 0,
                end: 4,
            },
            TwitchEmotePos {
                id: "25".into(),
                start: 11,
                end: 15,
            },
        ];
        let spans = tokenize(text, false, &emotes, &|_| None);
        let emote_count = spans
            .iter()
            .filter(|s| matches!(s, Span::Emote { .. }))
            .count();
        assert_eq!(emote_count, 2);
    }
}
