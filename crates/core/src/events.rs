use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::highlight::HighlightRule;
use crate::model::{
    Badge, ChannelId, ChatMessage, EmoteCatalogEntry, ModActionPreset, ReplyInfo, SenderNamePaint,
    SystemNotice, TwitchEmotePos, UserProfile,
};
use crate::plugins::{PluginUiSurfaceKind, PluginUiValue};

/// A single chat log entry from the IVR logs API.
#[derive(Debug, Clone)]
pub struct IvrLogEntry {
    /// Message text.
    pub text: String,
    /// ISO 8601 timestamp, e.g. "2026-03-05T09:35:03.061Z".
    pub timestamp: String,
    /// Display name of the sender.
    pub display_name: String,
    /// 1 = normal message, 2 = timeout/ban event.
    pub msg_type: u8,
}

/// A held AutoMod message entry that can be approved or denied by moderators.
#[derive(Debug, Clone)]
pub struct AutoModQueueItem {
    pub message_id: String,
    pub sender_user_id: String,
    pub sender_login: String,
    pub text: String,
    pub reason: Option<String>,
}

/// A pending unban request entry from Twitch moderation APIs/EventSub.
#[derive(Debug, Clone)]
pub struct UnbanRequestItem {
    pub request_id: String,
    pub user_id: String,
    pub user_login: String,
    pub text: Option<String>,
    pub created_at: Option<String>,
    pub status: Option<String>,
}
// LinkPreview: Open Graph / Twitter Card metadata for a URL

