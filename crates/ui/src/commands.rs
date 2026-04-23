use std::collections::HashMap;

use crust_core::plugin_command_infos;

/// Metadata for one built-in slash command.
#[derive(Clone, Copy)]
pub struct SlashCommandInfo {
    /// Primary command name without leading slash (e.g. "help").
    pub name: &'static str,
    /// Full usage string shown in help/autocomplete (e.g. "/help").
    pub usage: &'static str,
    /// Short one-line description.
    pub summary: &'static str,
    /// Optional aliases without leading slash.
    pub aliases: &'static [&'static str],
}

/// Owned slash-command suggestion used by autocomplete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommandSuggestion {
    pub name: String,
    pub usage: String,
    pub summary: String,
    pub aliases: Vec<String>,
}

impl From<&SlashCommandInfo> for SlashCommandSuggestion {
    fn from(value: &SlashCommandInfo) -> Self {
        Self {
            name: value.name.to_owned(),
            usage: value.usage.to_owned(),
            summary: value.summary.to_owned(),
            aliases: value.aliases.iter().map(|s| s.to_string()).collect(),
        }
    }
}

const BUILTIN_SLASH_COMMANDS: &[SlashCommandInfo] = &[
    SlashCommandInfo {
        name: "help",
        usage: "/help",
        summary: "Show command reference and slash-autocomplete tips.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "reloadplugins",
        usage: "/reloadplugins",
        summary: "Reload Crust Lua plugins from disk.",
        aliases: &["pluginsreload"],
    },
    SlashCommandInfo {
        name: "clearmessages",
        usage: "/clearmessages",
        summary: "Clear the current chat view locally (does not affect Twitch).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "chatters",
        usage: "/chatters",
        summary: "Show the current chatter count in this channel.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "poll",
        usage: "/poll --title \"<title>\" --choice \"<choice 1>\" --choice \"<choice 2>\" [--choice \"<choice 3>\"] [--duration <15..1800>|<60s|1m>] [--points <n>]",
        summary: "Create a Twitch poll with Chatterino-style flags (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "endpoll",
        usage: "/endpoll",
        summary: "End the active Twitch poll and archive results.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "cancelpoll",
        usage: "/cancelpoll",
        summary: "Cancel the active Twitch poll.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "redeem",
        usage: "/redeem <reward name>",
        summary: "Redeem a channel points reward by name.",
        aliases: &["reward"],
    },
    SlashCommandInfo {
        name: "prediction",
        usage: "/prediction <title> | <outcome1> | <outcome2> [--duration <30..1800>]",
        summary: "Create a Twitch prediction (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "lockprediction",
        usage: "/lockprediction",
        summary: "Lock wagering on the active Twitch prediction.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "endprediction",
        usage: "/endprediction <winning outcome index>",
        summary: "Resolve the active Twitch prediction.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "cancelprediction",
        usage: "/cancelprediction",
        summary: "Cancel the active Twitch prediction.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "commercial",
        usage: "/commercial [30|60|90|120|150|180]",
        summary: "Start a Twitch commercial break (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "marker",
        usage: "/marker [description]",
        summary: "Create a Twitch stream marker (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "announce",
        usage: "/announce <message> [--color primary|blue|green|orange|purple]",
        summary: "Send a Twitch announcement banner (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "shoutout",
        usage: "/shoutout <channel>",
        summary: "Send a Twitch shoutout to another channel (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unbanrequests",
        usage: "/unbanrequests",
        summary: "Fetch pending Twitch unban requests for this channel.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "resolveunban",
        usage: "/resolveunban <request_id> <approve|deny> [reason]",
        summary: "Resolve a Twitch unban request by id.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "automod",
        usage: "/automod <allow|deny> <message_id> <sender_user_id>",
        summary: "Approve or deny a held AutoMod message by id.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "modtools",
        usage: "/modtools",
        summary: "Open the in-app moderation tools window for this channel.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "requests",
        usage: "/requests [channel]",
        summary: "Open Twitch channel points reward queue for a channel.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "lowtrust",
        usage: "/lowtrust",
        summary: "Open low-trust moderation workflows (channel-dependent).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "mods",
        usage: "/mods",
        summary: "List current channel moderators (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "vips",
        usage: "/vips",
        summary: "List current channel VIPs (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "clip",
        usage: "/clip",
        summary: "Create a Twitch clip from the current stream (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "warn",
        usage: "/warn <user> [reason]",
        summary: "Issue a moderation warning (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "monitor",
        usage: "/monitor <user>",
        summary: "Mark a user as monitored/low-trust (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unmonitor",
        usage: "/unmonitor <user>",
        summary: "Remove monitored/low-trust status (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "restrict",
        usage: "/restrict <user>",
        summary: "Restrict a low-trust user (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unrestrict",
        usage: "/unrestrict <user>",
        summary: "Lift a low-trust restriction (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "ignore",
        usage: "/ignore <user>",
        summary: "Ignore a user locally/server-side depending on backend.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unignore",
        usage: "/unignore <user>",
        summary: "Remove an ignore on a user.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "fakemsg",
        usage: "/fakemsg <text>",
        summary: "Inject a local-only system message into chat.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "openurl",
        usage: "/openurl <url>",
        summary: "Open a URL in the system browser.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "nick",
        usage: "/nick <name>",
        summary: "Set your nickname for generic IRC servers.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "join",
        usage: "/join <#channel> [key]",
        summary: "Join or create an IRC channel on the current server.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "part",
        usage: "/part [#channel]",
        summary: "Leave an IRC channel (current channel by default).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "msg",
        usage: "/msg <target> <text>",
        summary: "Send IRC private or channel message.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "notice",
        usage: "/notice <target> <text>",
        summary: "Send an IRC NOTICE message.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "topic",
        usage: "/topic [#channel] [text]",
        summary: "Get or set channel topic on IRC.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "names",
        usage: "/names [#channel]",
        summary: "Request IRC nickname list for a channel.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "list",
        usage: "/list [mask]",
        summary: "List IRC channels available to join.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "mode",
        usage: "/mode <target> [modes]",
        summary: "Query or set IRC user/channel modes.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "kick",
        usage: "/kick <#channel> <nick> [reason]",
        summary: "Kick a user from an IRC channel (op required).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "invite",
        usage: "/invite <nick> [#channel]",
        summary: "Invite a user to an IRC channel.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "whois",
        usage: "/whois <nick>",
        summary: "Lookup user details via IRC WHOIS.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "who",
        usage: "/who [mask|#channel]",
        summary: "Query IRC users with WHO.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "away",
        usage: "/away [message]",
        summary: "Set or clear IRC away status.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "quit",
        usage: "/quit [message]",
        summary: "Disconnect from the current IRC server.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "pass",
        usage: "/pass <password>",
        summary: "Set IRC server password (applies on reconnect).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "server",
        usage: "/server <host[:port]>",
        summary: "Connect to an IRC server tab.",
        aliases: &["connect"],
    },
    SlashCommandInfo {
        name: "raw",
        usage: "/raw <line>",
        summary: "Send a raw IRC line directly to the server.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "popout",
        usage: "/popout [channel]",
        summary: "Open Twitch/Kick popout chat in the browser.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "user",
        usage: "/user <user>",
        summary: "Open a Twitch/Kick user page in the browser.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "usercard",
        usage: "/usercard <user>",
        summary: "Open the in-app user profile card.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "streamlink",
        usage: "/streamlink [channel]",
        summary: "Open the stream via streamlink:// URL scheme.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "banid",
        usage: "/banid <id>",
        summary: "Forward /ban using a Twitch user ID.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "timeout",
        usage: "/timeout <user> [seconds] [reason]",
        summary: "Twitch moderation timeout command (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "ban",
        usage: "/ban <user> [reason]",
        summary: "Twitch moderation ban command (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unban",
        usage: "/unban <user>",
        summary: "Lift a Twitch ban or timeout (server-side).",
        aliases: &["untimeout"],
    },
    SlashCommandInfo {
        name: "slow",
        usage: "/slow [seconds]",
        summary: "Enable Twitch slow mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "slowoff",
        usage: "/slowoff",
        summary: "Disable Twitch slow mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "followers",
        usage: "/followers [minutes]",
        summary: "Enable followers-only mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "followersoff",
        usage: "/followersoff",
        summary: "Disable followers-only mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "subscribers",
        usage: "/subscribers",
        summary: "Enable subscribers-only mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "subscribersoff",
        usage: "/subscribersoff",
        summary: "Disable subscribers-only mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "emoteonly",
        usage: "/emoteonly",
        summary: "Enable emote-only mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "emoteonlyoff",
        usage: "/emoteonlyoff",
        summary: "Disable emote-only mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "uniquechat",
        usage: "/uniquechat",
        summary: "Enable unique-chat mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "uniquechatoff",
        usage: "/uniquechatoff",
        summary: "Disable unique-chat mode (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "clear",
        usage: "/clear",
        summary: "Clear channel chat for everyone (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "delete",
        usage: "/delete <message-id>",
        summary: "Delete a specific Twitch message (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "mod",
        usage: "/mod <user>",
        summary: "Grant moderator role (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unmod",
        usage: "/unmod <user>",
        summary: "Remove moderator role (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "vip",
        usage: "/vip <user>",
        summary: "Grant VIP role (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unvip",
        usage: "/unvip <user>",
        summary: "Remove VIP role (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "color",
        usage: "/color <name>",
        summary: "Set your Twitch name color (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "raid",
        usage: "/raid <channel>",
        summary: "Start a Twitch raid (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unraid",
        usage: "/unraid",
        summary: "Cancel an outgoing Twitch raid (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "block",
        usage: "/block <user>",
        summary: "Block a Twitch user (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "unblock",
        usage: "/unblock <user>",
        summary: "Unblock a Twitch user (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "me",
        usage: "/me <message>",
        summary: "Send an action-style message (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "w",
        usage: "/w <user> <msg>",
        summary: "Send a Twitch whisper (Helix).",
        aliases: &["whisper"],
    },
    SlashCommandInfo {
        name: "r",
        usage: "/r <msg>",
        summary: "Reply to your most recent whisper (server-side).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "logs",
        usage: "/logs",
        summary: "Open the Crust log/data folder in the system file manager.",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "shield",
        usage: "/shield <on|off>",
        summary: "Toggle Twitch Shield Mode for this channel (mod/broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "setgame",
        usage: "/setgame <category>",
        summary: "Update the Twitch stream category (broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "settitle",
        usage: "/settitle <title>",
        summary: "Update the Twitch stream title (broadcaster only).",
        aliases: &[],
    },
    SlashCommandInfo {
        name: "follow-age",
        usage: "/follow-age [user]",
        summary: "Report how long a user has followed this channel (defaults to you).",
        aliases: &["followage"],
    },
    SlashCommandInfo {
        name: "account-age",
        usage: "/account-age [user]",
        summary: "Report the Twitch account age for a user (defaults to you).",
        aliases: &["accountage"],
    },
    SlashCommandInfo {
        name: "live",
        usage: "/live",
        summary: "List currently live Twitch channels that crust is tracking.",
        aliases: &[],
    },
];

