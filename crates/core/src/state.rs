use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::{
    events::ConnectionState,
    model::{ChannelId, ChannelState, ChatMessage, LiveChannelSnapshot},
};

/// Per-tab visibility rule. Mirrors Chatterino's right-click "hide when
/// offline" / "hide muted" tab options: lets users declutter the tab strip
/// for channels they only care about sometimes.
///
/// The rule is evaluated at render time against live runtime state (e.g. the
/// current live/offline status for Twitch channels); the tab reappears the
/// moment the state flips back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TabVisibilityRule {
    /// Always show the tab (default when no rule is set).
    Always,
    /// Hide the tab whenever the channel is offline. Twitch-only
    /// semantics; for non-Twitch channels this behaves as [`Self::Always`].
    HideWhenOffline,
}

impl Default for TabVisibilityRule {
    fn default() -> Self {
        Self::Always
    }
}

impl TabVisibilityRule {
    /// Serialize the rule as a short identifier suitable for
    /// configuration files.
    pub fn as_key(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::HideWhenOffline => "hide_when_offline",
        }
    }

    /// Parse a config-file identifier back into a rule. Unknown strings
    /// fall back to [`Self::Always`] so stale/partial configs never panic.
    pub fn from_key(key: &str) -> Self {
        match key {
            "hide_when_offline" => Self::HideWhenOffline,
            _ => Self::Always,
        }
    }
}

/// Cap on the cross-channel Mentions buffer. Messages older than this window
/// are dropped oldest-first so the Mentions tab never grows unbounded on
/// long-running sessions.
pub const MENTIONS_BUFFER_CAP: usize = 2_000;

/// In-flight Twitch hype train state for a single channel. Populated from
/// EventSub `channel.hype_train.*` notifications so the UI can render a
/// live banner (see C4 in `CHATTERINO_PARITY_TODO.md`).
///
/// Cleared after the hype train ends and its cooldown elapses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HypeTrainState {
    /// "begin", "progress", or "end".
    pub phase: String,
    /// Current level (1..=5, 0 when unknown).
    pub level: u32,
    /// Points accumulated towards the next level.
    pub progress: u64,
    /// Points needed to reach the next level (0 on end events).
    pub goal: u64,
    /// Total points accumulated across the whole train.
    pub total: u64,
    /// Display/login of the top contributor during the active train.
    pub top_contributor_login: Option<String>,
    /// Contribution channel: "bits" / "subscription" / other.
    pub top_contributor_type: Option<String>,
    /// Contribution total from the top contributor.
    pub top_contributor_total: Option<u64>,
    /// ISO 8601 time this phase ends (begin/progress) or cooldown ends (end).
    pub ends_at: Option<String>,
    /// Local instant the state was last applied; used for banner cooldown.
    pub updated_at: Instant,
}

/// Active raid banner state for a single channel. Kept separately from the
/// chat log so the UI can show and dismiss the banner independently of
/// message history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaidBannerState {
    /// Display name for the raiding channel or streamer.
    pub display_name: String,
    /// Viewer count reported with the raid.
    pub viewer_count: u32,
    /// Optional source login for linking back to the raider.
    pub source_login: Option<String>,
    /// Local instant the banner was shown.
    pub shown_at: Instant,
    /// Whether the banner was explicitly dismissed.
    pub dismissed: bool,
}

/// Authentication state.
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    /// Whether the user is logged in with an OAuth token.
    pub logged_in: bool,
    /// The authenticated username (display name).
    pub username: Option<String>,
    /// The Twitch user-id.
    pub user_id: Option<String>,
    /// CDN URL for the user's avatar image.
    pub avatar_url: Option<String>,
}