/// Open Graph / Twitter Card metadata fetched for a URL.
#[derive(Debug, Clone)]
pub struct LinkPreview {
    pub title: Option<String>,
    pub description: Option<String>,
    /// og:image URL (thumbnail). The image is fetched into `emote_bytes`.
    pub thumbnail_url: Option<String>,
    /// Site name from `og:site_name` (e.g. "YouTube", "Twitter").
    pub site_name: Option<String>,
    /// True once the fetch attempt has completed (even if it returned nothing).
    pub fetched: bool,
}
// Commands (UI to runtime): actions initiated by the UI

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppCommand {
    /// Join a channel.
    JoinChannel { channel: ChannelId },
    /// Join an IRC channel with an optional channel key.
    JoinIrcChannel {
        channel: ChannelId,
        key: Option<String>,
    },
    /// Leave / close a tab.
    LeaveChannel { channel: ChannelId },
    /// Request emote loading for a channel (by Twitch user-id).
    LoadChannelEmotes { channel_twitch_id: String },
    /// Fetch a single image on-demand (e.g. HD emote on hover).
    FetchImage { url: String },
    /// Log in with a Twitch OAuth token.
    Login { token: String },
    /// Log out and switch back to anonymous mode.
    Logout,
    /// Send a chat message to a channel (requires auth).
    SendMessage {
        channel: ChannelId,
        text: String,
        /// If set, the message is a reply to this server-assigned message ID.
        reply_to_msg_id: Option<String>,
        /// Optional reply context used for local-echo rendering.
        #[serde(default)]
        reply: Option<ReplyInfo>,
    },
    /// Send a Twitch whisper via Helix (`POST /helix/whispers`).
    SendWhisper {
        /// Target Twitch login (lowercase, no leading `@`).
        target_login: String,
        /// Whisper body text.
        text: String,
    },
    /// Request a Twitch stream status snapshot by login name.
    FetchStreamStatus { login: String },
    /// Request a Twitch user profile lookup by login name.
    FetchUserProfile { login: String },
    /// Timeout a user in a channel via the Twitch Helix API.
    TimeoutUser {
        channel: ChannelId,
        login: String,
        user_id: String,
        seconds: u32,
        reason: Option<String>,
    },
    /// Permanently ban a user from a channel via the Twitch Helix API.
    BanUser {
        channel: ChannelId,
        login: String,
        user_id: String,
        reason: Option<String>,
    },
    /// Lift an active ban or timeout for a user via the Twitch Helix API.
    UnbanUser {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    /// Warn a user in a channel via the Twitch Helix API.
    WarnUser {
        channel: ChannelId,
        login: String,
        user_id: String,
        reason: String,
    },
    /// Mark a user as monitored/restricted suspicious via the Twitch Helix API.
    SetSuspiciousUser {
        channel: ChannelId,
        login: String,
        user_id: String,
        restricted: bool,
    },
    /// Remove a user from suspicious-user treatment via the Twitch Helix API.
    ClearSuspiciousUser {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    /// Approve or deny a held AutoMod message via Twitch Helix.
    ResolveAutoModMessage {
        channel: ChannelId,
        message_id: String,
        sender_user_id: String,
        /// `ALLOW` or `DENY`.
        action: String,
    },
    /// Fetch pending unban requests for a channel via Twitch Helix.
    FetchUnbanRequests { channel: ChannelId },
    /// Approve or deny a pending unban request via Twitch Helix.
    ResolveUnbanRequest {
        channel: ChannelId,
        request_id: String,
        approve: bool,
        resolution_text: Option<String>,
    },
    /// Open the moderation tools window in the UI.
    OpenModerationTools {
        /// Optional channel to focus when opening the tools.
        channel: Option<ChannelId>,
    },
    /// Update a channel-points redemption status via Twitch Helix.
    /// `status` should be `FULFILLED` or `CANCELED`.
    UpdateRewardRedemptionStatus {
        channel: ChannelId,
        reward_id: String,
        redemption_id: String,
        status: String,
        user_login: String,
        reward_title: String,
    },
    /// Clears all messages in the channel display (visual-only, not sent to Twitch).
    ClearLocalMessages { channel: ChannelId },
    /// Opens a URL in the system default browser.
    OpenUrl { url: String },
    /// Open the Crust log/data directory in the system file manager.
    OpenLogsFolder,
    /// Launch `streamlink` for the given Twitch channel login using the
    /// persisted external-tools settings.
    OpenStreamlink { channel: String },
    /// Launch the user's custom player template for the given Twitch channel.
    OpenPlayer { channel: String },
    /// Persist Streamlink / external-player configuration.
    SetExternalToolsSettings {
        /// Path to the `streamlink` binary (empty = use `PATH`).
        streamlink_path: String,
        /// Preferred Streamlink quality token (e.g. `best`, `720p60`).
        streamlink_quality: String,
        /// Extra args prepended to the Streamlink invocation.
        streamlink_extra_args: String,
        /// Custom player command template with `{channel}` / `{url}` /
        /// `{quality}` / `{mpv}`.
        player_template: String,
        /// Path to the `mpv` binary (empty = use `PATH`).  Exposed via `{mpv}`
        /// in `player_template`.
        mpv_path: String,
        /// Optional Twitch session `auth-token` cookie value.  When non-empty,
        /// auto-injected as `--twitch-api-header "Authorization=OAuth <token>"`.
        streamlink_session_token: String,
    },
    /// Toggle Twitch Shield Mode on the given channel (mod/broadcaster only).
    SetShieldMode { channel: ChannelId, active: bool },
    /// Modify Twitch channel title and/or game (broadcaster scope required).
    UpdateChannelInfo {
        channel: ChannelId,
        /// New broadcast title. `None` leaves it unchanged.
        title: Option<String>,
        /// New game/category name (resolved to `game_id` via Helix). `None` leaves it unchanged.
        game_name: Option<String>,
    },
    /// Look up how long `user` has followed `channel`.
    FetchFollowAge { channel: ChannelId, user: String },
    /// Silent variant of [`FetchFollowAge`] used by the user-card popup.
    /// Result is delivered via [`AppEvent::UserCardFollowAgeLoaded`] rather
    /// than as a system notice.
    FetchUserCardFollowAge { channel: ChannelId, login: String },
    /// Toggle auto-claim of channel-points "Bonus Points" rewards.
    SetAutoClaimBonusPoints { enabled: bool },
    /// Open the embedded Twitch sign-in window so the user can authenticate
    /// the webview session. No-op when already signed in.
    OpenTwitchSignIn,
    /// Look up the account creation date for `user` and report its age.
    FetchAccountAge {
        /// Channel to inject the resulting local message into.
        channel: ChannelId,
        user: String,
    },
    /// Injects a local informational message into a channel's feed (not sent to Twitch).
    InjectLocalMessage { channel: ChannelId, text: String },
    /// Opens the user-card popup for the given login in a channel.
    ShowUserCard { login: String, channel: ChannelId },
    /// Execute a command registered by a plugin.
    RunPluginCommand {
        channel: ChannelId,
        command: String,
        words: Vec<String>,
        #[serde(default)]
        reply_to_msg_id: Option<String>,
        #[serde(default)]
        reply: Option<ReplyInfo>,
        raw_text: String,
    },
    /// Reload all plugins from disk.
    ReloadPlugins,
    /// Run a delayed Lua callback on the main app thread.
    RunPluginCallback { vm_key: usize, callback_ref: i32 },
    /// Fetch Open-Graph / Twitter-Card metadata for a URL to show a hover preview.
    FetchLinkPreview { url: String },
    /// Add a new account by validating and saving the given OAuth token.
    AddAccount { token: String },
    /// Switch the active session to an already-saved account.
    SwitchAccount { username: String },
    /// Remove a saved account (and its token) permanently.
    RemoveAccount { username: String },
    /// Mark an account as the one to auto-login on next startup.
    SetDefaultAccount { username: String },
    /// Set the IRC nickname used for generic IRC servers.
    SetIrcNick { nick: String },
    /// Set NickServ credentials for automatic IRC identification.
    SetIrcAuth {
        nickserv_user: String,
        nickserv_pass: String,
    },
    /// Persist beta transport feature toggles.
    SetBetaFeatures {
        kick_enabled: bool,
        irc_enabled: bool,
    },
    /// Toggle always-on-top window mode.
    SetAlwaysOnTop { enabled: bool },
    /// Switch UI theme ("dark" or "light") and persist to settings.
    SetTheme { theme: String },
    /// Persist chat body font size, UI scale ratio, and per-section font sizes.
    SetFontSizes {
        /// Chat body font size in points.
        chat_font_size: f32,
        /// UI scale ratio applied to egui `pixels_per_point`.
        ui_font_size: f32,
        /// Top chrome toolbar label size (pt).
        topbar_font_size: f32,
        /// Channel tab chip label size (pt).
        tabs_font_size: f32,
        /// Message timestamp size (pt).
        timestamps_font_size: f32,
        /// Room-state / viewer-count pill size (pt).
        pills_font_size: f32,
        /// Tooltip / popover label size (pt). 0.0 = auto.
        popups_font_size: f32,
        /// Inline chip / inline badge label size (pt). 0.0 = auto.
        chips_font_size: f32,
        /// User-card heading size (pt). 0.0 = auto.
        usercard_font_size: f32,
        /// Login / dialog helper-text size (pt). 0.0 = auto.
        dialog_font_size: f32,
    },
    /// Persist the last-focused channel so it can be restored on next launch.
    SetLastActiveChannel {
        /// Serialised ChannelId (platform-prefixed key).
        channel: String,
    },
    /// Persist current window geometry (logical points). `None` means unknown.
    SetWindowGeometry {
        pos: Option<[f32; 2]>,
        size: Option<[f32; 2]>,
        maximized: bool,
    },
    /// Persist whispers-panel state.
    SetWhispersPanel {
        visible: bool,
        active_login: String,
    },
    /// Persist user-arranged channel tab order.
    SetChannelOrder { order: Vec<String> },
    /// Persist split-pane layout.
    SetSplitPanes {
        /// Each entry: (channel_id, frac).
        panes: Vec<(String, f32)>,
        focused: usize,
    },
    /// Update the per-tab visibility rule for `channel`. Setting the rule
    /// to [`crate::state::TabVisibilityRule::Always`] removes any prior
    /// override. Persisted via `AppSettings::tab_visibility_rules`.
    SetTabVisibilityRule {
        channel: ChannelId,
        rule: crate::state::TabVisibilityRule,
    },
    /// Persist chat-input overflow behavior and long-message collapse settings.
    SetChatUiBehavior {
        /// `true` = Prevent mode (block typing past Twitch's limit).
        /// `false` = Highlight mode (allow typing and mark overflow).
        prevent_overlong_twitch_messages: bool,
        /// Whether to collapse very long messages in the message list.
        collapse_long_messages: bool,
        /// Maximum visible lines before appending an ellipsis.
        collapse_long_message_lines: usize,
        /// Only run animation-driven repainting while the window is focused.
        animations_when_focused: bool,
    },
    /// Persist general UI/filter/channel settings managed by the settings page.
    SetGeneralSettings {
        /// Show per-message timestamps in chat.
        show_timestamps: bool,
        /// Include seconds in chat timestamps.
        show_timestamp_seconds: bool,
        /// Use 24-hour clock formatting for chat timestamps.
        use_24h_timestamps: bool,
        /// Persist new chat messages into the local SQLite log index.
        local_log_indexing_enabled: bool,
        /// Channels to auto-join at startup/reconnect.
        auto_join: Vec<String>,
        /// Highlight keywords (free-form, case-insensitive matching ready).
        highlights: Vec<String>,
        /// Ignored usernames (lowercase expected).
        ignores: Vec<String>,
    },
    /// Persist slash command usage counts used to rank autocomplete.
    SetSlashUsageCounts {
        /// Command usage pairs of (`command_name`, `count`).
        usage_counts: Vec<(String, u32)>,
    },
    /// Persist emote picker user preferences.
    SetEmotePickerPreferences {
        /// Favorited emote URLs.
        favorites: Vec<String>,
        /// Recently used emote URLs (most-recent first).
        recent: Vec<String>,
        /// Optional boosted provider key (`twitch`, `7tv`, `bttv`, `ffz`, `emoji`).
        provider_boost: Option<String>,
    },
    /// Persist appearance and shell layout settings.
    SetAppearanceSettings {
        /// Preferred channel list layout (`sidebar` or `top_tabs`).
        channel_layout: String,
        /// Whether the sidebar should remain visible in sidebar mode.
        sidebar_visible: bool,
        /// Whether the analytics panel is visible.
        analytics_visible: bool,
        /// Whether the IRC diagnostics panel is visible.
        irc_status_visible: bool,
        /// Tab density/style (`compact` or `normal`).
        tab_style: String,
        /// Whether tabs show close affordances on hover/selection.
        show_tab_close_buttons: bool,
        /// Whether tabs show live indicators for live Twitch channels.
        show_tab_live_indicators: bool,
        /// Whether split headers show stream title metadata.
        split_header_show_title: bool,
        /// Whether split headers show game/category metadata.
        split_header_show_game: bool,
        /// Whether split headers show viewer counts.
        split_header_show_viewer_count: bool,
    },
    /// Fetch external chat logs for a user from the IVR logs API.
    FetchIvrLogs { channel: String, username: String },
    /// Load older locally persisted chat history (SQLite) before the oldest
    /// currently loaded message timestamp.
    LoadOlderLocalHistory {
        channel: ChannelId,
        /// Exclusive upper timestamp bound (Unix ms).
        before_ts_ms: i64,
        /// Maximum number of rows to load.
        limit: usize,
    },
    /// Create a Twitch poll via Helix (`POST /helix/polls`).
    CreatePoll {
        channel: ChannelId,
        title: String,
        choices: Vec<String>,
        duration_secs: u32,
        /// Optional channel-points per extra vote. When set, Helix poll
        /// creation enables channel-points voting.
        channel_points_per_vote: Option<u32>,
    },
    /// End or cancel the active Twitch poll via Helix (`PATCH /helix/polls`).
    /// `status` should be `ARCHIVED` (normal end) or `TERMINATED` (cancel).
    EndPoll { channel: ChannelId, status: String },
    /// Create a Twitch prediction via Helix (`POST /helix/predictions`).
    CreatePrediction {
        channel: ChannelId,
        title: String,
        outcomes: Vec<String>,
        duration_secs: u32,
    },
    /// Lock the active Twitch prediction via Helix (`PATCH /helix/predictions`, status=LOCKED).
    LockPrediction { channel: ChannelId },
    /// Resolve the active Twitch prediction with a 1-based outcome index.
    ResolvePrediction {
        channel: ChannelId,
        winning_outcome_index: usize,
    },
    /// Cancel the active Twitch prediction via Helix (`PATCH /helix/predictions`, status=CANCELED).
    CancelPrediction { channel: ChannelId },
    /// Start a Twitch commercial via Helix (`POST /helix/channels/commercial`).
    StartCommercial {
        channel: ChannelId,
        length_secs: u32,
    },
    /// Create a Twitch stream marker via Helix (`POST /helix/streams/markers`).
    CreateStreamMarker {
        channel: ChannelId,
        description: Option<String>,
    },
    /// Send a Twitch channel announcement via Helix (`POST /helix/chat/announcements`).
    SendAnnouncement {
        channel: ChannelId,
        message: String,
        color: Option<String>,
    },
    /// Send a Twitch shoutout via Helix (`POST /helix/chat/shoutouts`).
    SendShoutout {
        channel: ChannelId,
        target_login: String,
    },
    /// Delete a single message via Helix (`DELETE /helix/moderation/chat`).
    DeleteMessage {
        channel: ChannelId,
        /// Server-assigned message ID (from `ChatMessage::server_id`).
        message_id: String,
    },
    /// Hide all visible messages from a user locally in the current channel.
    ClearUserMessagesLocally { channel: ChannelId, login: String },
    /// Persist an updated ordered list of highlight rules.
    SetHighlightRules { rules: Vec<HighlightRule> },
    /// Persist an updated ordered list of filter records.
    SetFilterRecords {
        records: Vec<crate::model::FilterRecord>,
    },
    /// Persist an updated ordered list of moderation action presets.
    SetModActionPresets { presets: Vec<ModActionPreset> },
    /// Persist an updated list of nickname aliases.
    SetNicknames {
        nicknames: Vec<crate::model::Nickname>,
    },
    /// Persist an updated ignored-user list.
    SetIgnoredUsers {
        users: Vec<crate::ignores::IgnoredUser>,
    },
    /// Persist an updated ignored-phrase list.
    SetIgnoredPhrases {
        phrases: Vec<crate::ignores::IgnoredPhrase>,
    },
    /// Persist an updated list of custom command aliases. Replaces the
    /// entire set; send the full list every time the editor saves.
    SetCommandAliases {
        aliases: Vec<crate::commands::CommandAlias>,
    },
    /// Persist an updated hotkey binding map. Each pair is
    /// `(HotkeyAction::as_key(), KeyBinding)`. The entire set is replaced
    /// on every write.
    SetHotkeyBindings {
        bindings: Vec<(String, crate::hotkeys::KeyBinding)>,
    },
    /// Append a word to the user's custom spellcheck dictionary.
    /// Persisted via [`crate::events::AppEvent::SpellDictionaryUpdated`].
    /// Duplicates and non-alphabetic input are rejected silently on the
    /// storage side.
    AddWordToDictionary { word: String },
    /// Remove a word from the user's custom spellcheck dictionary.
    RemoveWordFromDictionary { word: String },
    /// Replace the user's custom spellcheck dictionary wholesale (used by
    /// the settings editor when bulk-editing the list).
    SetCustomSpellDictionary { words: Vec<String> },
    /// Enable or disable chat-input spellchecking.
    SetSpellcheckEnabled { enabled: bool },
    /// Persist the "show pronouns in usercard" toggle.
    SetShowPronounsInUsercard { enabled: bool },
    /// Request a pronouns fetch for a specific login.  No-op if the toggle is
    /// off or the value is already cached.
    FetchUserPronouns { login: String },
    /// Refresh authentication after a 401 - re-validate the stored token.
    RefreshAuth,
    /// Persist desktop notification toggle.
    SetNotificationSettings { desktop_notifications_enabled: bool },
    /// Trigger a GitHub releases update check.
    CheckForUpdates {
        /// True when initiated from explicit user action.
        manual: bool,
    },
    /// Enable/disable automatic background update checks.
    SetUpdateChecksEnabled { enabled: bool },
    /// Persist the currently skipped update version.
    SkipUpdateVersion { version: String },
    /// Download, verify, stage, and schedule installation of the available update.
    InstallAvailableUpdate {
        /// If true, exit the app after scheduling installer so update applies immediately.
        restart_now: bool,
    },
    /// Persist per-event sound notification settings (mention / whisper /
    /// sub / raid / custom highlight).  The UI sends the full configured
    /// set; missing keys fall back to [`crate::SoundEventSetting::default`]
    /// on the receiving end.
    SetSoundSettings {
        events: Vec<(String, crate::sound::SoundEventSetting)>,
    },
    /// Persist streamer-mode preferences.
    SetStreamerModeSettings {
        /// `off`, `auto`, or `on`.
        mode: String,
        /// Hide link preview tooltips while active.
        hide_link_previews: bool,
        /// Hide viewer counts in split headers while active.
        hide_viewer_counts: bool,
        /// Suppress sound notifications while active.
        suppress_sounds: bool,
    },
    /// Upload an image to the configured host (Imgur / Nuuls / ShareX).
    UploadImage {
        channel: ChannelId,
        /// Raw image bytes (already converted to a network format if needed).
        bytes: Vec<u8>,
        /// Extension without the dot: `"png"`, `"gif"`, `"jpeg"`.
        format: String,
        /// Original on-disk path if the image came from a file drop.
        source_path: Option<String>,
    },
    /// Force the live-feed poll task to refresh immediately (bypass tick wait).
    LiveFeedRefresh,
}

// Events (runtime to UI): notifications sent from runtime to UI

#[derive(Debug, Clone)]
pub enum AppEvent {
    ConnectionStateChanged {
        state: ConnectionState,
    },
    ChannelJoined {
        channel: ChannelId,
    },
    ChannelParted {
        channel: ChannelId,
    },
    /// An IRC channel redirect occurred (e.g. #chat -> ##chat on Libera).
    /// The UI should replace the old channel tab with the new one.
    ChannelRedirected {
        old_channel: ChannelId,
        new_channel: ChannelId,
    },
    MessageReceived {
        channel: ChannelId,
        message: ChatMessage,
    },
    /// Incoming Twitch whisper delivered out-of-band from channel chat.
    WhisperReceived {
        /// Login of the whisper sender.
        from_login: String,
        /// Display name of the whisper sender.
        from_display_name: String,
        /// Whisper target login (usually the authenticated user for incoming whispers).
        target_login: String,
        /// Whisper body text.
        text: String,
        /// Twitch native emote ranges parsed from IRC tags.
        twitch_emotes: Vec<TwitchEmotePos>,
        /// True when the local user sent the whisper (server echo).
        is_self: bool,
        /// Whisper timestamp from IRC tags when available.
        timestamp: DateTime<Utc>,
        /// True when this whisper is replayed from local history.
        is_history: bool,
    },
    MessageDeleted {
        channel: ChannelId,
        server_id: String,
    },
    SystemNotice(SystemNotice),
    /// Raw image bytes are ready; egui loaders handle decoding + animation.
    EmoteImageReady {
        uri: String,
        width: u32,
        height: u32,
        raw_bytes: Vec<u8>,
    },
    /// Authenticated successfully with Twitch.
    Authenticated {
        username: String,
        user_id: String,
    },
    /// Full snapshot of the emote catalog (sent after each load).
    EmoteCatalogUpdated {
        emotes: Vec<EmoteCatalogEntry>,
    },
    /// Logged out / reverted to anonymous.
    LoggedOut,
    Error {
        context: String,
        message: String,
    },
    /// Historical messages loaded from an external API (e.g. recent-messages).
    /// Should be prepended to the channel's message buffer.
    HistoryLoaded {
        channel: ChannelId,
        messages: Vec<ChatMessage>,
    },
    /// Mention-matching messages replayed from the local SQLite log on
    /// startup. Populates the cross-channel Mentions pseudo-tab buffer.
    MentionsLoaded {
        messages: Vec<ChatMessage>,
    },
    /// Twitch user profile loaded from the IVR API.
    UserProfileLoaded {
        profile: UserProfile,
    },
    /// Twitch stream status changed (from EventSub), allowing immediate
    /// live/offline indicators before full profile refresh completes.
    StreamStatusUpdated {
        /// Broadcaster login (lowercase preferred).
        login: String,
        /// `true` when stream is live, `false` when offline.
        is_live: bool,
        /// Optional stream title when known.
        title: Option<String>,
        /// Optional game/category name when known.
        game: Option<String>,
        /// Optional viewer count when known.
        viewers: Option<u64>,
    },
    /// A user profile lookup finished without data (network/API/user not found).
    UserProfileUnavailable {
        login: String,
    },
    /// Mark all visible messages from a user as deleted (timeout / ban).
    UserMessagesCleared {
        channel: ChannelId,
        login: String,
    },
    /// Low-trust treatment for a user changed. `status = None` means cleared.
    LowTrustStatusUpdated {
        channel: ChannelId,
        login: String,
        /// Twitch user-id; needed to call the Helix suspicious-user APIs
        /// from the moderation tools UI. Empty when unknown (legacy
        /// emitters before the field was added).
        user_id: String,
        /// Display name; lets the UI render the original casing.
        display_name: String,
        status: Option<crate::model::LowTrustStatus>,
    },
    /// USERSTATE received - badges, color and mod status for the logged-in user.
    UserStateUpdated {
        channel: ChannelId,
        is_mod: bool,
        badges: Vec<crate::model::Badge>,
        color: Option<String>,
    },
    /// Clear all messages from the given channel's UI buffer (response to ClearLocalMessages).
    ChannelMessagesCleared {
        channel: ChannelId,
    },
    /// The logged-in user's avatar URL has been resolved.
    SelfAvatarLoaded {
        avatar_url: String,
    },
    /// Source-channel profile for a Shared Chat message has been resolved. UI
    /// back-fills `ChatMessage.shared.{login,display_name,profile_url}` on
    /// every already-buffered message with a matching `room_id`, and caches
    /// the result for future mirrored deliveries. Image bytes land in
    /// `emote_bytes` via a companion `EmoteImageReady` event.
    SharedChannelResolved {
        room_id: String,
        login: String,
        display_name: String,
        profile_url: Option<String>,
    },
    /// A channel's Shared Chat session snapshot has been refreshed. `None`
    /// means the broadcaster is no longer in a shared-chat session (so the
    /// viewer-total banner should be removed).
    SharedChatSessionUpdated {
        channel: super::model::ChannelId,
        session: Option<crate::state::SharedChatSessionState>,
    },
    /// Open-Graph / Twitter-Card metadata is ready for a URL.
    LinkPreviewReady {
        url: String,
        title: Option<String>,
        description: Option<String>,
        /// og:image URL; the image bytes land in emote_bytes under this key.
        thumbnail_url: Option<String>,
        /// Site name from `og:site_name` (e.g. "YouTube", "Twitter").
        site_name: Option<String>,
    },
    /// The set of saved accounts or the active account changed.
    AccountListUpdated {
        /// Ordered list of all saved account usernames.
        accounts: Vec<String>,
        /// Username of the currently active (authenticated) account, if any.
        active: Option<String>,
        /// Username of the account that auto-logs in on startup, if set.
        default: Option<String>,
    },
    /// The topic for an IRC channel was set or changed.
    IrcTopicChanged {
        channel: ChannelId,
        topic: String,
    },
    /// Channel emote catalog loaded (including 0 when none exist).
    ChannelEmotesLoaded {
        channel: ChannelId,
        count: usize,
    },
    /// Beta transport feature toggles loaded/updated from settings.
    BetaFeaturesUpdated {
        kick_enabled: bool,
        irc_enabled: bool,
        /// NickServ username for IRC auto-identification.
        irc_nickserv_user: String,
        /// NickServ password for IRC auto-identification.
        irc_nickserv_pass: String,
        /// Whether always-on-top is enabled.
        always_on_top: bool,
    },
    /// Chat UX/perf behavior loaded or updated from settings.
    ChatUiBehaviorUpdated {
        /// `true` = Prevent mode (block typing past Twitch's limit).
        /// `false` = Highlight mode (allow typing and mark overflow).
        prevent_overlong_twitch_messages: bool,
        /// Whether to collapse very long messages in the message list.
        collapse_long_messages: bool,
        /// Maximum visible lines before appending an ellipsis.
        collapse_long_message_lines: usize,
        /// Only run animation-driven repainting while the window is focused.
        animations_when_focused: bool,
    },
    /// General settings snapshot loaded/updated from persistent storage.
    GeneralSettingsUpdated {
        /// Show per-message timestamps in chat.
        show_timestamps: bool,
        /// Include seconds in chat timestamps.
        show_timestamp_seconds: bool,
        /// Use 24-hour clock formatting for chat timestamps.
        use_24h_timestamps: bool,
        /// Persist new chat messages into the local SQLite log index.
        local_log_indexing_enabled: bool,
        /// Channels to auto-join at startup/reconnect.
        auto_join: Vec<String>,
        /// Highlight keywords.
        highlights: Vec<String>,
        /// Ignored usernames (lowercase).
        ignores: Vec<String>,
        /// Enable desktop notifications for highlight rules with `show_in_mentions`.
        desktop_notifications_enabled: bool,
    },
    /// External-tool settings snapshot (Streamlink + custom player).
    ExternalToolsSettingsUpdated {
        streamlink_path: String,
        streamlink_quality: String,
        streamlink_extra_args: String,
        player_template: String,
        mpv_path: String,
        streamlink_session_token: String,
    },
    /// Slash command usage counts loaded/updated from persistent storage.
    SlashUsageCountsUpdated {
        usage_counts: Vec<(String, u32)>,
    },
    /// Emote picker preferences loaded/updated from persistent storage.
    EmotePickerPreferencesUpdated {
        favorites: Vec<String>,
        recent: Vec<String>,
        provider_boost: Option<String>,
    },
    /// User-managed spellcheck state loaded/updated from persistent storage.
    /// Emitted once at startup and after every `AddWordToDictionary`,
    /// `RemoveWordFromDictionary`, `SetCustomSpellDictionary` or
    /// `SetSpellcheckEnabled` command so the UI can refresh its in-memory
    /// mirror.
    SpellDictionaryUpdated {
        /// Whether chat-input spellchecking should underline misspelled
        /// words and surface suggestions.
        enabled: bool,
        /// Sorted list of user-added words (lowercase).
        words: Vec<String>,
    },
    /// Startup hint: the last-focused channel from the previous session.
    /// UI activates this channel once it has been joined and is present in
    /// `AppState.channels`. Also carries previous-session UI layout
    /// (channel order, split panes, whispers panel) so the UI can rebuild
    /// the workspace exactly as the user left it.
    RestoreLastActiveChannel {
        /// Serialized ChannelId (platform-prefixed key).
        channel: String,
        /// Previous-session channel tab order (serialized ChannelIds).
        channel_order: Vec<String>,
        /// Previous-session split-pane layout: (channel, frac).
        split_panes: Vec<(String, f32)>,
        /// Focused-pane index within `split_panes`.
        split_panes_focused: usize,
        /// Whispers panel was visible at last shutdown.
        whispers_visible: bool,
        /// Last-focused whisper thread login.
        last_whisper_login: String,
    },
    /// Font sizing snapshot loaded/updated from persistent storage.
    FontSettingsUpdated {
        /// Chat body font size in points.
        chat_font_size: f32,
        /// UI scale ratio for egui `pixels_per_point`.
        ui_font_size: f32,
        /// Top chrome toolbar label size (pt).
        topbar_font_size: f32,
        /// Channel tab chip label size (pt).
        tabs_font_size: f32,
        /// Message timestamp size (pt).
        timestamps_font_size: f32,
        /// Room-state / viewer-count pill size (pt).
        pills_font_size: f32,
        /// Tooltip / popover label size (pt). 0.0 = auto.
        popups_font_size: f32,
        /// Inline chip / inline badge label size (pt). 0.0 = auto.
        chips_font_size: f32,
        /// User-card heading size (pt). 0.0 = auto.
        usercard_font_size: f32,
        /// Login / dialog helper-text size (pt). 0.0 = auto.
        dialog_font_size: f32,
    },
    /// Per-tab visibility rules loaded/updated from persistent storage.
    /// Emitted at startup (initial snapshot) and after every successful
    /// `SetTabVisibilityRule` write so the UI mirror stays consistent
    /// with disk.
    TabVisibilityRulesUpdated {
        /// List of (channel-id, rule) pairs. Channels absent from the
        /// list default to [`crate::state::TabVisibilityRule::Always`].
        rules: Vec<(ChannelId, crate::state::TabVisibilityRule)>,
    },
    /// Appearance and shell layout settings loaded/updated from storage.
    AppearanceSettingsUpdated {
        channel_layout: String,
        sidebar_visible: bool,
        analytics_visible: bool,
        irc_status_visible: bool,
        tab_style: String,
        show_tab_close_buttons: bool,
        show_tab_live_indicators: bool,
        split_header_show_title: bool,
        split_header_show_game: bool,
        split_header_show_viewer_count: bool,
    },
    /// A batch of image prefetch tasks has been queued.  The loading screen
    /// uses this to track progress vs `EmoteImageReady` completions.
    ImagePrefetchQueued {
        count: usize,
    },
    /// Twitch ROOMSTATE tags updated - room modes for a channel.
    RoomStateUpdated {
        channel: ChannelId,
        emote_only: Option<bool>,
        followers_only: Option<i32>,
        slow: Option<u32>,
        subs_only: Option<bool>,
        r9k: Option<bool>,
    },
    /// 7TV cosmetics resolved for a Twitch user id; UI should update visible
    /// messages from this sender so badges appear without waiting for
    /// a new message.
    SenderCosmeticsUpdated {
        user_id: String,
        color: Option<String>,
        /// Optional 7TV name paint metadata.
        name_paint: Option<SenderNamePaint>,
        badge: Option<Badge>,
        /// 7TV animated avatar URL (if the user has one set).
        avatar_url: Option<String>,
    },
    /// External IVR chat logs loaded for a user.
    IvrLogsLoaded {
        username: String,
        messages: Vec<IvrLogEntry>,
    },
    /// External IVR log fetch failed.
    IvrLogsFailed {
        username: String,
        error: String,
    },
    /// A held AutoMod message has entered the moderation queue.
    AutoModQueueAppend {
        channel: ChannelId,
        item: AutoModQueueItem,
    },
    /// Remove a held AutoMod message from the moderation queue.
    AutoModQueueRemove {
        channel: ChannelId,
        message_id: String,
        action: Option<String>,
    },
    /// Hide all visible messages from a user in a channel locally.
    ClearUserMessagesLocally {
        channel: ChannelId,
        login: String,
    },
    /// Unban requests snapshot loaded for a channel.
    UnbanRequestsLoaded {
        channel: ChannelId,
        requests: Vec<UnbanRequestItem>,
    },
    /// Failed to fetch unban requests.
    UnbanRequestsFailed {
        channel: ChannelId,
        error: String,
    },
    /// A new unban request was created.
    UnbanRequestUpsert {
        channel: ChannelId,
        request: UnbanRequestItem,
    },
    /// A pending unban request was resolved.
    UnbanRequestResolved {
        channel: ChannelId,
        request_id: String,
        status: String,
    },
    /// Request that the UI opens moderation tools.
    OpenModerationTools {
        channel: Option<ChannelId>,
    },
    /// Updated highlight rules list (sent after persistence).
    HighlightRulesUpdated {
        rules: Vec<HighlightRule>,
    },
    /// Updated filter records list.
    FilterRecordsUpdated {
        records: Vec<crate::model::FilterRecord>,
    },
    /// Updated moderation action preset list.
    ModActionPresetsUpdated {
        presets: Vec<ModActionPreset>,
    },
    /// Updated nickname alias list.
    NicknamesUpdated {
        nicknames: Vec<crate::model::Nickname>,
    },
    /// Updated ignored-user list.
    IgnoredUsersUpdated {
        users: Vec<crate::ignores::IgnoredUser>,
    },
    /// Updated ignored-phrase list.
    IgnoredPhrasesUpdated {
        phrases: Vec<crate::ignores::IgnoredPhrase>,
    },
    /// Updated custom-command alias list.
    CommandAliasesUpdated {
        aliases: Vec<crate::commands::CommandAlias>,
    },
    /// Updated hotkey bindings. Emitted once on startup (from saved
    /// settings, or defaults) and after every successful
    /// [`AppCommand::SetHotkeyBindings`].
    HotkeyBindingsUpdated {
        bindings: Vec<(String, crate::hotkeys::KeyBinding)>,
    },
    /// Pronouns resolved for a user (from alejo.io).  `pronouns=None` means
    /// the user has no pronouns set or a fetch miss.
    UserPronounsLoaded {
        login: String,
        pronouns: Option<String>,
    },
    /// Silent follow-age result for the user-card popup. `followed_at = None`
    /// means the user does not follow `channel`. Carries the channel so the
    /// popup can ignore stale results when its context has changed.
    UserCardFollowAgeLoaded {
        channel: ChannelId,
        login: String,
        followed_at: Option<String>,
    },
    /// Auto-claim setting changed (persisted in [`crate::*`] settings).
    AutoClaimBonusPointsUpdated { enabled: bool },
    /// Latest channel-points balance for the logged-in user in `channel`.
    /// Emitted by the periodic claimer poll; safe to ignore when the user
    /// is anonymous or not on a Twitch tab.
    ChannelPointsBalanceUpdated {
        channel: ChannelId,
        balance: u64,
    },
    /// A "Bonus Points" claim has been auto-redeemed. `points` = the amount
    /// the claim was worth, `balance` = the new running total.
    ChannelPointsClaimed {
        channel: ChannelId,
        points: u64,
        balance: u64,
    },
    /// Embedded webview login state changed.
    /// `logged_in = None` during initial probe.
    TwitchWebviewLoginState { logged_in: Option<bool> },
    /// Embedded webview DOM-clicked a Bonus Points button on `channel`.
    TwitchWebviewBonusClaimed { channel: crate::model::ChannelId },
    /// Usercard preferences loaded/updated (currently: show-pronouns toggle).
    UsercardSettingsUpdated {
        show_pronouns: bool,
    },
    /// Auth has expired; prompt user to re-authenticate.
    AuthExpired,
    /// Updater preference/state loaded or updated from settings.
    UpdaterSettingsUpdated {
        update_checks_enabled: bool,
        last_checked_at: Option<String>,
        skipped_version: String,
    },
    /// A newer app release is available on GitHub.
    UpdateAvailable {
        /// Newer semantic version string (without leading `v`).
        version: String,
        /// Human-facing GitHub release page URL.
        release_url: String,
        /// Selected Windows x64 zip asset name.
        asset_name: String,
    },
    /// Update check completed with no newer release.
    UpdateCheckUpToDate {
        /// Current app version used for comparison.
        version: String,
    },
    /// Update check failed (network/parse/integrity metadata issue).
    UpdateCheckFailed {
        message: String,
        /// True when initiated from explicit user action.
        manual: bool,
    },
    /// Update install pipeline started (download/verify/extract).
    UpdateInstallStarted {
        version: String,
    },
    /// Update installer has been staged and scheduled.
    UpdateInstallScheduled {
        version: String,
        restart_now: bool,
    },
    /// Update install failed.
    UpdateInstallFailed {
        version: String,
        message: String,
    },
    /// A plugin-owned UI widget emitted an action event.
    PluginUiAction {
        plugin_name: String,
        surface_kind: PluginUiSurfaceKind,
        surface_id: String,
        widget_id: String,
        action: Option<String>,
        value: Option<PluginUiValue>,
        form_values: BTreeMap<String, PluginUiValue>,
    },
    /// A plugin-owned UI input changed.
    PluginUiChange {
        plugin_name: String,
        surface_kind: PluginUiSurfaceKind,
        surface_id: String,
        widget_id: String,
        value: PluginUiValue,
        form_values: BTreeMap<String, PluginUiValue>,
    },
    /// A plugin-owned UI form or button requested submission.
    PluginUiSubmit {
        plugin_name: String,
        surface_kind: PluginUiSurfaceKind,
        surface_id: String,
        widget_id: Option<String>,
        action: Option<String>,
        form_values: BTreeMap<String, PluginUiValue>,
    },
    /// A plugin-owned floating window was closed by the user.
    PluginUiWindowClosed {
        plugin_name: String,
        window_id: String,
    },
    /// Per-event sound notification settings loaded or updated from
    /// storage. Emitted once at startup (initial snapshot from
    /// `AppSettings.sounds`) and after every successful
    /// [`AppCommand::SetSoundSettings`] so the UI's
    /// `SoundController` can refresh its in-memory copy.
    SoundSettingsUpdated {
        events: Vec<(String, crate::sound::SoundEventSetting)>,
    },
    /// Streamer-mode preferences loaded or updated from storage.
    StreamerModeSettingsUpdated {
        mode: String,
        hide_link_previews: bool,
        hide_viewer_counts: bool,
        suppress_sounds: bool,
    },
    /// Effective streamer-mode active flag changed (driven by setting + detection).
    StreamerModeActiveChanged {
        active: bool,
    },
    /// An image upload started (show spinner / status banner).
    UploadStarted {
        channel: ChannelId,
    },
    /// An image upload finished. On success the URL is appended to the input
    /// buffer of `channel`; on failure the error text is surfaced as a toast /
    /// inline system notice.
    UploadFinished {
        channel: ChannelId,
        result: Result<String, String>,
    },
    /// Hype train lifecycle update from Twitch EventSub.  Emitted for the
    /// broadcaster's own channel once per EventSub notification (begin,
    /// progress, end).  The UI renders a live banner from the latest state
    /// until `phase == "end"` and the banner's configured cooldown elapses.
    HypeTrainUpdated {
        /// Target channel the hype train belongs to.
        channel: ChannelId,
        /// "begin", "progress", or "end".
        phase: String,
        /// Current level (1..=5, 0 when unavailable).
        level: u32,
        /// Points accumulated towards the next level.
        progress: u64,
        /// Points needed to reach the next level (0 on end events).
        goal: u64,
        /// Total points accumulated across the whole train.
        total: u64,
        /// Display name of the current top contributor.
        top_contributor_login: Option<String>,
        /// "bits" / "subscription" / other.
        top_contributor_type: Option<String>,
        /// Contribution size for the top contributor.
        top_contributor_total: Option<u64>,
        /// ISO 8601 timestamp the current phase expires (begin/progress) or
        /// the cooldown ends (end).
        ends_at: Option<String>,
    },
    /// A raid arrived for a channel. The UI reduces this into
    /// `AppState.raid_banners` so a prominent banner renders above the chat
    /// until dismissed or the TTL elapses. Also emitted for backfilled or
    /// EventSub-sourced raids so both IRC and EventSub paths converge.
    RaidBannerShown {
        channel: ChannelId,
        display_name: String,
        viewer_count: u32,
        source_login: Option<String>,
    },
    /// Latest list of currently-live followed channels, already sorted
    /// desc by viewer_count. Replaces `AppState.live_channels` wholesale.
    LiveFeedUpdated {
        channels: Vec<crate::model::LiveChannelSnapshot>,
    },
    /// Live-feed poll task failed; previous snapshot is preserved.
    LiveFeedError {
        message: String,
    },
    /// Partial live-feed snapshot accompanied by an error. Applied atomically
    /// so the UI doesn't flash through a "success cleared error" intermediate
    /// state before the matching error arrives.
    LiveFeedPartialUpdate {
        channels: Vec<crate::model::LiveChannelSnapshot>,
        error: String,
    },
}

// ConnectionState: connection status enumeration

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
    Error(String),
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::Disconnected
    }
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Disconnected"),
            ConnectionState::Connecting => write!(f, "Connecting..."),
            ConnectionState::Connected => write!(f, "Connected"),
            ConnectionState::Reconnecting { attempt } => {
                write!(f, "Reconnecting (attempt {attempt})...")
            }
            ConnectionState::Error(e) => write!(f, "Error: {e}"),
        }
    }
}

