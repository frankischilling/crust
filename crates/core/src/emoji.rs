//! Lightweight emoji detection and Twemoji URL generation.
//!
//! Does not aim to cover every emoji spec edge case - just the common
//! pictographic ranges that appear in Twitch chat.

/// Returns `true` if `c` is a likely start of an emoji sequence.
pub fn is_emoji_start(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x1F600..=0x1F64F   // Emoticons
        | 0x1F300..=0x1F5FF  // Misc Symbols and Pictographs
        | 0x1F680..=0x1F6FF  // Transport and Map
        | 0x1F1E0..=0x1F1FF  // Regional Indicator Symbols (flags)
        | 0x1F900..=0x1F9FF  // Supplemental Symbols and Pictographs
        | 0x1FA00..=0x1FA6F  // Chess Symbols
        | 0x1FA70..=0x1FAFF  // Symbols and Pictographs Extended-A
        | 0x2600..=0x26FF    // Misc Symbols
        | 0x2700..=0x27BF    // Dingbats
        | 0x2300..=0x23FF    // Misc Technical
        | 0x2B50..=0x2B55    // Stars, circles
        | 0x25AA..=0x25FE    // Geometric shapes (includes Watch, Hourglass)
        | 0x00A9             // ©
        | 0x00AE             // ®
        | 0x203C             // ‼
        | 0x2049             // ⁉
        | 0x2122             // ™
        | 0x2139             // ℹ
        | 0x2194..=0x21AA    // Arrows
        | 0x2934..=0x2935    // Arrows
        | 0x3030             // Wavy dash
        | 0x303D             // Part alternation mark
        | 0x3297             // Circled ideograph congratulation
        | 0x3299             // Circled ideograph secret
    )
}

/// Characters that can continue an emoji sequence (ZWJ, variation selectors,
/// skin-tone modifiers, regional indicator continuations, keycap, etc.).
pub fn is_emoji_continuation(c: char) -> bool {
    let cp = c as u32;
    is_emoji_start(c)
        || matches!(
            cp,
            0x200D             // Zero Width Joiner
            | 0xFE00..=0xFE0F  // Variation Selectors
            | 0x20E3            // Combining Enclosing Keycap
            | 0x1F3FB..=0x1F3FF // Skin tone modifiers
            | 0xE0020..=0xE007F // Tags (flag sequences)
            | 0xE0001           // Language tag
        )
}

/// Returns `true` only if the collected codepoint sequence is a real emoji
/// that Twemoji is likely to have an image for.
///
/// Rules (in order):
/// - Any codepoint ≥ U+1F000 → SMP emoji plane, Twemoji covers these fully.
/// - BMP sequence containing U+FE0F (emoji presentation selector) or U+20E3
///   (combining enclosing keycap) → explicitly emoji form.
/// - Whitelist of the half-dozen unambiguous BMP single-char emoji that
///   always render as pictures even without FE0F (©, ®, ™, ℹ, ‼, ⁉).
///
/// Everything else (lone geometric shapes, arrows, technical symbols) is
/// left as plain text to avoid □ rendering in the UI.
pub fn is_definitely_emoji(codepoints: &[u32]) -> bool {
    if codepoints.is_empty() {
        return false;
    }
    let first = codepoints[0];

    // SMP emoji (flag pairs, pictographs, etc.) – Twemoji covers these fully.
    // Regional indicators (0x1F1E0-0x1F1FF) are included here.
    if first >= 0x1F000 {
        return true;
    }

    // Any BMP sequence using emoji variation selector or keycap combiner.
    if codepoints.iter().any(|&cp| cp == 0xFE0F || cp == 0x20E3) {
        return true;
    }

    // Unambiguous always-emoji BMP chars (no FE0F needed).
    matches!(
        first,
        0x00A9   // ©
        | 0x00AE // ®
        | 0x203C // ‼
        | 0x2049 // ⁉
        | 0x2122 // ™
        | 0x2139 // ℹ
    )
}

/// Build a Twemoji CDN URL for the given codepoint sequence.
///
/// Twemoji uses lowercase hex codepoints separated by `-`, stripping
/// variation selectors like U+FE0F for most emoji.
///
/// Example: 😀  → `1f600`, 🇺🇸 → `1f1fa-1f1f8`
pub fn twemoji_url(codepoints: &[u32]) -> String {
    let hex: Vec<String> = codepoints
        .iter()
        .copied()
        // Strip common variation selector (VS16) which Twemoji omits in filenames
        .filter(|&cp| cp != 0xFE0F)
        .map(|cp| format!("{cp:x}"))
        .collect();
    let slug = hex.join("-");
    format!("https://cdn.jsdelivr.net/gh/twitter/twemoji@14.0.2/assets/72x72/{slug}.png")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smile_is_emoji() {
        assert!(is_emoji_start('😀'));
        assert!(is_emoji_start('❤'));
    }

    #[test]
    fn ascii_not_emoji() {
        assert!(!is_emoji_start('A'));
        assert!(!is_emoji_start(' '));
    }

    #[test]
    fn twemoji_url_simple() {
        let url = twemoji_url(&[0x1F600]);
        assert!(url.contains("1f600.png"));
    }

    #[test]
    fn twemoji_url_flag() {
        let url = twemoji_url(&[0x1F1FA, 0x1F1F8]);
        assert!(url.contains("1f1fa-1f1f8.png"));
    }
}