/// The single source of truth for the whole application.
#[derive(Debug)]
pub struct AppState {
    pub connection: ConnectionState,
    pub auth: AuthState,
    pub channels: HashMap<ChannelId, ChannelState>,
    /// The currently-visible channel tab.
    pub active_channel: Option<ChannelId>,
    /// Ordered list so tabs render in a stable order.
    pub channel_order: Vec<ChannelId>,
    /// All saved account usernames (used by the account switcher UI).
    pub accounts: Vec<String>,
    /// Currently-live followed channels, sorted desc by viewer_count.
    /// Replaced wholesale on each snapshot from the live-feed task.
    pub live_channels: Vec<LiveChannelSnapshot>,
    /// `false` until the first snapshot OR error arrives. Used by the UI to
    /// distinguish "loading" from "loaded but empty / loaded with error".
    pub live_feed_loaded: bool,
    /// Human-readable last error from the live-feed task (or `None`).
    pub live_feed_error: Option<String>,
    /// Monotonic instant of the last successful snapshot (or `None`).
    /// Use `.elapsed()` to render "last updated Xs ago" - do NOT compare
    /// to `SystemTime::now()`.
    pub live_feed_last_updated: Option<Instant>,
    /// Cross-channel buffer of highlight-matching messages (mentions, keyword
    /// highlights, first-time-chatter messages, pinned messages). Oldest at
    /// the front. Capped at [`MENTIONS_BUFFER_CAP`]; the Mentions pseudo-tab
    /// renders this directly, so ordering + channel attribution must be
    /// preserved as-received.
    pub mentions: VecDeque<ChatMessage>,
    /// Unread count for the Mentions tab (incremented by new mentions while
    /// the Mentions tab is not active). Cleared on activation.
    pub mentions_unread: u32,
    /// Per-channel tab visibility rules. Only entries with a non-default
    /// rule are stored; absence means [`TabVisibilityRule::Always`].
    /// Persisted across sessions via `AppSettings::tab_visibility_rules`.
    pub tab_visibility_rules: HashMap<ChannelId, TabVisibilityRule>,
    /// Current hype-train state per channel, keyed by channel id. Populated
    /// from EventSub `channel.hype_train.*` notifications. The entry is
    /// cleared once the train's cooldown elapses (see
    /// [`AppState::expire_stale_hype_trains`]).
    pub hype_trains: HashMap<ChannelId, HypeTrainState>,
    /// Current raid banner state per channel, keyed by channel id. Dismissal
    /// marks the banner as dismissed; the next expiry sweep removes dismissed
    /// or stale entries.
    pub raid_banners: HashMap<ChannelId, RaidBannerState>,
    /// Resolved Shared Chat source-channel metadata keyed by Twitch user id
    /// (`source-room-id` IRC tag). Populated by background Helix/IVR lookups
    /// that fire the first time the app sees a PRIVMSG mirrored from a
    /// channel it does not already have open. The profile picture URL is
    /// also pre-fetched into `emote_bytes` so the shared-chat badge chip
    /// paints without a second frame.
    pub shared_channel_profiles: HashMap<String, SharedChannelProfile>,
    /// Active Shared Chat session per channel keyed by `ChannelId`.
    /// Populated by `/helix/shared_chat/session` lookups; refreshed
    /// periodically so the viewer-count total tracks reality.
    pub shared_chat_sessions: HashMap<ChannelId, SharedChatSessionState>,
}

/// Live Shared Chat session metadata used by the viewer-total banner.
/// Combines `/helix/shared_chat/session` (participants) with
/// `/helix/streams` (per-participant live status + viewer count).
#[derive(Debug, Clone)]
pub struct SharedChatSessionState {
    pub session_id: String,
    pub host_broadcaster_id: String,
    pub participants: Vec<SharedChatParticipant>,
    /// Monotonic timestamp of the last successful refresh. Used to
    /// throttle background polling (don't re-hit Helix more than once
    /// per refresh interval).
    pub updated_at: Instant,
}

impl SharedChatSessionState {
    /// Sum of viewer counts across all live participants. Offline
    /// participants contribute 0.
    pub fn total_viewers(&self) -> u64 {
        self.participants
            .iter()
            .filter(|p| p.live)
            .map(|p| p.viewer_count)
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct SharedChatParticipant {
    /// Twitch broadcaster user id.
    pub broadcaster_id: String,
    /// Lowercase login.
    pub login: String,
    /// Display name (preserves unicode + capitalisation).
    pub display_name: String,
    /// Profile image URL, when resolved via Helix /users.
    pub profile_url: Option<String>,
    /// Current viewer count. `0` when offline.
    pub viewer_count: u64,
    /// Whether the participant is currently streaming.
    pub live: bool,
}

/// Resolved metadata for a Shared Chat source channel. Shown on the
/// shared-chat badge chip (matches Chatterino's `makeSharedChatBadge`).
#[derive(Debug, Clone)]
pub struct SharedChannelProfile {
    pub login: String,
    pub display_name: String,
    pub profile_url: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            auth: AuthState::default(),
            channels: HashMap::new(),
            active_channel: None,
            channel_order: Vec::new(),
            accounts: Vec::new(),
            live_channels: Vec::new(),
            live_feed_loaded: false,
            live_feed_error: None,
            live_feed_last_updated: None,
            mentions: VecDeque::new(),
            mentions_unread: 0,
            tab_visibility_rules: HashMap::new(),
            hype_trains: HashMap::new(),
            raid_banners: HashMap::new(),
            shared_channel_profiles: HashMap::new(),
            shared_chat_sessions: HashMap::new(),
        }
    }
}