/// Return all built-in commands.
pub fn built_in_commands() -> &'static [SlashCommandInfo] {
    BUILTIN_SLASH_COMMANDS
}

/// Build the `/help` message shown in chat.
pub fn render_help_message() -> String {
    let mut out = String::from(
        "Crust command guide\n\
Type `/` in chat for command autocomplete.\n\
Use Up/Down to navigate suggestions, Enter or Tab to accept.\n\
\n\
Built-in commands:\n",
    );

    for cmd in built_in_commands() {
        out.push_str(&format!("  {:<24} {}\n", cmd.usage, cmd.summary));
    }

    let plugin_commands = plugin_command_infos();
    if !plugin_commands.is_empty() {
        out.push_str("\nPlugin commands:\n");
        for cmd in &plugin_commands {
            out.push_str(&format!("  {:<24} {}\n", cmd.usage, cmd.summary));
        }
    }

    out.push_str(
        "\nAliases:\n\
  /whisper is the same as /w\n\
    /untimeout is the same as /unban\n\
\n\
Anonymous mode:\n\
    Plain messages and server-side slash commands require login.\n\
    Anonymous users can run local slash commands only.\n\
\n\
IRC tip:\n\
  In IRC tabs, uppercase protocol lines like `PRIVMSG #rust :hello`\n\
  are sent as raw IRC commands automatically.\n\
\n\
All other /commands are forwarded to the active chat backend\n\
(Twitch/IRC; Kick message sending is currently unavailable).",
    );

    out
}

