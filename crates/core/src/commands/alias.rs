//! Custom command aliases. Users define triggers like `/hello` that expand to
//! another message or command - e.g. `/hello = /me says hi {1} {2+}`.
//!
//! Mirrors Chatterino2's `CommandController` / `Command` but reworked as a
//! pure function that takes an input line and an alias list and returns the
//! expanded text. The UI layer calls [`expand_command_aliases`] before
//! dispatching a message, so aliases compose with the built-in slash parser:
//! aliases whose body starts with another `/<cmd>` get re-parsed through the
//! normal slash-command pipeline (server-side `/me`, Helix `/announce`, …).
//!
//! Supported variables in the alias body:
//!
//! | Token       | Expands to                                               |
//! | ----------- | -------------------------------------------------------- |
//! | `{1}`       | 1st whitespace-separated argument after the trigger      |
//! | `{2}` …     | n-th argument                                            |
//! | `{1+}`      | 1st argument plus everything after it (verbatim)         |
//! | `{2+}` …    | n-th argument plus everything after it                   |
//! | `{input}`   | Full argument list (everything after the trigger)        |
//! | `{channel}` | Current channel login (display name, no leading `#`)     |
//! | `{user}`    | Currently authenticated user login, or empty if anon     |
//! | `{streamer}`| Alias of `{channel}` - matches Chatterino's naming       |
//!
//! Missing positional arguments expand to the empty string (matching
//! Chatterino). Unknown variables (e.g. `{foo}`) are left untouched so users
//! can include literal braces in chat by picking names the engine doesn't
//! recognise.
//!
//! Recursion: aliases may invoke other aliases. We cap expansion depth at
//! [`MAX_ALIAS_DEPTH`] and reject cycles by tracking the chain of triggers
//! visited during a single expansion.

use serde::{Deserialize, Serialize};

/// One user-defined alias. `trigger` is the bare command name *without* the
/// leading slash (Chatterino stores it the same way). `body` is the raw
/// expansion template - it may itself start with `/` to chain into another
/// command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAlias {
    /// Bare command name (no leading `/`). Matched case-insensitively.
    pub trigger: String,
    /// Raw expansion body. May contain `{1}`, `{1+}`, `{channel}`, etc.
    pub body: String,
    /// Soft-disable toggle - disabled aliases are left in settings but skip
    /// expansion.
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

fn bool_true() -> bool {
    true
}

impl CommandAlias {
    /// Construct a new enabled alias, stripping a leading slash from the
    /// trigger if the user included one (Chatterino's "/hello" input works
    /// identically to "hello").
    pub fn new(trigger: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            trigger: normalize_trigger(&trigger.into()),
            body: body.into(),
            enabled: true,
        }
    }

    /// Canonical (lowercased, slash-stripped) trigger used for lookup.
    pub fn canonical_trigger(&self) -> String {
        normalize_trigger(&self.trigger).to_ascii_lowercase()
    }

    /// `true` if the trigger is usable - non-empty after normalising and has
    /// no embedded whitespace (a multi-token trigger would be ambiguous).
    pub fn is_valid(&self) -> bool {
        let t = normalize_trigger(&self.trigger);
        !t.is_empty()
            && !t.contains(char::is_whitespace)
            && !self.body.is_empty()
    }
}

/// Remove leading slashes and trim whitespace. Internal triggers are stored
/// without a slash so we can compare against the already-stripped `cmd` from
/// the slash parser.
pub fn normalize_trigger(raw: &str) -> String {
    let mut s = raw.trim();
    while let Some(rest) = s.strip_prefix('/') {
        s = rest.trim_start();
    }
    s.to_owned()
}

/// Max recursive alias expansions per input line. Chatterino uses a similar
/// cap; we pick a small number because any legitimate use rarely chains more
/// than two or three aliases.
pub const MAX_ALIAS_DEPTH: usize = 8;

/// Outcome of [`expand_command_aliases`]. `Unchanged` is returned when the
/// input is either not a slash command or the first token doesn't match any
/// enabled alias - callers can cheaply forward the original text in that
/// case without re-allocating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasExpansion {
    /// No alias matched. Original text should be used as-is.
    Unchanged,
    /// An alias matched and the input was fully expanded. Contains the final
    /// text plus the ordered chain of triggers that were expanded (useful
    /// for diagnostics / tests).
    Expanded {
        text: String,
        chain: Vec<String>,
    },
}

/// Errors raised while expanding an alias. Each variant carries enough
/// context for a user-facing chat-injected error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandAliasError {
    /// Alias chain revisited a trigger already in the expansion stack.
    Recursion {
        /// Ordered list of triggers that were expanded before the cycle was
        /// detected. The last element is the trigger that closed the cycle.
        chain: Vec<String>,
    },
    /// Expansion exceeded [`MAX_ALIAS_DEPTH`] without cycling. Usually means
    /// the user built a deeply nested alias tree without a cycle; still
    /// refuse to keep expanding to bound work per input line.
    DepthLimit {
        chain: Vec<String>,
    },
}

