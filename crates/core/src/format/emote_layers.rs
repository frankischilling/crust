use crate::model::Span;

/// One layout item, borrowing the original spans so copy and saved text stay intact.
#[derive(Debug)]
pub enum DisplaySpan<'a> {
    Single(&'a Span),
    /// A base emote and its overlays, including their original separators.
    EmoteLayers(&'a [Span]),
}

pub struct DisplaySpans<'a> {
    remaining: &'a [Span],
}

/// Group consecutive overlay emotes with their base. Horizontal whitespace between
/// layers does not occupy space; text and line breaks end a group. An orphan overlay
/// remains an ordinary emote and can itself receive subsequent overlays.
pub fn display_spans(spans: &[Span]) -> DisplaySpans<'_> {
    DisplaySpans { remaining: spans }
}

impl<'a> Iterator for DisplaySpans<'a> {
    type Item = DisplaySpan<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = self.remaining.first()?;
        let mut end = 1;
        if matches!(first, Span::Emote { .. }) {
            let mut cursor = 1;
            while let Some(span) = self.remaining.get(cursor) {
                match span {
                    Span::Text { text, .. }
                        if !text.is_empty() && text.chars().all(|c| matches!(c, ' ' | '\t')) => {}
                    Span::Emote {
                        zero_width: true, ..
                    } => end = cursor + 1,
                    _ => break,
                }
                cursor += 1;
            }
        }
        let item = if end > 1 {
            DisplaySpan::EmoteLayers(&self.remaining[..end])
        } else {
            DisplaySpan::Single(first)
        };
        self.remaining = &self.remaining[end..];
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::tokenize_with_flags;
    use crate::model::TwitchEmotePos;

    fn lookup(code: &str) -> Option<(String, String, String, String, Option<String>, bool)> {
        matches!(code, "Base" | "Hat" | "Sparkle").then(|| {
            (
                code.into(),
                code.into(),
                format!("https://example.test/{code}.webp"),
                "7tv".into(),
                None,
                code != "Base",
            )
        })
    }

    fn visible_items(spans: &[Span]) -> Vec<String> {
        display_spans(spans)
            .map(|item| match item {
                DisplaySpan::Single(Span::Emote { code, .. }) => code.clone(),
                DisplaySpan::Single(Span::Text { text, .. }) => text.clone(),
                DisplaySpan::EmoteLayers(layers) => layers
                    .iter()
                    .filter_map(|span| match span {
                        Span::Emote { code, .. } => Some(code.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("+"),
                other => panic!("unexpected item: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn successive_layers_form_one_item_and_preserve_original_text() {
        let text = "Base  Hat Sparkle  Base Hat ";
        let spans = tokenize_with_flags(text, false, &[], &lookup);
        assert_eq!(
            visible_items(&spans),
            ["Base+Hat+Sparkle", " ", " ", "Base+Hat", " "]
        );
        let original: String = spans
            .iter()
            .map(|span| match span {
                Span::Emote { code, .. } => code.as_str(),
                Span::Text { text, .. } => text.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(original, text);
    }

    #[test]
    fn orphan_overlays_and_intervening_text_do_not_attach_to_earlier_emotes() {
        let spans = tokenize_with_flags("Hat Base text Hat", false, &[], &lookup);
        assert_eq!(
            visible_items(&spans),
            ["Hat", " ", "Base", " ", "text ", "Hat"]
        );
        let spans = tokenize_with_flags("Base \n Hat Sparkle", false, &[], &lookup);
        assert_eq!(visible_items(&spans), ["Base", " ", "\n ", "Hat+Sparkle"]);
        let spans = tokenize_with_flags("Hat Sparkle", false, &[], &lookup);
        assert_eq!(visible_items(&spans), ["Hat+Sparkle"]);
    }

    #[test]
    fn native_ranges_stay_authoritative_and_can_receive_overlays() {
        let spans = tokenize_with_flags(
            "Base Hat",
            false,
            &[TwitchEmotePos {
                id: "native".into(),
                start: 0,
                end: 3,
            }],
            &lookup,
        );
        assert!(
            matches!(&spans[0], Span::Emote { id, provider, zero_width: false, .. }
            if id == "native" && provider == "twitch")
        );
        assert!(matches!(
            &spans[2],
            Span::Emote {
                zero_width: true,
                ..
            }
        ));
        assert_eq!(visible_items(&spans), ["Base+Hat"]);
    }
}
