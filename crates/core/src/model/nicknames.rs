use serde::{Deserialize, Serialize};

/// User login -> custom display name mapping.
///
/// A nickname whose `channel` is `None` applies
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

/// Apply the current alias while preserving the server's display name.
/// Removing an alias or changing its channel scope restores the original.
pub fn apply_sender_nickname(
    nicknames: &[Nickname],
    channel_login: &str,
    sender: &mut super::Sender,
) {
    if let Some(nickname) = lookup_nickname(nicknames, &sender.login, channel_login) {
        if sender.original_display_name.is_none() {
            sender.original_display_name = Some(sender.display_name.clone());
        }
        if sender.display_name != nickname.nickname {
            sender.display_name.clone_from(&nickname.nickname);
        }
    } else if let Some(original) = sender.original_display_name.take() {
        sender.display_name = original;
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

    fn sender() -> super::super::Sender {
        super::super::Sender {
            user_id: super::super::UserId("42".to_owned()),
            login: "alice".to_owned(),
            display_name: "AliceFromServer".to_owned(),
            original_display_name: None,
            color: None,
            name_paint: None,
            badges: Vec::new(),
        }
    }

    #[test]
    fn changing_and_removing_aliases_restores_the_original_name() {
        let mut sender = sender();
        apply_sender_nickname(&[Nickname::new("alice", "First alias")], "room", &mut sender);
        apply_sender_nickname(&[Nickname::new("alice", "Second alias")], "room", &mut sender);
        assert_eq!(sender.display_name, "Second alias");
        assert_eq!(sender.original_display_name.as_deref(), Some("AliceFromServer"));
        apply_sender_nickname(&[], "room", &mut sender);
        assert_eq!(sender.display_name, "AliceFromServer");
        assert!(sender.original_display_name.is_none());
    }

    #[test]
    fn moving_out_of_alias_scope_restores_the_original_name() {
        let mut nickname = Nickname::new("alice", "Local alias");
        nickname.channel = Some("first".to_owned());
        let mut sender = sender();
        apply_sender_nickname(std::slice::from_ref(&nickname), "first", &mut sender);
        assert_eq!(sender.display_name, "Local alias");
        apply_sender_nickname(&[nickname], "second", &mut sender);
        assert_eq!(sender.display_name, "AliceFromServer");
    }

    #[test]
    fn saved_aliases_keep_original_names_and_old_senders_still_load() {
        let mut sender = sender();
        let original_json = serde_json::to_value(&sender).unwrap();
        assert!(original_json.get("original_display_name").is_none());
        let old_sender: super::super::Sender = serde_json::from_value(original_json).unwrap();
        assert!(old_sender.original_display_name.is_none());
        apply_sender_nickname(&[Nickname::new("alice", "Local alias")], "room", &mut sender);
        let mut restored: super::super::Sender =
            serde_json::from_str(&serde_json::to_string(&sender).unwrap()).unwrap();
        apply_sender_nickname(&[], "room", &mut restored);
        assert_eq!(restored.display_name, "AliceFromServer");
    }
}