impl ExpandAliasError {
    /// Convert the error into a human-readable chat message suitable for
    /// injection via `AppCommand::InjectLocalMessage`.
    pub fn user_message(&self) -> String {
        match self {
            ExpandAliasError::Recursion { chain } => {
                format!(
                    "Command alias recursion detected ({}). Check your Commands settings for a cycle.",
                    format_chain(chain),
                )
            }
            ExpandAliasError::DepthLimit { chain } => {
                format!(
                    "Command alias expansion exceeded {MAX_ALIAS_DEPTH} levels ({}). Simplify your aliases.",
                    format_chain(chain),
                )
            }
        }
    }
}

fn format_chain(chain: &[String]) -> String {
    if chain.is_empty() {
        return "<empty>".to_owned();
    }
    chain
        .iter()
        .map(|t| format!("/{t}"))
        .collect::<Vec<_>>()
        .join(" → ")
}

/// Look up an alias by its trigger using case-insensitive matching. Disabled
/// entries and entries with invalid triggers are skipped so the UI can show
/// them in the editor without affecting runtime behaviour.
pub fn find_alias<'a>(aliases: &'a [CommandAlias], cmd: &str) -> Option<&'a CommandAlias> {
    let needle = normalize_trigger(cmd).to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    aliases.iter().find(|alias| {
        alias.enabled
            && alias.is_valid()
            && alias.canonical_trigger() == needle
    })
}

/// Try to expand `input` against `aliases`. Returns `Unchanged` when no alias
/// fires so the caller can forward the original text without copying.
///
/// `channel` is the current channel login (without a leading `#`), `user` is
/// the authenticated login (empty string when anonymous). These feed the
/// `{channel}` / `{streamer}` / `{user}` variables.
pub fn expand_command_aliases(
    input: &str,
    aliases: &[CommandAlias],
    channel: &str,
    user: &str,
) -> Result<AliasExpansion, ExpandAliasError> {
    // Fast path: input must start with `/` to hit any alias.
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return Ok(AliasExpansion::Unchanged);
    }

    // Peek the first token; if it's not an alias we have nothing to do.
    let (first_cmd, _first_rest) = split_command_head(trimmed);
    if find_alias(aliases, first_cmd).is_none() {
        return Ok(AliasExpansion::Unchanged);
    }

    // Preserve any leading whitespace the user typed so downstream parsers
    // that trim won't misbehave, even though parse_slash_command trims too.
    let leading_ws = &input[..input.len() - trimmed.len()];

    let mut current = trimmed.to_owned();
    let mut chain: Vec<String> = Vec::new();

    for _ in 0..=MAX_ALIAS_DEPTH {
        let (cmd, rest) = split_command_head(&current);
        let Some(alias) = find_alias(aliases, cmd) else {
            // Stopped on a non-alias command (or non-slash body). Return the
            // accumulated expansion.
            if chain.is_empty() {
                return Ok(AliasExpansion::Unchanged);
            }
            return Ok(AliasExpansion::Expanded {
                text: format!("{leading_ws}{current}"),
                chain,
            });
        };

        let canonical = alias.canonical_trigger();
        if chain.iter().any(|t| *t == canonical) {
            // Cycle: record the repeated trigger so the message points at it.
            let mut cycle = chain.clone();
            cycle.push(canonical);
            return Err(ExpandAliasError::Recursion { chain: cycle });
        }
        chain.push(canonical);

        if chain.len() > MAX_ALIAS_DEPTH {
            return Err(ExpandAliasError::DepthLimit { chain });
        }

        current = substitute_variables(&alias.body, rest, channel, user);
        // Re-normalise leading whitespace in the expansion so the next loop
        // iteration sees a fresh `/cmd` head. Aliases whose body does NOT
        // start with `/` terminate the chain on the next loop iteration
        // because split_command_head returns an empty cmd.
        current = current.trim_start().to_owned();
    }

    // If we fell out of the loop without returning, the last expansion
    // filled MAX_ALIAS_DEPTH + 1 iterations. Treat it as depth overflow so
    // the user gets a clean error instead of quiet truncation.
    Err(ExpandAliasError::DepthLimit { chain })
}

/// Split `"/cmd rest of line"` into `("cmd", "rest of line")`. Returns
/// `("", full)` when the input doesn't start with `/` (terminates alias
/// expansion loops cleanly).
fn split_command_head(line: &str) -> (&str, &str) {
    let Some(rest) = line.strip_prefix('/') else {
        return ("", line);
    };
    match rest.split_once(char::is_whitespace) {
        Some((cmd, tail)) => (cmd, tail.trim_start()),
        None => (rest, ""),
    }
}

