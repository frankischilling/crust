use serde::{Deserialize, Serialize};

/// User login → custom display name mapping.
///
/// Mirrors chatterino's `Nickname` entries but scoped per-field to the subset
/// the rest of crust needs.  A nickname whose `channel` is `None` applies
/// globally; when `Some(channel)`, it only applies in that channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Nickname {
    /// Lowercase (or case-preserved if `case_sensitive`) login of the user to rename.
    pub login: String,
    /// Replacement display name shown in chat, usercards, toasts.
    pub nickname: String,
    /// Also rewrite @mentions in message bodies of other users when they address this login.
    #[serde(default = "bool_true")]
    pub replace_mentions: bool,
    /// Match `login` case-sensitively.  Default is case-insensitive.
    #[serde(default)]
    pub case_sensitive: bool,
    /// `None` ⇒ global.  `Some(channel_login)` ⇒ only inside that channel.
    #[serde(default)]
    pub channel: Option<String>,
}

fn bool_true() -> bool {
    true
}

impl Nickname {
    /// Construct a global nickname entry with sensible defaults.
    pub fn new(login: impl Into<String>, nickname: impl Into<String>) -> Self {
        Self {
            login: login.into(),
            nickname: nickname.into(),
            replace_mentions: true,
            case_sensitive: false,
            channel: None,
        }
    }

    /// Does this entry's `login` field match the supplied login?
    pub fn matches_login(&self, login: &str) -> bool {
        if self.case_sensitive {
            self.login == login
        } else {
            self.login.eq_ignore_ascii_case(login)
        }
    }

    /// Is this entry in scope for the given channel login?
    pub fn matches_scope(&self, channel_login: &str) -> bool {
        match self.channel.as_deref() {
            None => true,
            Some(ch) => ch.eq_ignore_ascii_case(channel_login),
        }
    }
}

/// Find the best matching nickname for `login` in `channel_login`.
///
/// A channel-scoped entry always wins over a global one when both match.
pub fn lookup_nickname<'a>(
    nicknames: &'a [Nickname],
    login: &str,
    channel_login: &str,
) -> Option<&'a Nickname> {
    let chan_match = nicknames
        .iter()
        .find(|n| n.channel.is_some() && n.matches_scope(channel_login) && n.matches_login(login));
    if chan_match.is_some() {
        return chan_match;
    }
    nicknames
        .iter()
        .find(|n| n.channel.is_none() && n.matches_login(login))
}

/// Rewrite `display_name` in place if a matching nickname exists.
pub fn apply_nickname(
    nicknames: &[Nickname],
    login: &str,
    channel_login: &str,
    display_name: &mut String,
) {
    if let Some(n) = lookup_nickname(nicknames, login, channel_login) {
        *display_name = n.nickname.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_match_by_default() {
        let n = Nickname::new("angel_of_malice", "Angel");
        assert!(n.matches_login("ANGEL_OF_MALICE"));
    }

    #[test]
    fn case_sensitive_match_respects_flag() {
        let mut n = Nickname::new("Foo", "F");
        n.case_sensitive = true;
        assert!(!n.matches_login("foo"));
        assert!(n.matches_login("Foo"));
    }

    #[test]
    fn channel_scope_beats_global() {
        let global = Nickname::new("bob", "Bobby");
        let mut chan = Nickname::new("bob", "BobInChan");
        chan.channel = Some("somechan".into());
        let list = vec![global, chan];
        let hit = lookup_nickname(&list, "bob", "somechan").unwrap();
        assert_eq!(hit.nickname, "BobInChan");
    }

    #[test]
    fn global_matches_when_channel_scope_misses() {
        let mut chan = Nickname::new("bob", "BobInOther");
        chan.channel = Some("otherchan".into());
        let global = Nickname::new("bob", "Bobby");
        let list = vec![chan, global];
        let hit = lookup_nickname(&list, "bob", "somechan").unwrap();
        assert_eq!(hit.nickname, "Bobby");
    }

    #[test]
    fn apply_rewrites_display_name() {
        let list = vec![Nickname::new("angel_of_malice", "Angel")];
        let mut d = "AngelOfMalice".to_owned();
        apply_nickname(&list, "angel_of_malice", "anychan", &mut d);
        assert_eq!(d, "Angel");
    }

    #[test]
    fn apply_leaves_display_name_when_no_match() {
        let list = vec![Nickname::new("other", "X")];
        let mut d = "AngelOfMalice".to_owned();
        apply_nickname(&list, "angel_of_malice", "anychan", &mut d);
        assert_eq!(d, "AngelOfMalice");
    }
}