/// If `buf` currently contains an in-progress slash command token, return
/// the command query (without leading slash). Returns `Some(\"\")` for `/`.
pub fn extract_slash_query(buf: &str) -> Option<&str> {
    // After a trailing whitespace, user has moved on to arguments.
    if buf
        .chars()
        .last()
        .map(|c| c.is_whitespace())
        .unwrap_or(false)
    {
        return None;
    }

    let trimmed_start = buf.trim_start();
    if !trimmed_start.starts_with('/') {
        return None;
    }

    let trimmed = trimmed_start.trim_end();
    let after = &trimmed[1..];
    if after.contains(char::is_whitespace) {
        return None;
    }
    Some(after)
}

/// Replace the in-progress slash token with `/<command> `.
pub fn replace_slash_token(buf: &mut String, command: &str) {
    let leading_len = buf.len() - buf.trim_start().len();
    let trimmed_end_len = buf.trim_end().len();

    if leading_len < trimmed_end_len {
        let token = &buf[leading_len..trimmed_end_len];
        if token.starts_with('/') && !token[1..].contains(char::is_whitespace) {
            buf.replace_range(leading_len..trimmed_end_len, &format!("/{command} "));
            return;
        }
    }

    *buf = format!("/{command} ");
}

/// Find command suggestions for a slash query.
pub fn slash_command_matches(query: &str, limit: usize) -> Vec<SlashCommandSuggestion> {
    let usage_counts: HashMap<String, u32> = HashMap::new();
    slash_command_matches_ranked(query, limit, &usage_counts)
}