impl AppState {
    pub fn join_channel(&mut self, id: ChannelId) {
        if !self.channels.contains_key(&id) {
            self.channels
                .insert(id.clone(), ChannelState::new(id.clone()));
            self.channel_order.push(id.clone());
        }
        if self.active_channel.is_none() {
            self.active_channel = Some(id);
        }
    }

    pub fn leave_channel(&mut self, id: &ChannelId) {
        self.channels.remove(id);
        self.channel_order.retain(|c| c != id);
        self.hype_trains.remove(id);
        self.raid_banners.remove(id);
        if self.active_channel.as_ref() == Some(id) {
            self.active_channel = self.channel_order.first().cloned();
        }
    }

    /// Redirect a channel (e.g. IRC 470 forward).
    /// Moves the existing channel state to the new id, preserving messages
    /// and tab position.
    pub fn redirect_channel(&mut self, old: &ChannelId, new: &ChannelId) {
        if let Some(mut ch_state) = self.channels.remove(old) {
            ch_state.id = new.clone();
            self.channels.insert(new.clone(), ch_state);
        }
        // Update channel order in-place to preserve tab position.
        if let Some(pos) = self.channel_order.iter().position(|c| c == old) {
            self.channel_order[pos] = new.clone();
        }
        if self.active_channel.as_ref() == Some(old) {
            self.active_channel = Some(new.clone());
        }
    }

    pub fn active_state(&self) -> Option<&ChannelState> {
        self.active_channel
            .as_ref()
            .and_then(|id| self.channels.get(id))
    }

    pub fn active_state_mut(&mut self) -> Option<&mut ChannelState> {
        self.active_channel
            .as_ref()
            .and_then(|id| self.channels.get_mut(id))
    }

    /// Apply the latest hype train update for `channel`. Overwrites the
    /// previous state for that channel. Set the "end" phase to leave the
    /// banner visible for a brief cooldown; callers should invoke
    /// [`Self::expire_stale_hype_trains`] on a timer to drop stale entries.
    pub fn apply_hype_train_update(&mut self, channel: ChannelId, state: HypeTrainState) {
        self.hype_trains.insert(channel, state);
    }

    /// Evict hype-train entries whose "end" phase has been stale for more
    /// than `cooldown`. Call this from the UI render loop so the banner
    /// hides after a natural cooldown even if Twitch never delivers a
    /// follow-up event.
    pub fn expire_stale_hype_trains(&mut self, now: Instant, cooldown: std::time::Duration) {
        self.hype_trains.retain(|_, st| {
            if st.phase != "end" {
                return true;
            }
            now.saturating_duration_since(st.updated_at) < cooldown
        });
    }

    /// Fetch the active hype-train banner state for `channel`, if any.
    pub fn hype_train_for(&self, channel: &ChannelId) -> Option<&HypeTrainState> {
        self.hype_trains.get(channel)
    }

    /// Show or replace the raid banner for `channel`.
    pub fn show_raid_banner(&mut self, channel: ChannelId, banner: RaidBannerState) {
        self.raid_banners.insert(channel, banner);
    }

    /// Dismiss the raid banner for `channel`.
    pub fn dismiss_raid_banner(&mut self, channel: &ChannelId) {
        if let Some(banner) = self.raid_banners.get_mut(channel) {
            banner.dismissed = true;
        }
    }

