use std::collections::HashMap;

use crate::TwitchError;

// IrcTags: IRCv3 tag parsing and storage

#[derive(Debug, Clone, Default)]
pub struct IrcTags(pub HashMap<String, String>);

impl IrcTags {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Parse "@k=v;k2=v2 " prefix.
    fn parse(raw: &str) -> Self {
        let mut map = HashMap::new();
        for pair in raw.split(';') {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next().unwrap_or("").to_owned();
            let val = parts.next().unwrap_or("");
            if !key.is_empty() {
                map.insert(key, unescape_tag_value(val));
            }
        }
        Self(map)
    }
}

/// Unescape an IRCv3 tag value.
///
/// Twitch (and the IRCv3 spec) encode special characters in tag values:
/// - `\s` → space
/// - `\:` → `;`
/// - `\\` → `\`
/// - `\n` → newline
/// - `\r` → carriage-return
/// - Any other `\X` → `X` (passthrough)
fn unescape_tag_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some(':') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(x) => out.push(x),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

// IrcMessage: parsed IRC message structure

#[derive(Debug, Clone)]
pub struct IrcMessage {
    pub tags: IrcTags,
    /// e.g. "nick!user@host.tmi.twitch.tv"
    pub prefix: Option<String>,
    /// e.g. "PRIVMSG", "JOIN", "PING", "001", ...
    pub command: String,
    /// All parameters.  params[0] is the channel for PRIVMSG, etc.
    pub params: Vec<String>,
}

impl IrcMessage {
    /// The trailing (last) param, which is the message text.
    pub fn trailing(&self) -> Option<&str> {
        self.params.last().map(String::as_str)
    }

    /// Extract nick from "nick!user@host" prefix.
    pub fn nick(&self) -> Option<&str> {
        self.prefix.as_deref().and_then(|p| p.split('!').next())
    }
}

// Parser: IRC line and frame parsing utilities

/// Parse a single IRC line (no leading \r\n).
pub fn parse_line(line: &str) -> Result<IrcMessage, TwitchError> {
    let mut rest = line;

    // Optional IRCv3 tags: starts with '@'
    let tags = if rest.starts_with('@') {
        let end = rest
            .find(' ')
            .ok_or_else(|| TwitchError::IrcParse(format!("bad tags: {line}")))?;
        let tag_str = &rest[1..end];
        rest = &rest[end + 1..];
        IrcTags::parse(tag_str)
    } else {
        IrcTags::default()
    };

    // Optional prefix: starts with ':'
    let prefix = if rest.starts_with(':') {
        let end = rest
            .find(' ')
            .ok_or_else(|| TwitchError::IrcParse(format!("bad prefix: {line}")))?;
        let pfx = rest[1..end].to_owned();
        rest = &rest[end + 1..];
        Some(pfx)
    } else {
        None
    };

    // Command
    let cmd_end = rest.find(' ').unwrap_or(rest.len());
    let command = rest[..cmd_end].to_owned();
    rest = if cmd_end < rest.len() {
        &rest[cmd_end + 1..]
    } else {
        ""
    };

    // Params
    let mut params = Vec::new();
    while !rest.is_empty() {
        if rest.starts_with(':') {
            // Trailing param: everything remaining
            params.push(rest[1..].to_owned());
            break;
        }
        let end = rest.find(' ').unwrap_or(rest.len());
        params.push(rest[..end].to_owned());
        rest = if end < rest.len() {
            &rest[end + 1..]
        } else {
            ""
        };
    }

    Ok(IrcMessage {
        tags,
        prefix,
        command,
        params,
    })
}

/// Split a raw WebSocket text frame into individual IRC lines, then parse each.
///
/// Twitch may put multiple `\r\n`-delimited messages in one frame.
pub fn split_and_parse(frame: &str) -> Vec<Result<IrcMessage, TwitchError>> {
    frame
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .map(parse_line)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privmsg() {
        let line = "@badge-info=;color=#FF0000;display-name=SomeUser;id=abc-123;tmi-sent-ts=1700000000000 :someuser!someuser@someuser.tmi.twitch.tv PRIVMSG #testchan :hello world";
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.command, "PRIVMSG");
        assert_eq!(msg.params[0], "#testchan");
        assert_eq!(msg.trailing(), Some("hello world"));
        assert_eq!(msg.tags.get("color"), Some("#FF0000"));
        assert_eq!(msg.nick(), Some("someuser"));
    }

    #[test]
    fn ping() {
        let line = "PING :tmi.twitch.tv";
        let msg = parse_line(line).unwrap();
        assert_eq!(msg.command, "PING");
        assert_eq!(msg.trailing(), Some("tmi.twitch.tv"));
    }

    #[test]
    fn multi_frame() {
        let frame = "PING :tmi.twitch.tv\r\nPONG :tmi.twitch.tv\r\n";
        let msgs: Vec<_> = split_and_parse(frame)
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(msgs.len(), 2);
    }
}
