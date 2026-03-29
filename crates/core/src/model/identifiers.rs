use serde::{Deserialize, Serialize};

/// Streaming platform that a channel belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Platform {
    #[default]
    Twitch,
    Kick,
    Irc,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Twitch => write!(f, "Twitch"),
            Platform::Kick => write!(f, "Kick"),
            Platform::Irc => write!(f, "IRC"),
        }
    }
}

/// Channel identifier that encodes both platform and channel name.
///
/// Internally:
/// - Kick channels are stored as "kick:<slug>"
/// - IRC channels are stored as "irc:<tls>:<host>:<port>:<channel>"
/// - Twitch channels are stored as the bare lowercase login (no `#` prefix)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub String);

pub const IRC_SERVER_CONTROL_CHANNEL: &str = "__server__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcTarget {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub channel: String,
}

impl ChannelId {
    /// Create a Twitch channel ID (default, backward-compatible).
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_lowercase().trim_start_matches('#').to_owned())
    }

    /// Create a Kick channel ID.
    pub fn kick(slug: impl Into<String>) -> Self {
        let slug = slug.into().to_lowercase();
        Self(format!("kick:{slug}"))
    }

    /// Create an IRC channel target.
    ///
    /// The `channel` parameter should be the internal form: wire name with
    /// the **first** `#` already stripped.  For example `##chat` on the wire
    /// is stored as `#chat` internally and reconstructed as `##chat` when
    /// building IRC commands by prefixing `#`.
    pub fn irc(host: impl Into<String>, port: u16, tls: bool, channel: impl Into<String>) -> Self {
        let host = host.into().trim().to_lowercase();
        let channel = channel.into().trim().to_lowercase();
        let tls_flag = if tls { "1" } else { "0" };
        Self(format!("irc:{tls_flag}:{host}:{port}:{channel}"))
    }

    pub fn platform(&self) -> Platform {
        if self.0.starts_with("kick:") {
            Platform::Kick
        } else if self.0.starts_with("irc:") {
            Platform::Irc
        } else {
            Platform::Twitch
        }
    }

    /// Human-readable channel name (strips any platform prefix).
    pub fn display_name(&self) -> &str {
        if let Some(v) = self.0.strip_prefix("kick:") {
            v
        } else if self.0.starts_with("irc:") {
            let mut parts = self.0.splitn(5, ':');
            let _ = parts.next(); // irc
            let _ = parts.next(); // tls flag
            let host = parts.next().unwrap_or(&self.0);
            let _ = parts.next(); // port
            let channel = parts.next().unwrap_or(host);
            if channel == IRC_SERVER_CONTROL_CHANNEL {
                host
            } else {
                channel
            }
        } else {
            &self.0
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns "#channel" form required by Twitch IRC JOIN / PRIVMSG.
    pub fn irc_name(&self) -> String {
        format!("#{}", self.display_name())
    }

    /// Returns the Kick slug, if this is a Kick channel.
    pub fn kick_slug(&self) -> Option<&str> {
        self.0.strip_prefix("kick:")
    }

    /// Decode canonical IRC channel form.
    pub fn irc_target(&self) -> Option<IrcTarget> {
        let rest = self.0.strip_prefix("irc:")?;
        let mut parts = rest.splitn(4, ':');
        let tls_part = parts.next()?;
        let host = parts.next()?.to_owned();
        let port = parts.next()?.parse::<u16>().ok()?;
        let channel = parts.next()?.to_owned();
        let tls = matches!(tls_part, "1" | "true" | "tls");
        if host.is_empty() || channel.is_empty() {
            return None;
        }
        Some(IrcTarget {
            host,
            port,
            tls,
            channel,
        })
    }

    /// Parse user input from join/auto-join formats.
    ///
    /// Supported:
    /// - `channel` or `twitch:channel`
    /// - `kick:channel`
    /// - `irc://host[:port]/channel`
    /// - `ircs://host[:port]/channel`
    /// - `irc:host[:port]/channel`
    pub fn parse_user_input(raw: &str) -> Option<Self> {
        let input = raw.trim();
        if input.is_empty() {
            return None;
        }

        if let Some(slug) = input.strip_prefix("kick:") {
            let slug = slug.trim().trim_start_matches('#');
            if slug.is_empty() {
                return None;
            }
            return Some(Self::kick(slug));
        }

        if let Some(name) = input.strip_prefix("twitch:") {
            let name = name.trim().trim_start_matches('#').to_lowercase();
            if !Self::is_valid_twitch_login(&name) {
                return None;
            }
            return Some(Self(name));
        }

        if let Some(rest) = input.strip_prefix("ircs://") {
            return Self::parse_irc_url_like(rest, true);
        }
        if let Some(rest) = input.strip_prefix("irc://") {
            return Self::parse_irc_url_like(rest, false);
        }
        if let Some(rest) = input.strip_prefix("irc:") {
            if let Some(id) = Self::parse_irc_canonical(input) {
                return Some(id);
            }
            return Self::parse_irc_url_like(rest, false);
        }

        let twitch = input.trim_start_matches('#').to_lowercase();
        if !Self::is_valid_twitch_login(&twitch) {
            return None;
        }
        Some(Self(twitch))
    }

    fn parse_irc_url_like(rest: &str, tls: bool) -> Option<Self> {
        let without_slashes = rest.trim_start_matches('/').trim();
        let (host_port, channel_raw) = without_slashes
            .split_once('/')
            .unwrap_or((without_slashes, ""));
        // Strip exactly ONE leading '#' so ##channels are preserved.
        let channel_trimmed = channel_raw.trim();
        let channel = channel_trimmed.strip_prefix('#').unwrap_or(channel_trimmed);

        let default_port = if tls { 6697 } else { 6667 };
        let (host_raw, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
            if let Ok(parsed) = p.trim().parse::<u16>() {
                (h.trim(), parsed)
            } else {
                (host_port.trim(), default_port)
            }
        } else {
            (host_port.trim(), default_port)
        };

        let host = host_raw.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return None;
        }
        if channel.is_empty() {
            Some(Self::irc(host, port, tls, IRC_SERVER_CONTROL_CHANNEL))
        } else {
            Some(Self::irc(host, port, tls, channel))
        }
    }

    fn parse_irc_canonical(input: &str) -> Option<Self> {
        let mut parts = input.splitn(5, ':');
        let prefix = parts.next()?;
        if prefix != "irc" {
            return None;
        }
        let tls_part = parts.next()?;
        if tls_part != "0" && tls_part != "1" {
            return None;
        }
        let host = parts.next()?;
        let port = parts.next()?.parse::<u16>().ok()?;
        let channel = parts.next()?;
        if host.is_empty() || channel.is_empty() {
            return None;
        }
        Some(Self::irc(host, port, tls_part == "1", channel))
    }

    fn is_valid_twitch_login(login: &str) -> bool {
        let len = login.len();
        if !(3..=25).contains(&len) {
            return false;
        }
        login
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    pub fn is_kick(&self) -> bool {
        self.platform() == Platform::Kick
    }

    pub fn is_twitch(&self) -> bool {
        self.platform() == Platform::Twitch
    }

    pub fn is_irc(&self) -> bool {
        self.platform() == Platform::Irc
    }

    pub fn is_irc_server_tab(&self) -> bool {
        self.irc_target()
            .map(|t| t.channel == IRC_SERVER_CONTROL_CHANNEL)
            .unwrap_or(false)
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// Twitch numeric or string user-id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub String);

/// Local monotonic message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub u64);