    /// Evict banners that were already dismissed or have exceeded `ttl`.
    pub fn expire_stale_raid_banners(&mut self, now: Instant, ttl: std::time::Duration) {
        self.raid_banners.retain(|_, banner| {
            !banner.dismissed && now.saturating_duration_since(banner.shown_at) < ttl
        });
    }

    /// Fetch the current raid banner for `channel`, if any.
    pub fn raid_banner_for(&self, channel: &ChannelId) -> Option<&RaidBannerState> {
        self.raid_banners.get(channel).filter(|banner| !banner.dismissed)
    }

    /// Replace the live snapshot wholesale and mark loaded. Clears any
    /// previous error since a successful snapshot supersedes it.
    pub fn apply_live_snapshot(&mut self, snapshots: Vec<LiveChannelSnapshot>) {
        self.live_channels = snapshots;
        self.live_feed_loaded = true;
        self.live_feed_error = None;
        self.live_feed_last_updated = Some(Instant::now());
    }

    /// Record a live-feed error. Keeps the last good snapshot visible. Marks
    /// the feed as loaded so the UI exits its loading state and shows the
    /// error banner instead of an indefinite spinner.
    pub fn apply_live_error(&mut self, message: String) {
        self.live_feed_error = Some(message);
        self.live_feed_loaded = true;
    }

    /// Atomic partial update: replace snapshot AND set error in one step, so
    /// the UI never sees an intermediate "snapshot applied, error cleared"
    /// frame.
    pub fn apply_live_partial(&mut self, snapshots: Vec<LiveChannelSnapshot>, error: String) {
        self.live_channels = snapshots;
        self.live_feed_loaded = true;
        self.live_feed_error = Some(error);
        self.live_feed_last_updated = Some(Instant::now());
    }

    /// Append a mention-matching message to the cross-channel mentions buffer.
    /// Enforces the [`MENTIONS_BUFFER_CAP`] ring-buffer cap. Increments the
    /// Mentions-tab unread counter iff `bump_unread` is true (caller decides
    /// based on whether the Mentions tab is currently the active tab).
    ///
    /// The caller is responsible for checking that the message qualifies as
    /// a mention - this function does no filtering. Messages loaded from
    /// history (`flags.is_history == true`) are accepted so restart
    /// persistence can backfill the buffer without bumping unread.
    pub fn push_mention(&mut self, msg: ChatMessage, bump_unread: bool) {
        // Dedupe: if we already have this server_id in the buffer (history
        // replay racing a live echo, for example) skip to avoid duplicates.
        if let Some(sid) = msg.server_id.as_deref() {
            if self
                .mentions
                .iter()
                .any(|m| m.server_id.as_deref() == Some(sid))
            {
                return;
            }
        }
        if self.mentions.len() >= MENTIONS_BUFFER_CAP {
            self.mentions.pop_front();
        }
        self.mentions.push_back(msg);
        if bump_unread {
            self.mentions_unread = self.mentions_unread.saturating_add(1);
        }
    }

    /// Clear the Mentions-tab unread counter. Call when the user activates
    /// the Mentions tab.
    pub fn clear_mentions_unread(&mut self) {
        self.mentions_unread = 0;
    }

    /// Get the visibility rule for `id`. Returns [`TabVisibilityRule::Always`]
    /// when no explicit rule has been configured.
    pub fn tab_visibility_rule(&self, id: &ChannelId) -> TabVisibilityRule {
        self.tab_visibility_rules
            .get(id)
            .copied()
            .unwrap_or(TabVisibilityRule::Always)
    }

    /// Set (or clear, when `rule` is [`TabVisibilityRule::Always`]) the
    /// visibility rule for `id`. Returns the previous rule so callers can
    /// detect no-op writes.
    pub fn set_tab_visibility_rule(
        &mut self,
        id: ChannelId,
        rule: TabVisibilityRule,
    ) -> TabVisibilityRule {
        let prev = self.tab_visibility_rule(&id);
        match rule {
            TabVisibilityRule::Always => {
                self.tab_visibility_rules.remove(&id);
            }
            other => {
                self.tab_visibility_rules.insert(id, other);
            }
        }
        prev
    }

