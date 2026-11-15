use std::sync::LazyLock;

use crate::model::Nickname;

/// Match aliases enabled for this login and channel.
pub fn contains_nickname_mention(
    text: &str,
    login: &str,
    channel: &str,
    nicknames: &[Nickname],
) -> bool {
    nicknames.iter().any(|nickname| {
        nickname.replace_mentions
            && nickname.matches_login(login)
            && nickname.matches_scope(channel)
            && contains_mention(text, &nickname.nickname)
    })
}

/// Match a complete login or nickname without treating a longer name as a mention.
pub fn contains_mention(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if text.is_ascii() && name.is_ascii() {
        let bytes = text.as_bytes();
        return bytes
            .windows(name.len())
            .enumerate()
            .any(|(start, candidate)| {
                candidate.eq_ignore_ascii_case(name.as_bytes())
                    && (start == 0 || !ascii_word_char(bytes[start - 1]))
                    && bytes
                        .get(start + name.len())
                        .is_none_or(|byte| !ascii_word_char(*byte))
            });
    }

    let text = text.to_lowercase();
    let name = name.to_lowercase();
    text.char_indices().any(|(start, _)| {
        text[start..].starts_with(&name)
            && text[..start]
                .chars()
                .next_back()
                .is_none_or(|ch| !word_char(ch))
            && text[start + name.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !word_char(ch))
    })
}

fn ascii_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn word_char(ch: char) -> bool {
    if ch.is_ascii() {
        return ascii_word_char(ch as u8);
    }
    // Unicode word characters include combining marks and connector punctuation.
    static WORD_CHAR: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\w$").expect("valid word-character pattern"));
    let mut bytes = [0; 4];
    WORD_CHAR.is_match(ch.encode_utf8(&mut bytes))
}

#[cfg(test)]
mod tests {
    use super::{contains_mention, contains_nickname_mention, Nickname};

    #[test]
    fn nickname_mentions_respect_channel_and_replacement_settings() {
        let mut scoped = Nickname::new("alice", "Local name");
        scoped.channel = Some("first".to_owned());
        let global = Nickname::new("alice", "Global name");
        let mut nicknames = vec![scoped, global];
        assert!(contains_nickname_mention(
            "@Local name",
            "ALICE",
            "FIRST",
            &nicknames
        ));
        assert!(!contains_nickname_mention(
            "@Local name",
            "alice",
            "second",
            &nicknames
        ));
        assert!(!contains_nickname_mention(
            "@Local names",
            "alice",
            "first",
            &nicknames
        ));
        assert!(!contains_nickname_mention(
            "@Local name",
            "bob",
            "first",
            &nicknames
        ));
        assert!(contains_nickname_mention(
            "@Global name",
            "alice",
            "second",
            &nicknames
        ));
        nicknames[0].replace_mentions = false;
        assert!(!contains_nickname_mention(
            "@Local name",
            "alice",
            "first",
            &nicknames
        ));
    }

    #[test]
    fn mention_matches_complete_names_with_or_without_at_sign() {
        for text in [
            "alice",
            "@ALICE",
            "hello Alice!",
            "(@alice), hi",
            "alice: yes",
        ] {
            assert!(contains_mention(text, "Alice"), "{text}");
        }
        for text in [
            "@alice_other",
            "@alice123",
            "@alicea",
            "malice",
            "_alice",
            "alice_",
        ] {
            assert!(!contains_mention(text, "alice"), "{text}");
        }
        assert!(contains_mention("@alice_other and @alice", "alice"));
        assert!(!contains_mention("anything", ""));
        assert!(!contains_mention("", "alice"));
    }

    #[test]
    fn mention_boundaries_respect_unicode_word_characters() {
        for text in [
            "@aliceé",
            "@alice\u{301}",
            "@alice\u{203f}more",
            "名alice",
            "alice名",
        ] {
            assert!(!contains_mention(text, "alice"), "{text}");
        }
        assert!(contains_mention("😀@ALICE。", "alice"));
        assert!(contains_mention("hello @ÉLODIE!", "Élodie"));
        assert!(!contains_mention("hello @Élodie2!", "Élodie"));
        assert!(contains_mention("hello @名!", "名"));
    }

    #[test]
    fn mention_preserves_multiword_aliases_and_irc_nick_punctuation() {
        assert!(contains_mention("hello @My Friend!", "my friend"));
        assert!(!contains_mention("hello @My Friends!", "my friend"));
        assert!(contains_mention("Nick[away]: hello", "Nick[away]"));
        assert!(!contains_mention("Nick[away]extra: hello", "Nick[away]"));
    }

    #[test]
    fn unicode_text_keeps_overlapping_alias_candidates() {
        for text in ["xbob bob bob", "xbob bob bob 🙂"] {
            assert!(contains_mention(text, "bob bob"), "{text}");
        }
        assert!(contains_mention("xélodie élodie élodie", "ÉLODIE ÉLODIE"));
        assert!(!contains_mention("xbob bob bobx 🙂", "bob bob"));
    }
}
