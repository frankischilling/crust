use serde::{Deserialize, Serialize};

/// A single live followed-channel as displayed in the Live feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveChannelSnapshot {
    /// Twitch broadcaster id.
    pub user_id: String,
    /// Lowercase login (used to construct ChannelId on click).
    pub user_login: String,
    /// Display name (preserves capitalisation / Unicode).
    pub user_name: String,
    /// Current viewer count from Helix.
    pub viewer_count: u32,
    /// Resolved thumbnail URL with `{width}` / `{height}` already
    /// substituted. Use [`Self::template_thumbnail`] to resolve a raw
    /// Helix URL before constructing this struct.
    pub thumbnail_url: String,
    /// RFC 3339 stream start timestamp. Sorted lexicographically as a
    /// tiebreaker for entries with equal `viewer_count` (RFC 3339 strings
    /// sort in chronological order).
    pub started_at: String,
}

impl LiveChannelSnapshot {
    /// Substitute `{width}` / `{height}` in a raw Helix template URL.
    pub fn template_thumbnail(raw: &str, width: u32, height: u32) -> String {
        raw.replace("{width}", &width.to_string())
            .replace("{height}", &height.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_thumbnail_substitutes_dimensions() {
        let raw = "https://x/cdn/{width}x{height}.jpg";
        let out = LiveChannelSnapshot::template_thumbnail(raw, 320, 180);
        assert_eq!(out, "https://x/cdn/320x180.jpg");
    }

    #[test]
    fn template_thumbnail_with_no_placeholders_is_unchanged() {
        let raw = "https://x/cdn/static.jpg";
        let out = LiveChannelSnapshot::template_thumbnail(raw, 320, 180);
        assert_eq!(out, raw);
    }
}