    /// Bulk-replace the visibility rule map (used when rehydrating from
    /// persistent settings at startup).
    pub fn replace_tab_visibility_rules(
        &mut self,
        rules: HashMap<ChannelId, TabVisibilityRule>,
    ) {
        // Drop any `Always` entries callers may have passed in; absence is
        // the canonical representation of the default.
        self.tab_visibility_rules = rules
            .into_iter()
            .filter(|(_, r)| *r != TabVisibilityRule::Always)
            .collect();
    }

    /// Decide whether a channel's tab should be hidden given its current
    /// live state. `is_live` should be `Some(true)` for channels known to
    /// be live, `Some(false)` for known-offline, and `None` when the
    /// live state is unknown (e.g. non-Twitch channels or before the
    /// first status fetch arrives).
    ///
    /// The active tab is never reported as hidden so the user never loses
    /// sight of the pane they are currently viewing.
    pub fn is_tab_hidden(&self, id: &ChannelId, is_live: Option<bool>) -> bool {
        if self.active_channel.as_ref() == Some(id) {
            return false;
        }
        match self.tab_visibility_rule(id) {
            TabVisibilityRule::Always => false,
            TabVisibilityRule::HideWhenOffline => matches!(is_live, Some(false)),
        }
    }

    /// Prepend historical mentions (from SQLite) into the buffer. Duplicates
    /// are skipped by `server_id`. After the merge the buffer is sorted by
    /// timestamp ascending so restart-loaded rows interleave correctly with
    /// any already-accumulated live rows.
    pub fn prepend_mentions_history(&mut self, mut msgs: Vec<ChatMessage>) {
        let existing_ids: std::collections::HashSet<&str> = self
            .mentions
            .iter()
            .filter_map(|m| m.server_id.as_deref())
            .collect();
        msgs.retain(|m| {
            m.server_id
                .as_deref()
                .map(|id| !existing_ids.contains(id))
                .unwrap_or(true)
        });
        if msgs.is_empty() {
            return;
        }
        let total_cap = (msgs.len() + self.mentions.len()).min(MENTIONS_BUFFER_CAP);
        let mut merged: Vec<ChatMessage> =
            Vec::with_capacity(msgs.len() + self.mentions.len());
        merged.extend(self.mentions.drain(..));
        merged.extend(msgs);
        merged.sort_by_key(|m| m.timestamp);
        if merged.len() > MENTIONS_BUFFER_CAP {
            let excess = merged.len() - MENTIONS_BUFFER_CAP;
            merged.drain(0..excess);
        }
        let mut new_deque = VecDeque::with_capacity(total_cap);
        new_deque.extend(merged);
        self.mentions = new_deque;
    }
}

#[cfg(test)]
mod live_feed_state_tests {
    use super::*;
    use crate::model::LiveChannelSnapshot;

    fn snap(login: &str, viewers: u32) -> LiveChannelSnapshot {
        LiveChannelSnapshot {
            user_id: format!("id-{login}"),
            user_login: login.to_owned(),
            user_name: login.to_owned(),
            viewer_count: viewers,
            thumbnail_url: String::new(),
            started_at: String::new(),
        }
    }

    #[test]
    fn default_has_empty_live_feed() {
        let s = AppState::default();
        assert!(s.live_channels.is_empty());
        assert!(!s.live_feed_loaded);
        assert!(s.live_feed_error.is_none());
        assert!(s.live_feed_last_updated.is_none());
    }

    #[test]
    fn apply_snapshot_replaces_state_and_marks_loaded() {
        let mut s = AppState::default();
        s.live_feed_error = Some("old error".to_owned());
        s.apply_live_snapshot(vec![snap("a", 10), snap("b", 5)]);
        assert_eq!(s.live_channels.len(), 2);
        assert!(s.live_feed_loaded);
        assert!(
            s.live_feed_error.is_none(),
            "snapshot must clear stale error"
        );
        assert!(s.live_feed_last_updated.is_some());
    }

    #[test]
    fn apply_error_sets_error_without_clearing_snapshot() {
        let mut s = AppState::default();
        s.apply_live_snapshot(vec![snap("a", 10)]);
        s.apply_live_error("boom".to_owned());
        assert_eq!(s.live_channels.len(), 1, "last good snapshot stays");
        assert_eq!(s.live_feed_error.as_deref(), Some("boom"));
    }

