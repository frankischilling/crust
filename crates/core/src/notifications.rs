use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Platform identifier for cross-platform notification support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    Twitch,
    Kick,
}

/// A channel being watched for live status notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedChannel {
    pub channel_name: String,
    pub platform: Platform,
    /// Channel ID (broadcaster_user_id for Twitch).
    pub channel_id: Option<String>,
    /// Last known live status to detect transitions.
    #[serde(default)]
    pub was_live: bool,
}

/// Controller for managing watched channels and stream status notifications.
#[derive(Debug, Clone, Default)]
pub struct NotificationController {
    watched_channels: HashMap<(String, Platform), WatchedChannel>,
}

impl NotificationController {
    pub fn new() -> Self {
        Self {
            watched_channels: HashMap::new(),
        }
    }

    /// Add a channel to the watch list.
    pub fn add_channel(
        &mut self,
        channel_name: String,
        platform: Platform,
        channel_id: Option<String>,
    ) {
        let key = (channel_name.to_lowercase(), platform);
        self.watched_channels.insert(
            key.clone(),
            WatchedChannel {
                channel_name: channel_name.clone(),
                platform,
                channel_id,
                was_live: false,
            },
        );
    }

    /// Remove a channel from the watch list.
    pub fn remove_channel(&mut self, channel_name: &str, platform: Platform) {
        let key = (channel_name.to_lowercase(), platform);
        self.watched_channels.remove(&key);
    }

    /// Check if a channel is being watched.
    pub fn is_watching(&self, channel_name: &str, platform: Platform) -> bool {
        let key = (channel_name.to_lowercase(), platform);
        self.watched_channels.contains_key(&key)
    }

    /// Update a channel's live status and return `Some(true)` if it just went live,
    /// `Some(false)` if it just went offline, or `None` if no transition occurred.
    pub fn update_live_status(
        &mut self,
        channel_name: &str,
        platform: Platform,
        is_live: bool,
    ) -> Option<bool> {
        let key = (channel_name.to_lowercase(), platform);
        if let Some(watched) = self.watched_channels.get_mut(&key) {
            if watched.was_live != is_live {
                watched.was_live = is_live;
                return Some(is_live);
            }
        }
        None
    }

    /// Get all watched channels for a specific platform.
    pub fn get_watched_for_platform(&self, platform: Platform) -> Vec<&WatchedChannel> {
        self.watched_channels
            .values()
            .filter(|ch| ch.platform == platform)
            .collect()
    }

    /// Get all watched Twitch channel IDs (broadcaster_user_id) for EventSub subscriptions.
    pub fn get_twitch_channel_ids(&self) -> Vec<String> {
        self.watched_channels
            .values()
            .filter(|ch| ch.platform == Platform::Twitch)
            .filter_map(|ch| ch.channel_id.clone())
            .collect()
    }

    /// Export watched channels for serialization.
    pub fn export_channels(&self) -> Vec<WatchedChannel> {
        self.watched_channels.values().cloned().collect()
    }

    /// Import watched channels from deserialized data.
    pub fn import_channels(&mut self, channels: Vec<WatchedChannel>) {
        self.watched_channels.clear();
        for ch in channels {
            let key = (ch.channel_name.to_lowercase(), ch.platform);
            self.watched_channels.insert(key, ch);
        }
    }
}

/// Notification payload for a channel going live.
#[derive(Debug, Clone)]
pub struct LiveNotification {
    pub channel_id: String,
    pub channel_name: String,
    pub display_name: String,
    pub title: Option<String>,
    pub game: Option<String>,
    pub viewer_count: Option<u32>,
    pub platform: Platform,
}

/// Notification payload for a channel going offline.
#[derive(Debug, Clone)]
pub struct OfflineNotification {
    pub channel_id: String,
    pub channel_name: String,
    pub platform: Platform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_check_watched_channel() {
        let mut controller = NotificationController::new();
        controller.add_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        assert!(controller.is_watching("xqc", Platform::Twitch));
        assert!(!controller.is_watching("forsen", Platform::Twitch));
    }

    #[test]
    fn remove_watched_channel() {
        let mut controller = NotificationController::new();
        controller.add_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        controller.remove_channel("xqc", Platform::Twitch);
        assert!(!controller.is_watching("xqc", Platform::Twitch));
    }

    #[test]
    fn update_live_status_detects_transition_to_live() {
        let mut controller = NotificationController::new();
        controller.add_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));

        let transition = controller.update_live_status("xqc", Platform::Twitch, true);
        assert_eq!(transition, Some(true));
    }

    #[test]
    fn update_live_status_no_transition_when_already_live() {
        let mut controller = NotificationController::new();
        controller.add_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        controller.update_live_status("xqc", Platform::Twitch, true);

        let transition = controller.update_live_status("xqc", Platform::Twitch, true);
        assert_eq!(transition, None);
    }

    #[test]
    fn get_twitch_channel_ids_returns_all_ids() {
        let mut controller = NotificationController::new();
        controller.add_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        controller.add_channel(
            "forsen".to_owned(),
            Platform::Twitch,
            Some("456".to_owned()),
        );
        controller.add_channel(
            "kick_streamer".to_owned(),
            Platform::Kick,
            Some("789".to_owned()),
        );

        let ids = controller.get_twitch_channel_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"123".to_owned()));
        assert!(ids.contains(&"456".to_owned()));
    }

    #[test]
    fn export_and_import_preserves_watched_channels() {
        let mut controller = NotificationController::new();
        controller.add_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        controller.add_channel(
            "forsen".to_owned(),
            Platform::Twitch,
            Some("456".to_owned()),
        );

        let exported = controller.export_channels();

        let mut new_controller = NotificationController::new();
        new_controller.import_channels(exported);

        assert!(new_controller.is_watching("xqc", Platform::Twitch));
        assert!(new_controller.is_watching("forsen", Platform::Twitch));
    }
}