/// Find command suggestions for a slash query, weighted by command usage.
pub fn slash_command_matches_ranked(
    query: &str,
    limit: usize,
    usage_counts: &HashMap<String, u32>,
) -> Vec<SlashCommandSuggestion> {
    let q = query.to_ascii_lowercase();
    let mut matches: Vec<(SlashCommandSuggestion, u8, String)> = built_in_commands()
        .iter()
        .map(SlashCommandSuggestion::from)
        .chain(
            plugin_command_infos()
                .into_iter()
                .map(|cmd| SlashCommandSuggestion {
                    name: cmd.name,
                    usage: cmd.usage,
                    summary: cmd.summary,
                    aliases: cmd.aliases,
                }),
        )
        .filter_map(|cmd| {
            // Compute the lowercase name once per cmd and reuse for all
            // match tests + the final sort key.  Aliases use
            // `eq_ignore_ascii_case` / substring tests that allocate
            // only for the contains fallback (rare).
            let name = cmd.name.to_ascii_lowercase();

            if q.is_empty() {
                return Some((cmd, 1, name));
            }

            let exact = name == q || cmd.aliases.iter().any(|al| al.eq_ignore_ascii_case(&q));
            if exact {
                return Some((cmd, 0, name));
            }

            let prefix = name.starts_with(&q)
                || cmd
                    .aliases
                    .iter()
                    .any(|al| starts_with_ignore_ascii_case(al, &q));
            if prefix {
                return Some((cmd, 1, name));
            }

            let contains = name.contains(&q)
                || cmd
                    .aliases
                    .iter()
                    .any(|al| contains_ignore_ascii_case(al, &q));
            if contains {
                return Some((cmd, 2, name));
            }

            let summary_contains = contains_ignore_ascii_case(&cmd.summary, &q)
                || contains_ignore_ascii_case(&cmd.usage, &q);
            if summary_contains {
                return Some((cmd, 3, name));
            }

            None
        })
        .collect();

    matches.sort_by(|(a, a_rank, a_name), (b, b_rank, b_name)| {
        let a_usage = usage_weight(a, usage_counts);
        let b_usage = usage_weight(b, usage_counts);

        a_rank
            .cmp(b_rank)
            .then_with(|| b_usage.cmp(&a_usage))
            .then_with(|| a_name.len().cmp(&b_name.len()))
            .then_with(|| a_name.cmp(b_name))
    });

    matches.truncate(limit);
    matches.into_iter().map(|(cmd, _, _)| cmd).collect()
}