    #[test]
    fn apply_error_marks_loaded_so_ui_exits_loading_state() {
        let mut s = AppState::default();
        assert!(!s.live_feed_loaded);
        s.apply_live_error("boom".to_owned());
        assert!(
            s.live_feed_loaded,
            "error path must mark loaded so UI stops spinning"
        );
    }
}

#[cfg(test)]
mod raid_banner_state_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn raid_banner_lifecycle_show_dismiss_and_expire() {
        let channel = ChannelId::new("rustlang");
        let mut state = AppState::default();
        let now = Instant::now();

        state.show_raid_banner(
            channel.clone(),
            RaidBannerState {
                display_name: "Raider".into(),
                viewer_count: 42,
                source_login: Some("raider".into()),
                shown_at: now,
                dismissed: false,
            },
        );
        assert_eq!(state.raid_banner_for(&channel).unwrap().viewer_count, 42);

        state.dismiss_raid_banner(&channel);
        assert!(state.raid_banner_for(&channel).is_none());
        assert!(state
            .raid_banners
            .get(&channel)
            .is_some_and(|banner| banner.dismissed));

        state.show_raid_banner(
            channel.clone(),
            RaidBannerState {
                display_name: "Raider".into(),
                viewer_count: 99,
                source_login: Some("raider".into()),
                shown_at: now - Duration::from_secs(21),
                dismissed: false,
            },
        );
        state.expire_stale_raid_banners(now, Duration::from_secs(20));
        assert!(state.raid_banner_for(&channel).is_none());
        assert!(state.raid_banners.get(&channel).is_none());
    }

    #[test]
    fn leave_channel_removes_raid_banner() {
        let channel = ChannelId::new("rustlang");
        let mut state = AppState::default();

        state.show_raid_banner(
            channel.clone(),
            RaidBannerState {
                display_name: "Raider".into(),
                viewer_count: 42,
                source_login: Some("raider".into()),
                shown_at: Instant::now(),
                dismissed: false,
            },
        );
        state.leave_channel(&channel);

        assert!(state.raid_banner_for(&channel).is_none());
    }

    #[test]
    fn expire_stale_raid_banners_removes_dismissed_entries() {
        let channel = ChannelId::new("rustlang");
        let mut state = AppState::default();
        let now = Instant::now();

        state.show_raid_banner(
            channel.clone(),
            RaidBannerState {
                display_name: "Raider".into(),
                viewer_count: 42,
                source_login: Some("raider".into()),
                shown_at: now,
                dismissed: false,
            },
        );
        state.dismiss_raid_banner(&channel);
        state.expire_stale_raid_banners(now, Duration::from_secs(20));

        assert!(state.raid_banners.is_empty());
    }
}

#[cfg(test)]
mod mentions_state_tests {
    use super::*;
    use crate::model::{ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};
    use chrono::{TimeZone, Utc};

