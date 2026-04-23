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

/// Reserved `ChannelId` for the aggregated "Live" pseudo-tab.
///
/// Twitch logins cannot start with `_`, and the sentinel survives
/// `ChannelId::new` (lowercase + strip leading `#`) unchanged, so it cannot
/// collide with a real channel.
pub const LIVE_FEED_CHANNEL: &str = "__live_feed__";

/// Reserved `ChannelId` for the cross-channel "Mentions" pseudo-tab.
///
/// Same rationale as [`LIVE_FEED_CHANNEL`]: leading `_` means it cannot
/// collide with any real Twitch login, and it round-trips through
/// `ChannelId::new` unchanged.
pub const MENTIONS_CHANNEL: &str = "__mentions__";

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
        if login.starts_with('_') {
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

    /// Sentinel ChannelId for the Live feed pseudo-tab.
    pub fn live_feed() -> Self {
        Self(LIVE_FEED_CHANNEL.to_owned())
    }

    /// True iff this id is the Live feed sentinel.
    pub fn is_live_feed(&self) -> bool {
        self.0 == LIVE_FEED_CHANNEL
    }

    /// Sentinel ChannelId for the cross-channel Mentions pseudo-tab.
    pub fn mentions() -> Self {
        Self(MENTIONS_CHANNEL.to_owned())
    }

    /// True iff this id is the Mentions sentinel.
    pub fn is_mentions(&self) -> bool {
        self.0 == MENTIONS_CHANNEL
    }

    /// True iff this id is any of the synthetic pseudo-tabs (Live / Mentions).
    /// Useful for code paths that need to skip real-channel logic (joins,
    /// persistence, chat input, etc.) for all virtual tabs at once.
    pub fn is_virtual(&self) -> bool {
        self.is_live_feed() || self.is_mentions()
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

#[cfg(test)]
mod live_feed_sentinel_tests {
    use super::*;

    #[test]
    fn sentinel_constant_value() {
        assert_eq!(LIVE_FEED_CHANNEL, "__live_feed__");
    }

    #[test]
    fn sentinel_id_round_trips_through_new() {
        // ChannelId::new lowercases + strips '#'. The sentinel survives both.
        let id = ChannelId::new(LIVE_FEED_CHANNEL);
        assert_eq!(id.0, LIVE_FEED_CHANNEL);
    }

    #[test]
    fn live_feed_id_helper_returns_sentinel() {
        assert_eq!(ChannelId::live_feed().0, LIVE_FEED_CHANNEL);
    }

    #[test]
    fn is_live_feed_true_only_for_sentinel() {
        assert!(ChannelId::live_feed().is_live_feed());
        assert!(!ChannelId::new("forsen").is_live_feed());
        assert!(!ChannelId::new("__server__").is_live_feed());
    }

    #[test]
    fn live_feed_sentinel_is_not_a_valid_twitch_login() {
        assert!(ChannelId::is_valid_twitch_login("forsen"));
        assert!(!ChannelId::is_valid_twitch_login(LIVE_FEED_CHANNEL));
        assert_eq!(ChannelId::parse_user_input(LIVE_FEED_CHANNEL), None);
    }

    #[test]
    fn mentions_sentinel_round_trips_and_is_distinct_from_live_feed() {
        let m = ChannelId::mentions();
        assert_eq!(m.0, MENTIONS_CHANNEL);
        assert!(m.is_mentions());
        assert!(!m.is_live_feed());
        assert!(m.is_virtual());
        // Leading `_` blocks it from ever being parsed as a real login.
        assert!(!ChannelId::is_valid_twitch_login(MENTIONS_CHANNEL));
        assert_eq!(ChannelId::parse_user_input(MENTIONS_CHANNEL), None);
        // And survives the same normalisation path as `new()`.
        assert_eq!(ChannelId::new(MENTIONS_CHANNEL).0, MENTIONS_CHANNEL);
    }

    #[test]
    fn mentions_and_live_feed_sentinels_do_not_collide() {
        assert_ne!(ChannelId::mentions(), ChannelId::live_feed());
        assert!(!ChannelId::mentions().is_live_feed());
        assert!(!ChannelId::live_feed().is_mentions());
    }
}