fn starts_with_ignore_ascii_case(hay: &str, needle: &str) -> bool {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return false;
    }
    h[..n.len()]
        .iter()
        .zip(n.iter())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn contains_ignore_ascii_case(hay: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = hay.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    let first = n[0].to_ascii_lowercase();
    let last_start = h.len() - n.len();
    'outer: for start in 0..=last_start {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }
        for i in 1..n.len() {
            if !h[start + i].eq_ignore_ascii_case(&n[i]) {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

fn usage_weight(cmd: &SlashCommandSuggestion, usage_counts: &HashMap<String, u32>) -> u32 {
    let mut out = usage_counts
        .get(&cmd.name.to_ascii_lowercase())
        .copied()
        .unwrap_or(0);
    for alias in &cmd.aliases {
        out = out.saturating_add(
            usage_counts
                .get(&alias.to_ascii_lowercase())
                .copied()
                .unwrap_or(0),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        built_in_commands, extract_slash_query, slash_command_matches, slash_command_matches_ranked,
    };

    #[test]
    fn slash_query_detects_root_slash() {
        assert_eq!(extract_slash_query("/"), Some(""));
    }

    #[test]
    fn slash_matches_return_all_for_empty_query() {
        let all = slash_command_matches("", usize::MAX);
        assert_eq!(all.len(), built_in_commands().len());
    }

    #[test]
    fn slash_matches_include_prediction_commands() {
        let pred = slash_command_matches("pred", usize::MAX);
        assert!(pred.iter().any(|c| c.name == "prediction"));
    }

    #[test]
    fn slash_matches_include_new_moderation_discovery_commands() {
        let modtools = slash_command_matches("modtoo", usize::MAX);
        assert!(modtools.iter().any(|c| c.name == "modtools"));

        let lowtrust = slash_command_matches("restrict", usize::MAX);
        assert!(lowtrust.iter().any(|c| c.name == "restrict"));
        assert!(lowtrust.iter().any(|c| c.name == "unrestrict"));
    }

    #[test]
    fn slash_matches_include_commercial_command() {
        let matches = slash_command_matches("comm", usize::MAX);
        assert!(matches.iter().any(|c| c.name == "commercial"));
    }

    #[test]
    fn slash_matches_do_not_advertise_vote_command() {
        let matches = slash_command_matches("vote", usize::MAX);
        assert!(!matches.iter().any(|c| c.name == "vote"));
    }

    #[test]
    fn slash_matches_include_requests_command() {
        let matches = slash_command_matches("request", usize::MAX);
        assert!(matches.iter().any(|c| c.name == "requests"));
    }

    #[test]
    fn slash_matches_support_substring_discovery() {
        // Discovery should find commands by meaningful substrings.
        let noisy = slash_command_matches("edict", usize::MAX);
        assert!(noisy.iter().any(|c| c.name == "prediction"));
    }

    #[test]
    fn slash_matches_prioritize_recently_used_commands() {
        let mut usage = HashMap::new();
        usage.insert("shoutout".to_owned(), 25);
        usage.insert("help".to_owned(), 2);

        let ranked = slash_command_matches_ranked("", usize::MAX, &usage);
        assert_eq!(ranked.first().map(|c| c.name.as_str()), Some("shoutout"));
    }
}
