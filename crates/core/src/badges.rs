use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Badge information with multiple versions (1x, 2x, 4x image URLs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadgeVersion {
    pub id: String,
    pub image_url_1x: String,
    pub image_url_2x: String,
    pub image_url_4x: String,
    pub title: String,
    pub description: String,
    pub click_action: Option<String>,
    pub click_url: Option<String>,
}

/// A badge set (e.g., "subscriber", "bits", "moderator").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadgeSet {
    pub set_id: String,
    pub versions: HashMap<String, BadgeVersion>,
}

/// Badge manager for global and channel-specific badge sets.
#[derive(Debug, Clone, Default)]
pub struct BadgeController {
    /// Global badges available across all channels.
    global_badges: HashMap<String, BadgeSet>,
    /// Channel-specific badges keyed by channel_id.
    channel_badges: HashMap<String, HashMap<String, BadgeSet>>,
}

impl BadgeController {
    pub fn new() -> Self {
        Self {
            global_badges: HashMap::new(),
            channel_badges: HashMap::new(),
        }
    }

    /// Set global badge sets (from Helix Global Chat Badges API).
    pub fn set_global_badges(&mut self, badges: Vec<BadgeSet>) {
        self.global_badges.clear();
        for set in badges {
            self.global_badges.insert(set.set_id.clone(), set);
        }
    }

    /// Set channel-specific badge sets for a given channel.
    pub fn set_channel_badges(&mut self, channel_id: String, badges: Vec<BadgeSet>) {
        let mut map = HashMap::new();
        for set in badges {
            map.insert(set.set_id.clone(), set);
        }
        self.channel_badges.insert(channel_id, map);
    }

    /// Look up a badge version for a given set_id and version_id.
    /// Checks channel-specific badges first, then falls back to global badges.
    pub fn get_badge(&self, channel_id: Option<&str>, set_id: &str, version_id: &str) -> Option<&BadgeVersion> {
        fn resolve_badge_version<'a>(
            set: &'a BadgeSet,
            version_id: &str,
        ) -> Option<&'a BadgeVersion> {
            if let Some(version) = set.versions.get(version_id) {
                return Some(version);
            }

            if let Some(version) = set.versions.get("1") {
                return Some(version);
            }

