use crust_core::notifications::{NotificationController, LiveNotification, OfflineNotification, Platform, WatchedChannel};

/// Stream status tracker that manages live/offline transitions and fires notifications.
#[derive(Debug, Clone)]
pub struct StreamStatusTracker {
    controller: NotificationController,
}

impl StreamStatusTracker {
    pub fn new() -> Self {
        Self {
            controller: NotificationController::new(),
        }
    }

    /// Add a channel to watch for live status.
    pub fn watch_channel(&mut self, channel_name: String, platform: Platform, channel_id: Option<String>) {
        self.controller.add_channel(channel_name, platform, channel_id);
    }

    /// Remove a channel from the watch list.
    pub fn unwatch_channel(&mut self, channel_name: &str, platform: Platform) {
        self.controller.remove_channel(channel_name, platform);
    }

    /// Check if a channel is being watched.
    pub fn is_watching(&self, channel_name: &str, platform: Platform) -> bool {
        self.controller.is_watching(channel_name, platform)
    }

    /// Update a channel's live status. Returns a notification payload if a transition occurred.
    pub fn update_stream_status(
        &mut self,
        channel_name: &str,
        platform: Platform,
        is_live: bool,
        title: Option<String>,
        game: Option<String>,
        viewer_count: Option<u32>,
    ) -> Option<StreamStatusUpdate> {
        if let Some(went_live) = self.controller.update_live_status(channel_name, platform, is_live) {
            if went_live {
                return Some(StreamStatusUpdate::Live(LiveNotification {
                    channel_id: String::new(),
                    channel_name: channel_name.to_owned(),
                    display_name: channel_name.to_owned(),
                    title,
                    game,
                    viewer_count,
                    platform,
                }));
            } else {
                return Some(StreamStatusUpdate::Offline(OfflineNotification {
                    channel_id: String::new(),
                    channel_name: channel_name.to_owned(),
                    platform,
                }));
            }
        }
        None
    }

    /// Get all watched channels for a platform.
    pub fn get_watched_channels(&self, platform: Platform) -> Vec<&WatchedChannel> {
        self.controller.get_watched_for_platform(platform)
    }

    /// Get all Twitch channel IDs for EventSub subscriptions.
    pub fn get_twitch_channel_ids(&self) -> Vec<String> {
        self.controller.get_twitch_channel_ids()
    }

    /// Export watched channels for persistence.
    pub fn export_channels(&self) -> Vec<WatchedChannel> {
        self.controller.export_channels()
    }

    /// Import watched channels from saved data.
    pub fn import_channels(&mut self, channels: Vec<WatchedChannel>) {
        self.controller.import_channels(channels);
    }
}

impl Default for StreamStatusTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Stream status update event.
#[derive(Debug, Clone)]
pub enum StreamStatusUpdate {
    Live(LiveNotification),
    Offline(OfflineNotification),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_and_unwatch_channel() {
        let mut tracker = StreamStatusTracker::new();
        tracker.watch_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        assert!(tracker.is_watching("xqc", Platform::Twitch));

        tracker.unwatch_channel("xqc", Platform::Twitch);
        assert!(!tracker.is_watching("xqc", Platform::Twitch));
    }

    #[test]
    fn update_stream_status_fires_live_notification() {
        let mut tracker = StreamStatusTracker::new();
        tracker.watch_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));

        let update = tracker.update_stream_status(
            "xqc",
            Platform::Twitch,
            true,
            Some("Playing games".to_owned()),
            Some("Just Chatting".to_owned()),
            Some(50000),
        );

        assert!(matches!(update, Some(StreamStatusUpdate::Live(_))));
    }

    #[test]
    fn update_stream_status_fires_offline_notification() {
        let mut tracker = StreamStatusTracker::new();
        tracker.watch_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));

        tracker.update_stream_status("xqc", Platform::Twitch, true, None, None, None);

        let update = tracker.update_stream_status("xqc", Platform::Twitch, false, None, None, None);
        assert!(matches!(update, Some(StreamStatusUpdate::Offline(_))));
    }

    #[test]
    fn no_notification_when_status_unchanged() {
        let mut tracker = StreamStatusTracker::new();
        tracker.watch_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));

        tracker.update_stream_status("xqc", Platform::Twitch, true, None, None, None);

        let update = tracker.update_stream_status("xqc", Platform::Twitch, true, None, None, None);
        assert!(update.is_none());
    }

    #[test]
    fn get_twitch_channel_ids() {
        let mut tracker = StreamStatusTracker::new();
        tracker.watch_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        tracker.watch_channel("forsen".to_owned(), Platform::Twitch, Some("456".to_owned()));
        tracker.watch_channel("kick_user".to_owned(), Platform::Kick, Some("789".to_owned()));

        let ids = tracker.get_twitch_channel_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"123".to_owned()));
        assert!(ids.contains(&"456".to_owned()));
    }

    #[test]
    fn export_and_import_channels() {
        let mut tracker = StreamStatusTracker::new();
        tracker.watch_channel("xqc".to_owned(), Platform::Twitch, Some("123".to_owned()));
        tracker.watch_channel("forsen".to_owned(), Platform::Twitch, Some("456".to_owned()));

        let exported = tracker.export_channels();

        let mut new_tracker = StreamStatusTracker::new();
        new_tracker.import_channels(exported);

        assert!(new_tracker.is_watching("xqc", Platform::Twitch));
        assert!(new_tracker.is_watching("forsen", Platform::Twitch));
    }
}
