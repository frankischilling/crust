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

const BUILTIN_SLASH_COMMANDS: &[SlashCommandInfo] = &[
    SlashCommandInfo {
        name: "help",
        usage: "/help",
        summary: "Show command reference and slash-autocomplete tips.",
        aliases: &[],
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
        name: "w",
        usage: "/w <user> <msg>",
        summary: "Send a Twitch whisper.",
        aliases: &["whisper"],
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

    out.push_str(
        "\nAliases:\n\
  /whisper is the same as /w\n\
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
pub fn slash_command_matches(query: &str, limit: usize) -> Vec<&'static SlashCommandInfo> {
    let q = query.to_ascii_lowercase();
    let mut matches: Vec<&SlashCommandInfo> = built_in_commands()
        .iter()
        .filter(|cmd| {
            if q.is_empty() {
                true
            } else {
                cmd.name.to_ascii_lowercase().contains(&q)
                    || cmd
                        .aliases
                        .iter()
                        .any(|a| a.to_ascii_lowercase().contains(&q))
            }
        })
        .collect();

    if !q.is_empty() {
        matches.sort_by(|a, b| {
            let a_name = a.name.to_ascii_lowercase();
            let b_name = b.name.to_ascii_lowercase();
            let a_prefix = a_name.starts_with(&q)
                || a.aliases
                    .iter()
                    .any(|al| al.to_ascii_lowercase().starts_with(&q));
            let b_prefix = b_name.starts_with(&q)
                || b.aliases
                    .iter()
                    .any(|al| al.to_ascii_lowercase().starts_with(&q));
            b_prefix
                .cmp(&a_prefix)
                .then_with(|| a_name.len().cmp(&b_name.len()))
                .then_with(|| a_name.cmp(&b_name))
        });
    }

    matches.truncate(limit);
    matches
}