#[cfg(test)]
mod live_feed_event_tests {
    use super::*;
    use crate::model::LiveChannelSnapshot;

    #[test]
    fn live_feed_refresh_command_is_unit_variant() {
        // Construct it; pattern-match it; if the variant ever grows fields this
        // test will force a deliberate update.
        let cmd = AppCommand::LiveFeedRefresh;
        match cmd {
            AppCommand::LiveFeedRefresh => {}
            _ => panic!("expected LiveFeedRefresh"),
        }
    }

    #[test]
    fn live_feed_updated_event_constructs_and_carries_channels() {
        let snap = LiveChannelSnapshot {
            user_id: "1".into(),
            user_login: "a".into(),
            user_name: "A".into(),
            viewer_count: 42,
            thumbnail_url: String::new(),
            started_at: String::new(),
        };
        let evt = AppEvent::LiveFeedUpdated {
            channels: vec![snap],
        };
        match evt {
            AppEvent::LiveFeedUpdated { channels } => {
                assert_eq!(channels.len(), 1);
                assert_eq!(channels[0].viewer_count, 42);
            }
            _ => panic!("expected LiveFeedUpdated"),
        }
    }

    #[test]
    fn live_feed_error_event_carries_message() {
        let evt = AppEvent::LiveFeedError {
            message: "boom".into(),
        };
        match evt {
            AppEvent::LiveFeedError { message } => assert_eq!(message, "boom"),
            _ => panic!("expected LiveFeedError"),
        }
    }
}