    fn mention(sid: &str, ts_secs: i64, highlighted: bool) -> ChatMessage {
        ChatMessage {
            id: MessageId(ts_secs as u64),
            server_id: Some(sid.to_owned()),
            timestamp: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            channel: ChannelId::new("someone"),
            sender: Sender {
                user_id: UserId("u".into()),
                login: "someone".into(),
                display_name: "Someone".into(),
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: "hi".into(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags {
                is_highlighted: highlighted,
                is_mention: !highlighted,
                ..Default::default()
            },
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn push_mention_bumps_unread_when_requested() {
        let mut s = AppState::default();
        s.push_mention(mention("a", 1, true), true);
        s.push_mention(mention("b", 2, true), true);
        assert_eq!(s.mentions.len(), 2);
        assert_eq!(s.mentions_unread, 2);
        s.clear_mentions_unread();
        assert_eq!(s.mentions_unread, 0);
    }

    #[test]
    fn push_mention_skips_unread_when_tab_is_active() {
        let mut s = AppState::default();
        s.push_mention(mention("a", 1, true), false);
        assert_eq!(s.mentions.len(), 1);
        assert_eq!(s.mentions_unread, 0);
    }

    #[test]
    fn push_mention_dedupes_by_server_id() {
        let mut s = AppState::default();
        s.push_mention(mention("same", 1, true), true);
        s.push_mention(mention("same", 2, true), true);
        assert_eq!(s.mentions.len(), 1);
        assert_eq!(s.mentions_unread, 1, "duplicate must not bump unread");
    }

    #[test]
    fn mentions_buffer_respects_cap() {
        let mut s = AppState::default();
        for i in 0..(MENTIONS_BUFFER_CAP + 50) {
            s.push_mention(mention(&format!("m{i}"), i as i64, true), false);
        }
        assert_eq!(s.mentions.len(), MENTIONS_BUFFER_CAP);
        // Oldest must have been evicted.
        assert_eq!(s.mentions.front().unwrap().server_id.as_deref(), Some("m50"));
    }

    #[test]
    fn prepend_mentions_history_interleaves_by_timestamp_and_dedupes() {
        let mut s = AppState::default();
        // Existing live mention (ts=100).
        s.push_mention(mention("live", 100, true), true);
        // History rows: one older, one that duplicates the live one.
        s.prepend_mentions_history(vec![
            mention("older", 50, true),
            mention("live", 100, true), // dup -> must be skipped
            mention("oldest", 10, true),
        ]);
        // Ordered oldest -> newest, with the dup dropped.
        let ids: Vec<_> = s
            .mentions
            .iter()
            .map(|m| m.server_id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, vec!["oldest", "older", "live"]);
    }
}

#[cfg(test)]
mod tab_visibility_tests {
    use super::*;
    use crate::model::ChannelId;

    #[test]
    fn default_rule_is_always_and_never_hides() {
        let s = AppState::default();
        let ch = ChannelId::new("forsen");
        assert_eq!(s.tab_visibility_rule(&ch), TabVisibilityRule::Always);
        assert!(!s.is_tab_hidden(&ch, Some(false)));
        assert!(!s.is_tab_hidden(&ch, Some(true)));
        assert!(!s.is_tab_hidden(&ch, None));
    }

    #[test]
    fn hide_when_offline_hides_only_for_known_offline() {
        let mut s = AppState::default();
        let ch = ChannelId::new("forsen");
        s.set_tab_visibility_rule(ch.clone(), TabVisibilityRule::HideWhenOffline);
        assert!(s.is_tab_hidden(&ch, Some(false)), "offline must hide");
        assert!(!s.is_tab_hidden(&ch, Some(true)), "live must stay visible");
        assert!(
            !s.is_tab_hidden(&ch, None),
            "unknown live state must not hide (no false-negatives before first fetch)"
        );
    }

    #[test]
    fn active_tab_is_never_hidden_even_when_offline() {
        let mut s = AppState::default();
        let ch = ChannelId::new("forsen");
        s.set_tab_visibility_rule(ch.clone(), TabVisibilityRule::HideWhenOffline);
        s.active_channel = Some(ch.clone());
        assert!(
            !s.is_tab_hidden(&ch, Some(false)),
            "active tab must always render so user doesn't lose their pane"
        );
    }

    #[test]
    fn setting_always_clears_prior_rule() {
        let mut s = AppState::default();
        let ch = ChannelId::new("forsen");
        s.set_tab_visibility_rule(ch.clone(), TabVisibilityRule::HideWhenOffline);
        assert_eq!(s.tab_visibility_rules.len(), 1);
        let prev = s.set_tab_visibility_rule(ch.clone(), TabVisibilityRule::Always);
        assert_eq!(prev, TabVisibilityRule::HideWhenOffline);
        assert!(s.tab_visibility_rules.is_empty());
    }

    #[test]
    fn replace_filters_out_always_entries() {
        let mut s = AppState::default();
        let mut map = HashMap::new();
        map.insert(ChannelId::new("a"), TabVisibilityRule::HideWhenOffline);
        map.insert(ChannelId::new("b"), TabVisibilityRule::Always);
        s.replace_tab_visibility_rules(map);
        assert_eq!(s.tab_visibility_rules.len(), 1);
        assert!(s
            .tab_visibility_rules
            .contains_key(&ChannelId::new("a")));
    }

    #[test]
    fn rule_key_roundtrip() {
        for rule in [TabVisibilityRule::Always, TabVisibilityRule::HideWhenOffline] {
            assert_eq!(TabVisibilityRule::from_key(rule.as_key()), rule);
        }
        // Unknown keys fall back to Always.
        assert_eq!(
            TabVisibilityRule::from_key("garbage"),
            TabVisibilityRule::Always
        );
    }
}