/// Split `args` on ASCII whitespace, preserving order. Used to resolve
/// positional variables. We deliberately don't honour quotes - Chatterino's
/// engine is also whitespace-split - so users get predictable behaviour.
fn split_args(args: &str) -> Vec<&str> {
    args.split_whitespace().collect()
}

/// Substitute `{N}`, `{N+}`, `{input}`, `{channel}`, `{streamer}`, `{user}`
/// tokens in `body` using `args` as the remaining input. Unknown `{…}`
/// tokens are passed through verbatim.
fn substitute_variables(body: &str, args: &str, channel: &str, user: &str) -> String {
    let tokens = split_args(args);
    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `{…}` patterns. Anything else is copied byte-for-byte.
        if bytes[i] == b'{' {
            if let Some(close_rel) = body[i + 1..].find('}') {
                let close = i + 1 + close_rel;
                let var = &body[i + 1..close];
                let replacement =
                    resolve_variable(var, args, &tokens, channel, user);
                match replacement {
                    Some(s) => out.push_str(&s),
                    None => {
                        // Pass through verbatim, including the surrounding braces.
                        out.push_str(&body[i..=close]);
                    }
                }
                i = close + 1;
                continue;
            }
        }
        // Copy one UTF-8 char. We cannot slice by byte safely in multibyte
        // bodies, so walk one char at a time.
        let rest = &body[i..];
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Resolve a single `{var}` against the argument list. Returns `None` when
/// the variable is unknown so the caller can leave the literal `{var}` in
/// place (useful for chat text that happens to contain braces).
fn resolve_variable(
    var: &str,
    raw_args: &str,
    tokens: &[&str],
    channel: &str,
    user: &str,
) -> Option<String> {
    let v = var.trim();
    if v.is_empty() {
        return None;
    }
    // Positional `{N}` or `{N+}`.
    if let Some(num_str) = v.strip_suffix('+') {
        if let Ok(n) = num_str.parse::<usize>() {
            if n == 0 {
                return Some(String::new());
            }
            if n > tokens.len() {
                return Some(String::new());
            }
            return Some(slice_from_nth_token(raw_args, n));
        }
    }
    if let Ok(n) = v.parse::<usize>() {
        if n == 0 {
            return Some(String::new());
        }
        return Some(tokens.get(n - 1).map(|s| (*s).to_owned()).unwrap_or_default());
    }

    match v.to_ascii_lowercase().as_str() {
        "input" => Some(raw_args.trim().to_owned()),
        "channel" | "streamer" => Some(channel.to_owned()),
        "user" => Some(user.to_owned()),
        _ => None,
    }
}

/// Return the verbatim substring of `raw_args` starting at the n-th
/// whitespace-separated token (1-indexed). Preserves the exact whitespace
/// the user typed between tokens, matching Chatterino's behaviour - e.g.
/// `/hi hello   there` with `{1+}` yields `"hello   there"` rather than
/// `"hello there"`.
fn slice_from_nth_token(raw_args: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut token_index = 0usize;
    let mut in_token = false;
    for (byte_idx, ch) in raw_args.char_indices() {
        let whitespace = ch.is_whitespace();
        if !whitespace && !in_token {
            token_index += 1;
            in_token = true;
            if token_index == n {
                return raw_args[byte_idx..].to_owned();
            }
        } else if whitespace {
            in_token = false;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias(trigger: &str, body: &str) -> CommandAlias {
        CommandAlias::new(trigger, body)
    }

    #[test]
    fn unchanged_returned_for_non_slash_input() {
        let out =
            expand_command_aliases("hello world", &[alias("hi", "/me says hi {1}")], "ch", "me")
                .unwrap();
        assert_eq!(out, AliasExpansion::Unchanged);
    }

    #[test]
    fn unchanged_returned_when_no_alias_matches() {
        let out =
            expand_command_aliases("/nope arg", &[alias("hi", "/me says hi {1}")], "ch", "me")
                .unwrap();
        assert_eq!(out, AliasExpansion::Unchanged);
    }

    #[test]
    fn positional_variable_expands_basic_arg() {
        let aliases = vec![alias("hi", "/me says hi {1}")];
        let out = expand_command_aliases("/hi mary", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, chain } => {
                assert_eq!(text, "/me says hi mary");
                assert_eq!(chain, vec!["hi"]);
            }
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn plus_positional_preserves_trailing_whitespace() {
        let aliases = vec![alias("hi", "/me greets {1} and {2+}")];
        let out =
            expand_command_aliases("/hi mary how   are you", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, .. } => {
                assert_eq!(text, "/me greets mary and how   are you");
            }
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn missing_positional_expands_to_empty_string() {
        let aliases = vec![alias("hi", "hello {1} {2}")];
        let out = expand_command_aliases("/hi solo", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, .. } => assert_eq!(text, "hello solo "),
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn input_variable_includes_full_arg_list() {
        let aliases = vec![alias("yell", "/me YELLS: {input}")];
        let out = expand_command_aliases("/yell rise and shine", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, .. } => {
                assert_eq!(text, "/me YELLS: rise and shine");
            }
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn channel_and_user_variables_expand() {
        let aliases = vec![alias(
            "intro",
            "hello from {user} in #{channel} (streamer={streamer})",
        )];
        let out = expand_command_aliases("/intro", &aliases, "forsen", "bob").unwrap();
        match out {
            AliasExpansion::Expanded { text, .. } => {
                assert_eq!(text, "hello from bob in #forsen (streamer=forsen)");
            }
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn unknown_variables_are_left_untouched() {
        let aliases = vec![alias("dice", "rolls {1} d{sides}")];
        let out = expand_command_aliases("/dice 2", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, .. } => assert_eq!(text, "rolls 2 d{sides}"),
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn case_insensitive_trigger_match() {
        let aliases = vec![alias("Hi", "/me says hi")];
        let out = expand_command_aliases("/HI", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { chain, .. } => assert_eq!(chain, vec!["hi"]),
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn chained_aliases_expand_transitively() {
        let aliases = vec![
            alias("a", "/b {1+}"),
            alias("b", "/me fires {1+}"),
        ];
        let out = expand_command_aliases("/a the cannon", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, chain } => {
                assert_eq!(text, "/me fires the cannon");
                assert_eq!(chain, vec!["a", "b"]);
            }
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn direct_recursion_is_rejected() {
        let aliases = vec![alias("loop", "/loop again")];
        let err = expand_command_aliases("/loop", &aliases, "ch", "me").unwrap_err();
        match err {
            ExpandAliasError::Recursion { chain } => {
                assert_eq!(chain, vec!["loop", "loop"]);
            }
            other => panic!("expected recursion, got {other:?}"),
        }
    }

    #[test]
    fn mutual_recursion_is_rejected() {
        let aliases = vec![alias("a", "/b"), alias("b", "/a")];
        let err = expand_command_aliases("/a", &aliases, "ch", "me").unwrap_err();
        match err {
            ExpandAliasError::Recursion { chain } => {
                assert!(chain.contains(&"a".to_owned()));
                assert!(chain.contains(&"b".to_owned()));
            }
            other => panic!("expected recursion, got {other:?}"),
        }
    }

    #[test]
    fn disabled_alias_is_ignored() {
        let mut a = alias("hi", "/me hi");
        a.enabled = false;
        let out = expand_command_aliases("/hi", &[a], "ch", "me").unwrap();
        assert_eq!(out, AliasExpansion::Unchanged);
    }

    #[test]
    fn invalid_alias_is_ignored() {
        let a = CommandAlias {
            trigger: "bad word".to_owned(),
            body: "/me".to_owned(),
            enabled: true,
        };
        assert!(!a.is_valid());
        let out = expand_command_aliases("/bad", &[a], "ch", "me").unwrap();
        assert_eq!(out, AliasExpansion::Unchanged);
    }

    #[test]
    fn trigger_with_leading_slash_is_normalised() {
        let a = CommandAlias::new("/greet", "/me says hi {1}");
        assert_eq!(a.trigger, "greet");
    }

    #[test]
    fn non_slash_body_terminates_chain() {
        // After first expansion the body is plain text - the loop must stop.
        let aliases = vec![alias("greet", "hello {1}")];
        let out = expand_command_aliases("/greet world", &aliases, "ch", "me").unwrap();
        match out {
            AliasExpansion::Expanded { text, chain } => {
                assert_eq!(text, "hello world");
                assert_eq!(chain, vec!["greet"]);
            }
            _ => panic!("expected expansion"),
        }
    }

    #[test]
    fn user_message_mentions_cycle_triggers() {
        let err = ExpandAliasError::Recursion {
            chain: vec!["a".into(), "b".into(), "a".into()],
        };
        let msg = err.user_message();
        assert!(msg.contains("/a"));
        assert!(msg.contains("/b"));
    }

    #[test]
    fn find_alias_skips_disabled_and_invalid() {
        let mut disabled = alias("hi", "/me");
        disabled.enabled = false;
        let invalid = CommandAlias {
            trigger: String::new(),
            body: "/me".into(),
            enabled: true,
        };
        let good = alias("hi", "/me HI");
        let pool = vec![disabled, invalid, good.clone()];
        assert_eq!(find_alias(&pool, "hi"), Some(&good));
        assert_eq!(find_alias(&pool, "nope"), None);
    }
}