            set.versions.values().next()
        }

        if let Some(ch_id) = channel_id {
            if let Some(ch_badges) = self.channel_badges.get(ch_id) {
                if let Some(set) = ch_badges.get(set_id) {
                    if let Some(version) = resolve_badge_version(set, version_id) {
                        return Some(version);
                    }
                }
            }
        }

        self.global_badges
            .get(set_id)
            .and_then(|set| resolve_badge_version(set, version_id))
    }

    /// Parse a badge string like "moderator/1,subscriber/12" into a vec of (set_id, version_id).
    pub fn parse_badge_string(badge_str: &str) -> Vec<(String, String)> {
        if badge_str.trim().is_empty() {
            return Vec::new();
        }

        badge_str
            .split(',')
            .filter_map(|part| {
                let mut split = part.trim().splitn(2, '/');
                let set_id = split.next()?.trim().to_owned();
                let version_id = split.next()?.trim().to_owned();
                if set_id.is_empty() || version_id.is_empty() {
                    return None;
                }
                Some((set_id, version_id))
            })
            .collect()
    }

    /// Resolve a parsed badge list into actual badge versions.
    pub fn resolve_badges(
        &self,
        channel_id: Option<&str>,
        badges: &[(String, String)],
    ) -> Vec<&BadgeVersion> {
        badges
            .iter()
            .filter_map(|(set_id, version_id)| self.get_badge(channel_id, set_id, version_id))
            .collect()
    }

    /// Clear all badge data (useful when switching accounts).
    pub fn clear(&mut self) {
        self.global_badges.clear();
        self.channel_badges.clear();
    }

    /// Remove badge data for a specific channel.
    pub fn clear_channel(&mut self, channel_id: &str) {
        self.channel_badges.remove(channel_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_badge(set_id: &str, version_id: &str, title: &str) -> BadgeVersion {
        BadgeVersion {
            id: version_id.to_owned(),
            image_url_1x: format!("https://example.com/{}/{}_1x.png", set_id, version_id),
            image_url_2x: format!("https://example.com/{}/{}_2x.png", set_id, version_id),
            image_url_4x: format!("https://example.com/{}/{}_4x.png", set_id, version_id),
            title: title.to_owned(),
            description: format!("{} badge", title),
            click_action: None,
            click_url: None,
        }
    }

    fn make_test_badge_set(set_id: &str, versions: Vec<(&str, &str)>) -> BadgeSet {
        let mut version_map = HashMap::new();
        for (version_id, title) in versions {
            version_map.insert(
                version_id.to_owned(),
                make_test_badge(set_id, version_id, title),
            );
        }
        BadgeSet {
            set_id: set_id.to_owned(),
            versions: version_map,
        }
    }

    #[test]
    fn get_global_badge() {
        let mut controller = BadgeController::new();
        controller.set_global_badges(vec![make_test_badge_set("moderator", vec![("1", "Moderator")])]);

        let badge = controller.get_badge(None, "moderator", "1");
        assert!(badge.is_some());
        assert_eq!(badge.unwrap().title, "Moderator");
    }

    #[test]
    fn get_channel_badge_overrides_global() {
        let mut controller = BadgeController::new();
        controller.set_global_badges(vec![make_test_badge_set("subscriber", vec![("0", "Sub")])]);
        controller.set_channel_badges(
            "123".to_owned(),
            vec![make_test_badge_set("subscriber", vec![("12", "12-Month Sub")])],
        );

        let badge = controller.get_badge(Some("123"), "subscriber", "12");
        assert!(badge.is_some());
        assert_eq!(badge.unwrap().title, "12-Month Sub");
    }

    #[test]
    fn parse_badge_string_multiple_badges() {
        let parsed = BadgeController::parse_badge_string("moderator/1,subscriber/12,vip/1");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], ("moderator".to_owned(), "1".to_owned()));
        assert_eq!(parsed[1], ("subscriber".to_owned(), "12".to_owned()));
        assert_eq!(parsed[2], ("vip".to_owned(), "1".to_owned()));
    }

    #[test]
    fn parse_badge_string_empty() {
        let parsed = BadgeController::parse_badge_string("");
        assert!(parsed.is_empty());
    }

    #[test]
    fn resolve_badges_returns_correct_versions() {
        let mut controller = BadgeController::new();
        controller.set_global_badges(vec![
            make_test_badge_set("moderator", vec![("1", "Moderator")]),
            make_test_badge_set("vip", vec![("1", "VIP")]),
        ]);

        let badges = vec![
            ("moderator".to_owned(), "1".to_owned()),
            ("vip".to_owned(), "1".to_owned()),
        ];
        let resolved = controller.resolve_badges(None, &badges);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].title, "Moderator");
        assert_eq!(resolved[1].title, "VIP");
    }

    #[test]
    fn clear_removes_all_badges() {
        let mut controller = BadgeController::new();
        controller.set_global_badges(vec![make_test_badge_set("moderator", vec![("1", "Moderator")])]);
        controller.set_channel_badges(
            "123".to_owned(),
            vec![make_test_badge_set("subscriber", vec![("12", "12-Month Sub")])],
        );

        controller.clear();

        assert!(controller.get_badge(None, "moderator", "1").is_none());
        assert!(controller.get_badge(Some("123"), "subscriber", "12").is_none());
    }

    #[test]
    fn clear_channel_removes_only_channel_badges() {
        let mut controller = BadgeController::new();
        controller.set_global_badges(vec![make_test_badge_set("moderator", vec![("1", "Moderator")])]);
        controller.set_channel_badges(
            "123".to_owned(),
            vec![make_test_badge_set("subscriber", vec![("12", "12-Month Sub")])],
        );

        controller.clear_channel("123");

        assert!(controller.get_badge(None, "moderator", "1").is_some());
        assert!(controller.get_badge(Some("123"), "subscriber", "12").is_none());
    }

    #[test]
    fn get_badge_falls_back_to_default_version_when_exact_missing() {
        let mut controller = BadgeController::new();
        controller.set_global_badges(vec![make_test_badge_set(
            "subscriber",
            vec![("1", "Sub 1")],
        )]);

        let badge = controller
            .get_badge(None, "subscriber", "12")
            .expect("fallback badge expected");
        assert_eq!(badge.id, "1");
    }
}
