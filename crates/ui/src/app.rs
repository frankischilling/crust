use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use image::DynamicImage;
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{
        AppCommand, AppEvent, AutoModQueueItem, ConnectionState, LinkPreview,
        UnbanRequestItem,
    },
    model::{
        ChannelId, ChannelState, EmoteCatalogEntry, MsgKind, ReplyInfo, IRC_SERVER_CONTROL_CHANNEL,
    },
    AppState,
};

use crate::commands::render_help_message;
use crate::perf::{ChatPerfStats, PerfOverlay};
use crate::stream_status::{StreamStatusTracker, StreamStatusUpdate};
use crate::theme as t;
use crate::widgets::{
    analytics::AnalyticsPanel,
    bytes_uri,
    channel_list::ChannelList,
    chat_input::ChatInput,
    chrome::{self, ChromeIcon, IconButtonState},
    emote_picker::EmotePicker,
    emote_picker::EmotePickerPreferences,
    info_bars::{show_channel_info_bars, StreamStatusInfo},
    irc_status::IrcStatusPanel,
    join_dialog::JoinDialog,
    loading_screen::{LoadEvent, LoadingScreen},
    login_dialog::{LoginAction, LoginDialog},
    message_list::MessageList,
    message_search::{
        should_use_search_window, show_message_search_inline, show_message_search_window,
        MessageSearchState,
    },
    settings_page::{
        parse_settings_lines, show_settings_page, SettingsPageState, SettingsSection, SettingsStats,
    },
    split_header::{show_split_header, SPLIT_HEADER_HEIGHT},
    user_profile_popup::{PopupAction, UserProfilePopup},
};

// Channel layout mode

const REPAINT_ANIM_MS: u64 = 33;
const REPAINT_HOUSEKEEPING_MS: u64 = 2_000;
const STREAM_REFRESH_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const STREAM_REFRESH_ACTIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
const STREAM_REFRESH_BACKGROUND_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);
const STREAM_NOTIFICATION_STARTUP_GRACE: std::time::Duration =
    std::time::Duration::from_secs(8);
const AUTH_REFRESH_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const AUTH_REFRESH_INFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const EVENT_TOAST_MAX_ACTIVE: usize = 5;
const EVENT_TOAST_QUEUE_MAX: usize = 24;
const EVENT_TOAST_STAGGER: std::time::Duration = std::time::Duration::from_millis(650);
const EVENT_TOAST_TTL_SECS: f32 = 5.0;
const NARROW_WINDOW_THRESHOLD: f32 = 520.0;
const VERY_NARROW_WINDOW_THRESHOLD: f32 = 320.0;
const REGULAR_MIN_CENTRAL_WIDTH: f32 = 250.0;
const NARROW_MIN_CENTRAL_WIDTH: f32 = 120.0;
const REGULAR_STATUS_BAR_HEIGHT: f32 = 44.0;
const NARROW_STATUS_BAR_HEIGHT: f32 = 36.0;
const VERY_NARROW_STATUS_BAR_HEIGHT: f32 = 30.0;
const ANALYTICS_DEFAULT_W: f32 = 220.0;
const ANALYTICS_MIN_W: f32 = 180.0;
const ANALYTICS_MAX_W: f32 = 340.0;
const ANALYTICS_COMPACT_DEFAULT_W: f32 = 176.0;
const ANALYTICS_COMPACT_MIN_W: f32 = 140.0;
const ANALYTICS_COMPACT_MAX_W: f32 = 260.0;
const LOCAL_HISTORY_SEARCH_PAGE: usize = 800;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ResponsiveLayout {
    force_top_tabs: bool,
    status_bar_height: f32,
    min_central_width: f32,
    sidebar_default_width: f32,
    sidebar_min_width: f32,
    analytics_default_width: f32,
    analytics_min_width: f32,
    analytics_max_width: f32,
}

fn responsive_layout(window_width: f32) -> ResponsiveLayout {
    let narrow = window_width < NARROW_WINDOW_THRESHOLD;
    let very_narrow = window_width < VERY_NARROW_WINDOW_THRESHOLD;

    ResponsiveLayout {
        force_top_tabs: narrow,
        status_bar_height: if very_narrow {
            VERY_NARROW_STATUS_BAR_HEIGHT
        } else if narrow {
            NARROW_STATUS_BAR_HEIGHT
        } else {
            REGULAR_STATUS_BAR_HEIGHT
        },
        min_central_width: if narrow {
            NARROW_MIN_CENTRAL_WIDTH
        } else {
            REGULAR_MIN_CENTRAL_WIDTH
        },
        sidebar_default_width: if narrow {
            t::SIDEBAR_COMPACT_W
        } else {
            t::SIDEBAR_W
        },
        sidebar_min_width: if narrow {
            t::SIDEBAR_COMPACT_MIN_W
        } else {
            t::SIDEBAR_MIN_W
        },
        analytics_default_width: if narrow {
            ANALYTICS_COMPACT_DEFAULT_W
        } else {
            ANALYTICS_DEFAULT_W
        },
        analytics_min_width: if narrow {
            ANALYTICS_COMPACT_MIN_W
        } else {
            ANALYTICS_MIN_W
        },
        analytics_max_width: if narrow {
            ANALYTICS_COMPACT_MAX_W
        } else {
            ANALYTICS_MAX_W
        },
    }
}

/// A pop-in banner shown briefly for high-visibility chat events (Sub / Raid / Bits).
#[derive(Clone)]
struct EventToast {
    /// Fully-formatted display text (icon + message).
    text: String,
    /// Accent tint used for the border (Sub = gold, Raid = cyan, Bits = orange).
    hue: Color32,
    /// Whether to draw celebratory confetti particles around the toast.
    confetti: bool,
    /// Wall-clock moment the toast was created.
    born: std::time::Instant,
}

#[derive(Clone)]
struct PendingReply {
    channel: ChannelId,
    info: ReplyInfo,
}

// ── Split-pane state ─────────────────────────────────────────────────────

/// One pane in the split view.
#[derive(Clone)]
struct Pane {
    channel: ChannelId,
    input_buf: String,
    /// Width fraction (0.0–1.0) of available space; all panes sum to ~1.0.
    frac: f32,
}

/// Manages up to 4 side-by-side panes within the central area.
/// When `panes` is empty the app falls back to the classic single-channel view
/// driven by `active_channel`.
#[derive(Default, Clone)]
struct SplitPanes {
    /// Active pane slots. 0 = classic single-pane, 1+ = split.
    panes: Vec<Pane>,
    /// Index of the focused pane (receives keyboard input, shown in info bar).
    focused: usize,
}

impl SplitPanes {
    /// Ensure `focused` stays within bounds.
    fn clamp_focus(&mut self) {
        if !self.panes.is_empty() {
            self.focused = self.focused.min(self.panes.len() - 1);
        } else {
            self.focused = 0;
        }
    }

    /// The channel of the focused pane, if any.
    fn focused_channel(&self) -> Option<&ChannelId> {
        self.panes.get(self.focused).map(|p| &p.channel)
    }

    /// Ensure all pane fractions sum to 1.0 and none are too tiny.
    fn normalize_fractions(&mut self) {
        let n = self.panes.len();
        if n == 0 {
            return;
        }
        let min_frac = 0.10_f32;
        // Clamp minimums.
        for p in self.panes.iter_mut() {
            p.frac = p.frac.max(min_frac);
        }
        let sum: f32 = self.panes.iter().map(|p| p.frac).sum();
        if sum > 0.0 {
            for p in self.panes.iter_mut() {
                p.frac /= sum;
            }
        }
    }

    /// Add a channel to a new pane (at the given position or end).  Caps at 4.
    fn add_pane(&mut self, channel: ChannelId, insert_at: Option<usize>) {
        if self.panes.len() >= 4 {
            return;
        }
        let new_frac = 1.0 / (self.panes.len() as f32 + 1.0);
        // Shrink existing panes proportionally to make room.
        let scale = 1.0 - new_frac;
        for p in self.panes.iter_mut() {
            p.frac *= scale;
        }
        let pane = Pane {
            channel,
            input_buf: String::new(),
            frac: new_frac,
        };
        match insert_at {
            Some(i) if i <= self.panes.len() => self.panes.insert(i, pane),
            _ => self.panes.push(pane),
        }
        self.normalize_fractions();
        self.clamp_focus();
    }

    /// Remove a pane by index.
    fn remove_pane(&mut self, idx: usize) {
        if idx < self.panes.len() {
            self.panes.remove(idx);
            self.normalize_fractions();
            self.clamp_focus();
        }
    }
}

/// Controls where the channel list is rendered.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelLayout {
    /// Classic left sidebar (default).
    #[default]
    Sidebar,
    /// Compact horizontal tab strip pinned to the top of the window.
    TopTabs,
}

impl ChannelLayout {
    fn from_settings(value: &str) -> Self {
        match value {
            "top_tabs" => Self::TopTabs,
            _ => Self::Sidebar,
        }
    }

    fn as_settings(self) -> &'static str {
        match self {
            Self::Sidebar => "sidebar",
            Self::TopTabs => "top_tabs",
        }
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabVisualStyle {
    #[default]
    Compact,
    Normal,
}

impl TabVisualStyle {
    fn from_settings(value: &str) -> Self {
        match value {
            "normal" => Self::Normal,
            _ => Self::Compact,
        }
    }

    fn as_settings(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TopTabMetrics {
    strip_height: f32,
    chip_height: f32,
    label_width: f32,
    chip_pad_x: i8,
    chip_pad_y: i8,
    close_button_size: f32,
}

fn top_tab_metrics(window_width: f32, style: TabVisualStyle) -> TopTabMetrics {
    let narrow = window_width < 760.0;
    match style {
        TabVisualStyle::Compact => TopTabMetrics {
            strip_height: if narrow { 28.0 } else { 30.0 },
            chip_height: if narrow { 18.0 } else { 20.0 },
            label_width: if window_width < 520.0 {
                84.0
            } else if window_width < 860.0 {
                92.0
            } else {
                112.0
            },
            chip_pad_x: 6,
            chip_pad_y: 1,
            close_button_size: 12.0,
        },
        TabVisualStyle::Normal => TopTabMetrics {
            strip_height: if narrow { 34.0 } else { 36.0 },
            chip_height: if narrow { 22.0 } else { 24.0 },
            label_width: if window_width < 720.0 {
                108.0
            } else if window_width < 1020.0 {
                132.0
            } else {
                156.0
            },
            chip_pad_x: 8,
            chip_pad_y: 2,
            close_button_size: 14.0,
        },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ToolbarVisibility {
    compact_controls: bool,
    compact_account: bool,
    ultra_compact_account: bool,
    show_logo: bool,
    show_connection_label: bool,
    show_join_button: bool,
    show_join_text: bool,
    show_join_in_overflow: bool,
    show_sidebar_actions: bool,
    show_overflow_menu: bool,
    show_perf_toggle: bool,
    show_perf_in_overflow: bool,
    show_stats_toggle: bool,
    show_stats_in_overflow: bool,
    show_irc_toggle: bool,
    show_irc_in_overflow: bool,
    show_emote_count: bool,
}

const TOOLBAR_ROW_PADDING_W: f32 = 28.0;
const TOOLBAR_GROUP_FRAME_W: f32 = 14.0;
const TOOLBAR_SEPARATOR_W: f32 = 8.0;
const TOOLBAR_LOGO_W: f32 = 54.0;
const TOOLBAR_DOT_W: f32 = 12.0;
const TOOLBAR_CONN_LABEL_W: f32 = 92.0;
const TOOLBAR_JOIN_LABEL_W: f32 = 32.0;
const TOOLBAR_ACCOUNT_PILL_W: f32 = 118.0;
const TOOLBAR_OVERFLOW_W: f32 = 30.0;
const TOOLBAR_EMOTE_LABEL_W: f32 = 94.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolbarDegradeStep {
    HideEmoteCount,
    HideJoinText,
    HideConnectionLabel,
    HideLogo,
    HideIrcToggle,
    HideStatsToggle,
    HidePerfToggle,
    CompactAccount,
    CompactControls,
    UltraCompactAccount,
    HideSidebarActions,
    HideJoinButton,
}

fn estimate_icon_button_width(compact: bool) -> f32 {
    if compact {
        t::ICON_BTN_SM
    } else {
        t::ICON_BTN
    }
}

fn estimate_toolbar_group_width(icon_count: usize, icon_spacing: f32, compact: bool) -> f32 {
    if icon_count == 0 {
        return 0.0;
    }
    TOOLBAR_GROUP_FRAME_W
        + icon_count as f32 * estimate_icon_button_width(compact)
        + icon_count.saturating_sub(1) as f32 * icon_spacing
}

fn estimate_toolbar_required_width(visibility: &ToolbarVisibility) -> f32 {
    let mut width = TOOLBAR_ROW_PADDING_W;

    if visibility.show_logo {
        width += TOOLBAR_LOGO_W + 8.0;
    }

    let mut left_group = TOOLBAR_DOT_W;
    if visibility.show_connection_label {
        left_group += TOOLBAR_CONN_LABEL_W + 4.0;
    }
    if visibility.show_join_button {
        left_group += estimate_icon_button_width(visibility.compact_controls) + 4.0;
        if visibility.show_join_text {
            left_group += TOOLBAR_JOIN_LABEL_W + 4.0;
        }
    }
    width += TOOLBAR_GROUP_FRAME_W + left_group;

    if visibility.show_sidebar_actions {
        width += 6.0 + estimate_toolbar_group_width(2, 4.0, visibility.compact_controls);
    }

    let account_width = if visibility.compact_account {
        estimate_icon_button_width(visibility.ultra_compact_account)
    } else {
        TOOLBAR_ACCOUNT_PILL_W
    };
    width += 8.0 + account_width;

    if !visibility.compact_controls || visibility.show_emote_count || visibility.show_overflow_menu {
        width += TOOLBAR_SEPARATOR_W;
    }

    if visibility.show_overflow_menu {
        width += TOOLBAR_OVERFLOW_W;
        if visibility.show_emote_count {
            width += TOOLBAR_SEPARATOR_W;
        }
    }

    if visibility.show_emote_count {
        width += TOOLBAR_EMOTE_LABEL_W + TOOLBAR_SEPARATOR_W;
    }

    let mut icon_count = 1; // settings
    if visibility.show_perf_toggle {
        icon_count += 1;
    }
    if visibility.show_stats_toggle {
        icon_count += 1;
    }
    if visibility.show_irc_toggle {
        icon_count += 1;
    }
    width + estimate_toolbar_group_width(icon_count, 4.0, visibility.compact_controls)
}

fn apply_toolbar_degrade_step(visibility: &mut ToolbarVisibility, step: ToolbarDegradeStep) {
    match step {
        ToolbarDegradeStep::HideEmoteCount => visibility.show_emote_count = false,
        ToolbarDegradeStep::HideJoinText => visibility.show_join_text = false,
        ToolbarDegradeStep::HideConnectionLabel => visibility.show_connection_label = false,
        ToolbarDegradeStep::HideLogo => visibility.show_logo = false,
        ToolbarDegradeStep::HideIrcToggle => visibility.show_irc_toggle = false,
        ToolbarDegradeStep::HideStatsToggle => visibility.show_stats_toggle = false,
        ToolbarDegradeStep::HidePerfToggle => visibility.show_perf_toggle = false,
        ToolbarDegradeStep::CompactAccount => visibility.compact_account = true,
        ToolbarDegradeStep::CompactControls => visibility.compact_controls = true,
        ToolbarDegradeStep::UltraCompactAccount => visibility.ultra_compact_account = true,
        ToolbarDegradeStep::HideSidebarActions => visibility.show_sidebar_actions = false,
        ToolbarDegradeStep::HideJoinButton => {
            visibility.show_join_button = false;
            visibility.show_join_text = false;
            visibility.show_join_in_overflow = true;
        }
    }
}

fn finalize_toolbar_visibility(visibility: &mut ToolbarVisibility, irc_beta_enabled: bool) {
    if !irc_beta_enabled {
        visibility.show_irc_toggle = false;
        visibility.show_irc_in_overflow = false;
    } else {
        visibility.show_irc_in_overflow = !visibility.show_irc_toggle;
    }

    visibility.show_perf_in_overflow = !visibility.show_perf_toggle;
    visibility.show_stats_in_overflow = !visibility.show_stats_toggle;

    if !visibility.show_join_button {
        visibility.show_join_text = false;
        visibility.show_join_in_overflow = true;
    }

    visibility.show_overflow_menu = visibility.show_join_in_overflow
        || visibility.show_perf_in_overflow
        || visibility.show_stats_in_overflow
        || visibility.show_irc_in_overflow;
}

fn toolbar_visibility(bar_width: f32, irc_beta_enabled: bool) -> ToolbarVisibility {
    let mut visibility = ToolbarVisibility {
        compact_controls: false,
        compact_account: false,
        ultra_compact_account: false,
        show_logo: true,
        show_connection_label: true,
        show_join_button: true,
        show_join_text: true,
        show_join_in_overflow: false,
        show_sidebar_actions: true,
        show_overflow_menu: false,
        show_perf_toggle: true,
        show_perf_in_overflow: false,
        show_stats_toggle: true,
        show_stats_in_overflow: false,
        show_irc_toggle: irc_beta_enabled,
        show_irc_in_overflow: false,
        show_emote_count: true,
    };

    const STEPS: [ToolbarDegradeStep; 12] = [
        ToolbarDegradeStep::HideEmoteCount,
        ToolbarDegradeStep::HideJoinText,
        ToolbarDegradeStep::HideConnectionLabel,
        ToolbarDegradeStep::HideLogo,
        ToolbarDegradeStep::HideIrcToggle,
        ToolbarDegradeStep::HideStatsToggle,
        ToolbarDegradeStep::HidePerfToggle,
        ToolbarDegradeStep::CompactAccount,
        ToolbarDegradeStep::CompactControls,
        ToolbarDegradeStep::UltraCompactAccount,
        ToolbarDegradeStep::HideSidebarActions,
        ToolbarDegradeStep::HideJoinButton,
    ];

    for step in STEPS {
        finalize_toolbar_visibility(&mut visibility, irc_beta_enabled);
        if estimate_toolbar_required_width(&visibility) <= bar_width {
            break;
        }
        apply_toolbar_degrade_step(&mut visibility, step);
    }

    finalize_toolbar_visibility(&mut visibility, irc_beta_enabled);

    // Extreme fallback to keep controls coherent on tiny windows.
    if estimate_toolbar_required_width(&visibility) > bar_width {
        visibility.compact_controls = true;
        visibility.compact_account = true;
        visibility.ultra_compact_account = true;
        visibility.show_logo = false;
        visibility.show_connection_label = false;
        visibility.show_join_text = false;
        visibility.show_sidebar_actions = false;
        visibility.show_perf_toggle = false;
        visibility.show_stats_toggle = false;
        visibility.show_irc_toggle = false;
        if bar_width < 300.0 {
            visibility.show_join_button = false;
            visibility.show_join_in_overflow = true;
        }
        finalize_toolbar_visibility(&mut visibility, irc_beta_enabled);
    }

    visibility
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AppearanceSnapshot {
    sidebar_visible: bool,
    channel_layout: ChannelLayout,
    analytics_visible: bool,
    irc_status_visible: bool,
    tab_style: TabVisualStyle,
    show_tab_close_buttons: bool,
    show_tab_live_indicators: bool,
    split_header_show_title: bool,
    split_header_show_game: bool,
    split_header_show_viewer_count: bool,
}

/// Upper bound for usernames tracked per channel for @autocomplete.
/// Keeps long-running channels from turning per-frame work into O(hours).
const MAX_TRACKED_CHATTERS: usize = 5_000;

// CrustApp struct and implementation

pub struct CrustApp {
    pub state: AppState,
    cmd_tx: mpsc::Sender<AppCommand>,
    event_rx: mpsc::Receiver<AppEvent>,
    emote_bytes: HashMap<String, (u32, u32, Arc<[u8]>)>,
    join_dialog: JoinDialog,
    login_dialog: LoginDialog,
    emote_picker: EmotePicker,
    chat_input_buf: String,
    emote_catalog: Vec<EmoteCatalogEntry>,
    perf: PerfOverlay,
    /// Reply pending for the next send (set by right-click → Reply).
    pending_reply: Option<PendingReply>,
    /// User profile card shown when clicking a username.
    user_profile_popup: UserProfilePopup,
    /// Cached link previews (Open-Graph metadata) keyed by URL.
    link_previews: HashMap<String, LinkPreview>,
    /// Running total of raw emote bytes - updated incrementally on EmoteImageReady
    /// so we don't iterate the entire map every frame.
    emote_ram_bytes: usize,
    /// Chat message history for Up/Down arrow recall.
    message_history: Vec<String>,
    /// Slash command usage frequency for autocomplete ranking.
    slash_usage_counts: HashMap<String, u32>,
    /// Controls whether the left channel sidebar is visible (Sidebar mode only).
    sidebar_visible: bool,
    /// Where channel tabs are rendered: left sidebar or top strip.
    channel_layout: ChannelLayout,
    /// Compact vs normal tab density.
    tab_style: TabVisualStyle,
    /// Whether tabs render close buttons on hover/selection.
    show_tab_close_buttons: bool,
    /// Whether tabs render live dots for live channels.
    show_tab_live_indicators: bool,
    /// Chatter analytics right panel.
    analytics_panel: AnalyticsPanel,
    /// Whether the analytics panel is visible.
    analytics_visible: bool,
    /// IRC diagnostics/status window.
    irc_status_panel: IrcStatusPanel,
    /// Whether the IRC status window is visible.
    irc_status_visible: bool,
    /// Startup loading overlay (shown until initial emotes + history are ready).
    loading_screen: LoadingScreen,
    /// Cached stream status per channel (key = channel login, lowercase).
    stream_statuses: HashMap<String, StreamStatusInfo>,
    /// When each channel's stream status was last fetched.
    stream_status_fetched: HashMap<String, std::time::Instant>,
    /// Channel logins currently being fetched for stream status.
    stream_status_fetch_inflight: HashSet<String>,
    /// Last time we scanned channels to schedule stale stream-status refreshes.
    last_stream_refresh_scan: std::time::Instant,
    /// Last time we forced a refresh for the active Twitch channel.
    last_active_stream_refresh: std::time::Instant,
    /// Cached live-status map derived from `stream_statuses`; rebuilt only on
    /// change rather than every frame.
    live_map_cache: HashMap<String, bool>,
    /// Tracks watched channels and suppresses duplicate live/offline transitions.
    stream_tracker: StreamStatusTracker,
    /// Short-lived pop-in banners for Sub / Raid / Bits events (cap 5).
    event_toasts: Vec<EventToast>,
    /// Backlog of toasts waiting for paced rendering.
    event_toast_queue: VecDeque<EventToast>,
    /// Last time a toast was emitted from the queue.
    last_event_toast_emit: Option<std::time::Instant>,
    /// Suppress stream live/offline toasts during startup sync.
    suppress_stream_toasts_until: std::time::Instant,
    /// Settings dialog visibility.
    settings_open: bool,
    /// Current section selected in the settings page.
    settings_section: SettingsSection,
    /// Persisted Kick compatibility (beta) toggle.
    kick_beta_enabled: bool,
    /// Persisted IRC compatibility (beta) toggle.
    irc_beta_enabled: bool,
    /// NickServ username for IRC auto-identification.
    irc_nickserv_user: String,
    /// NickServ password for IRC auto-identification.
    irc_nickserv_pass: String,
    /// Window always-on-top mode.
    always_on_top: bool,
    /// Twitch overflow handling mode:
    /// `true` = Prevent, `false` = Highlight.
    prevent_overlong_twitch_messages: bool,
    /// Collapse long messages in chat rendering.
    collapse_long_messages: bool,
    /// Maximum visible lines before collapsing.
    collapse_long_message_lines: usize,
    /// Only animate while the window is focused.
    animations_when_focused: bool,
    /// Show chat timestamps before each message.
    show_timestamps: bool,
    /// Include seconds in chat timestamps.
    show_timestamp_seconds: bool,
    /// Use 24-hour clock formatting for chat timestamps.
    use_24h_timestamps: bool,
    /// Persist incoming chat rows to local SQLite history.
    local_log_indexing_enabled: bool,
    /// Whether split headers show stream title metadata.
    split_header_show_title: bool,
    /// Whether split headers show stream game/category metadata.
    split_header_show_game: bool,
    /// Whether split headers show viewer counts.
    split_header_show_viewer_count: bool,
    /// Highlight keyword list from settings.
    highlights: Vec<String>,
    /// Compiled highlight rules (substring / regex / scope) used by message rendering.
    highlight_rules: Vec<crust_core::highlight::HighlightMatch>,
    /// Settings dialog state for highlight rules
    settings_highlight_rules: Vec<crust_core::highlight::HighlightRule>,
    settings_highlight_rule_bufs: Vec<String>,
    
    /// Compiled filter records for hiding messages.
    filter_records: Vec<crust_core::model::filters::CompiledFilter>,
    /// Settings dialog state for filter records.
    settings_filter_records: Vec<crust_core::model::filters::FilterRecord>,
    settings_filter_record_bufs: Vec<String>,
    
    /// User-defined moderation action presets
    mod_action_presets: Vec<crust_core::model::mod_actions::ModActionPreset>,
    settings_mod_action_presets: Vec<crust_core::model::mod_actions::ModActionPreset>,

    desktop_notifications_enabled: bool,
    /// Ignored usernames from settings.
    ignores: Vec<String>,
    /// Fast lookup set for ignored usernames.
    ignores_set: HashSet<String>,
    /// Auto-join channel list from settings.
    auto_join_channels: Vec<String>,
    /// Editable multiline text buffer for highlight keywords.
    highlights_buf: String,
    /// Editable multiline text buffer for ignored usernames.
    ignores_buf: String,
    /// Editable multiline text buffer for auto-join channels.
    auto_join_buf: String,
    /// 7TV animated avatar URLs keyed by Twitch user ID.
    stv_avatars: HashMap<String, String>,
    /// Cached static avatar textures used to freeze animated avatars in always-visible UI.
    static_avatar_frames: HashMap<String, egui::TextureHandle>,
    /// Split-pane state for multi-channel side-by-side view.
    split_panes: SplitPanes,
    /// Per-channel message search and filter state.
    message_search: HashMap<ChannelId, MessageSearchState>,
    /// Sorted chatter names per channel, rebuilt only when membership changes.
    sorted_chatters: HashMap<ChannelId, Vec<String>>,
    /// Last emote picker preferences acknowledged by runtime settings.
    emote_picker_prefs_last_saved: Option<EmotePickerPreferences>,
    /// Moderation tools dialog visibility.
    mod_tools_open: bool,
    /// Held AutoMod queue keyed by channel.
    automod_queue: HashMap<ChannelId, Vec<AutoModQueueItem>>,
    /// Pending unban requests keyed by channel.
    unban_requests: HashMap<ChannelId, Vec<UnbanRequestItem>>,
    /// Draft resolution text keyed by `channel::request_id`.
    unban_resolution_drafts: HashMap<String, String>,
    /// True while a background auth refresh is in-flight.
    auth_refresh_inflight: bool,
    /// Last time we attempted an auth refresh after a 401/AuthExpired event.
    last_auth_refresh_attempt: Option<std::time::Instant>,
}

/// Apply the Crust colour palette to egui, reading the current dark/light
/// flag from `theme::is_light()`.  Called once at startup and again whenever
/// the user toggles the theme.
fn apply_theme_visuals(ctx: &egui::Context) {
    let mut vis = if t::is_light() {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    vis.override_text_color = Some(t::text_primary());
    vis.panel_fill = t::bg_base();
    vis.window_fill = t::bg_dialog();
    vis.extreme_bg_color = t::bg_raised(); // TextEdit / ComboBox fill

    vis.widgets.inactive.weak_bg_fill = t::bg_surface();
    vis.widgets.inactive.bg_fill = t::bg_surface();
    vis.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, t::text_secondary());
    vis.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, t::border_subtle());
    vis.widgets.inactive.corner_radius = t::RADIUS;

    vis.widgets.hovered.weak_bg_fill = t::hover_bg();
    vis.widgets.hovered.bg_fill = t::hover_bg();
    vis.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, t::text_primary());
    vis.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, t::border_accent());
    vis.widgets.hovered.corner_radius = t::RADIUS;

    vis.widgets.active.weak_bg_fill = t::accent_dim();
    vis.widgets.active.bg_fill = t::accent_dim();
    vis.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::WHITE);
    vis.widgets.active.bg_stroke = egui::Stroke::new(1.0, t::accent());
    vis.widgets.active.corner_radius = t::RADIUS;

    vis.widgets.open.weak_bg_fill = t::bg_raised();
    vis.widgets.open.bg_fill = t::bg_raised();

    vis.selection.bg_fill = t::accent_dim();
    vis.selection.stroke = egui::Stroke::new(1.0, t::accent());

    vis.window_corner_radius = t::RADIUS;
    vis.window_stroke = t::stroke_subtle();
    vis.menu_corner_radius = t::RADIUS;
    vis.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, t::border_subtle());

    let mut style = egui::Style {
        visuals: vis,
        ..(*ctx.style()).clone()
    };
    style.spacing.item_spacing = t::ITEM_SPACING;
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = Margin::same(10);
    style.interaction.tooltip_delay = 0.0;
    style.interaction.tooltip_grace_time = 0.5;
    ctx.set_style(style);
}

impl CrustApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd_tx: mpsc::Sender<AppCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // -- Visuals -----------------------------------------------------------
        apply_theme_visuals(&cc.egui_ctx);

        install_system_fallback_fonts(&cc.egui_ctx);

        // Eagerly initialise the spell-check dictionary so the first
        // right-click context menu doesn't stall.
        crate::spellcheck::init();

        Self {
            state: AppState::default(),
            cmd_tx,
            event_rx,
            emote_bytes: HashMap::new(),
            join_dialog: JoinDialog::default(),
            login_dialog: LoginDialog::default(),
            emote_picker: EmotePicker::default(),
            chat_input_buf: String::new(),
            emote_catalog: Vec::new(),
            perf: PerfOverlay::default(),
            pending_reply: None,
            user_profile_popup: UserProfilePopup::default(),
            link_previews: HashMap::new(),
            emote_ram_bytes: 0,
            message_history: Vec::new(),
            slash_usage_counts: HashMap::new(),
            sidebar_visible: true,
            channel_layout: ChannelLayout::default(),
            tab_style: TabVisualStyle::default(),
            show_tab_close_buttons: true,
            show_tab_live_indicators: true,
            analytics_panel: AnalyticsPanel::default(),
            analytics_visible: false,
            irc_status_panel: IrcStatusPanel::default(),
            irc_status_visible: false,
            loading_screen: LoadingScreen::default(),
            stream_statuses: HashMap::new(),
            stream_status_fetched: HashMap::new(),
            stream_status_fetch_inflight: HashSet::new(),
            last_stream_refresh_scan: std::time::Instant::now(),
            last_active_stream_refresh: std::time::Instant::now(),
            live_map_cache: HashMap::new(),
            stream_tracker: StreamStatusTracker::default(),
            event_toasts: Vec::new(),
            event_toast_queue: VecDeque::new(),
            last_event_toast_emit: None,
            suppress_stream_toasts_until: std::time::Instant::now()
                + STREAM_NOTIFICATION_STARTUP_GRACE,
            settings_open: false,
            settings_section: SettingsSection::default(),
            kick_beta_enabled: false,
            irc_beta_enabled: false,
            irc_nickserv_user: String::new(),
            irc_nickserv_pass: String::new(),
            always_on_top: false,
            prevent_overlong_twitch_messages: true,
            collapse_long_messages: true,
            collapse_long_message_lines: 8,
            animations_when_focused: true,
            show_timestamps: true,
            show_timestamp_seconds: false,
            use_24h_timestamps: true,
            local_log_indexing_enabled: true,
            split_header_show_title: true,
            split_header_show_game: false,
            split_header_show_viewer_count: true,
            highlights: Vec::new(),
            highlight_rules: Vec::new(),
            settings_highlight_rules: Vec::new(),
            settings_highlight_rule_bufs: Vec::new(),
            filter_records: Vec::new(),
            settings_filter_records: Vec::new(),
            settings_filter_record_bufs: Vec::new(),
            mod_action_presets: Vec::new(),
            settings_mod_action_presets: Vec::new(),
            desktop_notifications_enabled: false,
            ignores: Vec::new(),
            ignores_set: HashSet::new(),
            auto_join_channels: Vec::new(),
            highlights_buf: String::new(),
            ignores_buf: String::new(),
            auto_join_buf: String::new(),
            stv_avatars: HashMap::new(),
            static_avatar_frames: HashMap::new(),
            split_panes: SplitPanes::default(),
            message_search: HashMap::new(),
            sorted_chatters: HashMap::new(),
            emote_picker_prefs_last_saved: None,
            mod_tools_open: false,
            automod_queue: HashMap::new(),
            unban_requests: HashMap::new(),
            unban_resolution_drafts: HashMap::new(),
            auth_refresh_inflight: false,
            last_auth_refresh_attempt: None,
        }
    }

    fn appearance_snapshot(&self) -> AppearanceSnapshot {
        AppearanceSnapshot {
            sidebar_visible: self.sidebar_visible,
            channel_layout: self.channel_layout,
            analytics_visible: self.analytics_visible,
            irc_status_visible: self.irc_status_visible,
            tab_style: self.tab_style,
            show_tab_close_buttons: self.show_tab_close_buttons,
            show_tab_live_indicators: self.show_tab_live_indicators,
            split_header_show_title: self.split_header_show_title,
            split_header_show_game: self.split_header_show_game,
            split_header_show_viewer_count: self.split_header_show_viewer_count,
        }
    }

    fn send_appearance_settings(&self) {
        self.send_cmd(AppCommand::SetAppearanceSettings {
            channel_layout: self.channel_layout.as_settings().to_owned(),
            sidebar_visible: self.sidebar_visible,
            analytics_visible: self.analytics_visible,
            irc_status_visible: self.irc_status_visible,
            tab_style: self.tab_style.as_settings().to_owned(),
            show_tab_close_buttons: self.show_tab_close_buttons,
            show_tab_live_indicators: self.show_tab_live_indicators,
            split_header_show_title: self.split_header_show_title,
            split_header_show_game: self.split_header_show_game,
            split_header_show_viewer_count: self.split_header_show_viewer_count,
        });
    }

    fn request_stream_status_refresh(&mut self, login: &str) {
        let login = login.trim().to_ascii_lowercase();
        if !is_valid_twitch_login(&login) {
            return;
        }
        if self.stream_status_fetch_inflight.contains(&login) {
            return;
        }

        self.stream_status_fetch_inflight.insert(login.clone());
        self.send_cmd(AppCommand::FetchUserProfile { login });
    }

    fn request_user_attention(&self, ctx: &Context, attention: egui::UserAttentionType) {
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(attention));
    }

    fn truncate_notification_text(text: &str, max_chars: usize) -> String {
        let mut flattened = text.trim().replace('\n', " ");
        if flattened.chars().count() <= max_chars {
            return flattened;
        }

        let keep = max_chars.saturating_sub(3);
        let mut truncated = String::with_capacity(max_chars);
        for (i, ch) in flattened.chars().enumerate() {
            if i >= keep {
                break;
            }
            truncated.push(ch);
        }
        truncated.push_str("...");
        flattened.clear();
        truncated
    }

    fn dispatch_desktop_notification(&self, title: &str, body: &str, with_sound: bool) {
        #[cfg(target_os = "windows")]
        {
            if with_sound {
                // Best-effort audible cue that does not depend on toast support.
                Self::play_windows_notification_beep();
            }

            let mut shell_candidates: Vec<std::path::PathBuf> = Vec::new();
            if let Some(windir) = std::env::var_os("WINDIR") {
                let base = std::path::PathBuf::from(windir);
                shell_candidates.push(
                    base.join("System32")
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join("powershell.exe"),
                );
                shell_candidates.push(
                    base.join("Sysnative")
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join("powershell.exe"),
                );
            }
            shell_candidates.push(std::path::PathBuf::from("powershell.exe"));
            shell_candidates.push(std::path::PathBuf::from("powershell"));
            shell_candidates.push(std::path::PathBuf::from("pwsh.exe"));
            shell_candidates.push(std::path::PathBuf::from("pwsh"));

            let title = title.to_owned();
            let body = body.to_owned();
            let sound_flag = if with_sound {
                "1".to_owned()
            } else {
                "0".to_owned()
            };

            std::thread::spawn(move || {
                let script = r#"
$ErrorActionPreference = 'Stop'
$title = $env:CRUST_NOTIFY_TITLE
$body = $env:CRUST_NOTIFY_BODY
$withSound = $env:CRUST_NOTIFY_SOUND -eq '1'
$shown = $false

try {
  [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
  [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null

  $xml = New-Object Windows.Data.Xml.Dom.XmlDocument
  $xml.LoadXml('<toast><visual><binding template="ToastGeneric"><text></text><text></text></binding></visual></toast>')

  $textNodes = $xml.GetElementsByTagName('text')
  $textNodes.Item(0).InnerText = $title
  $textNodes.Item(1).InnerText = $body

  $audio = $xml.CreateElement('audio')
  if ($withSound) {
    $audio.SetAttribute('src', 'ms-winsoundevent:Notification.Default')
  } else {
    $audio.SetAttribute('silent', 'true')
  }
  $xml.DocumentElement.AppendChild($audio) > $null

  $toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
    $notifier = [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('PowerShell')
    if ($notifier.Setting.ToString() -eq 'Enabled') {
        $notifier.Show($toast)
        $shown = $true
    }
} catch {}

if (-not $shown) {
    try {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing

        $form = New-Object System.Windows.Forms.Form
        $form.Text = $title
        $form.StartPosition = 'Manual'
        $form.FormBorderStyle = 'FixedToolWindow'
        $form.ShowInTaskbar = $false
        $form.TopMost = $true
        $form.Width = 380
        $form.Height = 120

        $work = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
        $x = [Math]::Max(0, $work.Right - $form.Width - 16)
        $y = [Math]::Max(0, $work.Bottom - $form.Height - 16)
        $form.Location = New-Object System.Drawing.Point($x, $y)

        $label = New-Object System.Windows.Forms.Label
        $label.AutoSize = $false
        $label.Left = 12
        $label.Top = 12
        $label.Width = 352
        $label.Height = 64
        $label.Text = $body
        $label.Font = New-Object System.Drawing.Font('Segoe UI', 9)
        $form.Controls.Add($label)

        $timer = New-Object System.Windows.Forms.Timer
        $timer.Interval = 4200
        $timer.add_Tick({
            $timer.Stop()
            $form.Close()
        })
        $form.add_Shown({ $timer.Start() })

        [void]$form.ShowDialog()
        $shown = $true
    } catch {}
}

if (-not $shown -and $withSound) {
  try { [System.Media.SystemSounds]::Asterisk.Play() } catch {}
}

if ($shown) {
  exit 0
}

exit 1

"#;

                let mut delivered = false;
                let mut last_error: Option<String> = None;
                for shell in shell_candidates {
                    let mut cmd = std::process::Command::new(&shell);
                    #[cfg(target_os = "windows")]
                    {
                        use std::os::windows::process::CommandExt as _;
                        const DETACHED_PROCESS: u32 = 0x0000_0008;
                        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
                    }
                    cmd.arg("-NoProfile")
                        .arg("-STA")
                        .arg("-NonInteractive")
                        .arg("-ExecutionPolicy")
                        .arg("Bypass")
                        .arg("-WindowStyle")
                        .arg("Hidden")
                        .arg("-Command")
                        .arg(script)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .env("CRUST_NOTIFY_TITLE", &title)
                        .env("CRUST_NOTIFY_BODY", &body)
                        .env("CRUST_NOTIFY_SOUND", &sound_flag);

                    match cmd.status() {
                        Ok(status) if status.success() => {
                            delivered = true;
                            break;
                        }
                        Ok(status) => {
                            last_error = Some(format!("{} (exit code {:?})", shell.display(), status.code()));
                        }
                        Err(error) => {
                            last_error = Some(format!("{} ({error})", shell.display()));
                        }
                    }
                }

                if !delivered {
                    if let Some(error) = last_error {
                        warn!("Failed to dispatch desktop notification popup: {error}");
                    } else {
                        warn!("Failed to dispatch desktop notification popup: no shell candidates available");
                    }
                }
            });
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = (title, body, with_sound);
        }
    }

    #[cfg(target_os = "windows")]
    fn play_windows_notification_beep() {
        // SAFETY: These Win32 calls use documented constants/pointers and do
        // not transfer ownership.
        unsafe {
            #[link(name = "winmm")]
            unsafe extern "system" {
                fn PlaySoundW(
                    psz_sound: *const u16,
                    hmod: *mut core::ffi::c_void,
                    fdw_sound: u32,
                ) -> i32;
                fn MessageBeep(u_type: u32) -> i32;
                fn Beep(freq: u32, duration: u32) -> i32;
            }

            const SND_ASYNC: u32 = 0x0000_0001;
            const SND_NODEFAULT: u32 = 0x0000_0002;
            const SND_ALIAS: u32 = 0x0001_0000;
            const SND_SYSTEM: u32 = 0x0020_0000;
            let flags = SND_ALIAS | SND_ASYNC | SND_SYSTEM | SND_NODEFAULT;

            for alias in ["SystemNotification", "SystemAsterisk", "SystemExclamation"] {
                let wide: Vec<u16> = alias.encode_utf16().chain(std::iter::once(0)).collect();
                if PlaySoundW(wide.as_ptr(), std::ptr::null_mut(), flags) != 0 {
                    return;
                }
            }

            // Fallback chain if alias playback is unavailable.
            let _ = MessageBeep(0x0000_0040);
            let _ = Beep(880, 120);
        }
    }

    fn push_event_toast(&mut self, text: String, hue: Color32, confetti: bool) {
        self.enqueue_event_toast(EventToast {
            text,
            hue,
            confetti,
            born: std::time::Instant::now(),
        });
    }

    fn enqueue_event_toast(&mut self, toast: EventToast) {
        if self.event_toast_queue.len() >= EVENT_TOAST_QUEUE_MAX {
            self.event_toast_queue.pop_front();
        }
        self.event_toast_queue.push_back(toast);
        self.flush_event_toast_queue();
    }

    fn flush_event_toast_queue(&mut self) {
        let now = std::time::Instant::now();
        let can_emit = self
            .last_event_toast_emit
            .map(|last| now.duration_since(last) >= EVENT_TOAST_STAGGER)
            .unwrap_or(true);

        if !can_emit {
            return;
        }

        let Some(mut toast) = self.event_toast_queue.pop_front() else {
            return;
        };

        toast.born = now;
        if self.event_toasts.len() >= EVENT_TOAST_MAX_ACTIVE {
            self.event_toasts.remove(0);
        }
        self.event_toasts.push(toast);
        self.last_event_toast_emit = Some(now);
    }

    fn handle_stream_status_transition(
        &mut self,
        ctx: &Context,
        login: &str,
        is_live: bool,
        title: Option<String>,
        game: Option<String>,
        viewers: Option<u64>,
    ) {
        use crust_core::notifications::Platform;

        let viewer_count = viewers.map(|v| v.min(u32::MAX as u64) as u32);

        if !self.stream_tracker.is_watching(login, Platform::Twitch) {
            self.stream_tracker
                .watch_channel(login.to_owned(), Platform::Twitch, None);
        }

        let Some(update) = self.stream_tracker.update_stream_status(
            login,
            Platform::Twitch,
            is_live,
            title,
            game,
            viewer_count,
        ) else {
            return;
        };

        let suppress_stream_toasts = self.loading_screen.is_active()
            || std::time::Instant::now() < self.suppress_stream_toasts_until;

        match update {
            StreamStatusUpdate::Live(payload) => {
                if suppress_stream_toasts {
                    return;
                }
                let mut text = format!("{} is live", payload.display_name);
                if let Some(stream_title) = payload.title.as_deref().filter(|s| !s.is_empty()) {
                    text = format!("{} is live: {}", payload.display_name, stream_title);
                }
                self.push_event_toast(text, t::raid_cyan(), false);
                if self.desktop_notifications_enabled {
                    self.request_user_attention(ctx, egui::UserAttentionType::Informational);
                }
            }
            StreamStatusUpdate::Offline(payload) => {
                if suppress_stream_toasts {
                    return;
                }
                self.push_event_toast(format!("{} went offline", payload.channel_name), t::text_muted(), false);
            }
        }
    }

    fn active_moderation_channel(&self) -> Option<ChannelId> {
        let active = self.state.active_channel.as_ref()?;
        if !active.is_twitch() {
            return None;
        }
        let ch = self.state.channels.get(active)?;
        let is_broadcaster = self
            .state
            .auth
            .username
            .as_deref()
            .map(|u| u.eq_ignore_ascii_case(active.display_name()))
            .unwrap_or(false);
        if ch.is_mod || is_broadcaster {
            Some(active.clone())
        } else {
            None
        }
    }

    fn unban_draft_key(channel: &ChannelId, request_id: &str) -> String {
        format!("{}::{request_id}", channel.as_str())
    }

    fn normalize_unban_requests(requests: &mut Vec<UnbanRequestItem>) {
        requests.retain(|request| {
            request
                .status
                .as_deref()
                .map(|status| status.eq_ignore_ascii_case("pending"))
                .unwrap_or(true)
        });

        requests.sort_by(|a, b| {
            b.created_at
                .as_deref()
                .cmp(&a.created_at.as_deref())
                .then_with(|| a.request_id.cmp(&b.request_id))
        });

        let mut seen_ids = HashSet::new();
        requests.retain(|request| seen_ids.insert(request.request_id.clone()));
    }

    fn drain_events(&mut self, ctx: &Context) -> u32 {
        const MAX_EVENTS_PER_FRAME: u32 = 200;
        let mut count = 0u32;
        while let Ok(evt) = self.event_rx.try_recv() {
            self.apply_event(evt, ctx);
            count += 1;
            if count >= MAX_EVENTS_PER_FRAME {
                // More events remain - schedule another repaint so we
                // drain them across multiple frames instead of stalling.
                ctx.request_repaint();
                break;
            }
        }
        count
    }

    fn apply_event(&mut self, evt: AppEvent, ctx: &Context) {
        self.irc_status_panel.on_event(&evt);

        // Feed the loading screen before the main state update.
        match &evt {
            AppEvent::ConnectionStateChanged { state } => {
                use crust_core::events::ConnectionState;
                match state {
                    ConnectionState::Connecting | ConnectionState::Reconnecting { .. } => {
                        self.loading_screen.on_event(LoadEvent::Connecting)
                    }
                    ConnectionState::Connected => {
                        self.loading_screen.on_event(LoadEvent::Connected)
                    }
                    _ => {}
                }
            }
            AppEvent::Authenticated { username, .. } => {
                self.loading_screen.on_event(LoadEvent::Authenticated {
                    username: username.clone(),
                })
            }
            AppEvent::ChannelJoined { channel } => {
                self.loading_screen.on_event(LoadEvent::ChannelJoined {
                    channel: channel.as_str().to_owned(),
                })
            }
            AppEvent::EmoteCatalogUpdated { emotes } => {
                self.loading_screen.on_event(LoadEvent::CatalogLoaded {
                    count: emotes.len(),
                })
            }
            AppEvent::HistoryLoaded { channel, messages } => {
                self.loading_screen.on_event(LoadEvent::HistoryLoaded {
                    channel: channel.as_str().to_owned(),
                    count: messages.len(),
                })
            }
            AppEvent::ChannelEmotesLoaded { channel, count } => {
                self.loading_screen
                    .on_event(LoadEvent::ChannelEmotesLoaded {
                        channel: channel.as_str().to_owned(),
                        count: *count,
                    })
            }
            AppEvent::ImagePrefetchQueued { count } => self
                .loading_screen
                .on_event(LoadEvent::ImagePrefetchQueued { count: *count }),
            AppEvent::EmoteImageReady { .. } => {
                self.loading_screen.on_event(LoadEvent::EmoteImageReady)
            }
            _ => {}
        }

        match evt {
            AppEvent::ConnectionStateChanged { state } => {
                self.state.connection = state;
            }
            AppEvent::ChannelJoined { channel } => {
                self.state.join_channel(channel.clone());
                self.sorted_chatters.entry(channel.clone()).or_default();
                // Kick off an immediate stream-status fetch for the new channel (Twitch only).
                if channel.is_twitch() {
                    let login = channel.display_name().to_ascii_lowercase();
                    self.stream_tracker.watch_channel(
                        login.clone(),
                        crust_core::notifications::Platform::Twitch,
                        None,
                    );
                    self.request_stream_status_refresh(&login);
                }
            }
            AppEvent::ChannelParted { channel } => {
                self.state.leave_channel(&channel);
                self.sorted_chatters.remove(&channel);
                self.automod_queue.remove(&channel);
                self.unban_requests.remove(&channel);
                let prefix = format!("{}::", channel.as_str());
                self.unban_resolution_drafts
                    .retain(|k, _| !k.starts_with(&prefix));
                if channel.is_twitch() {
                    let login = channel.display_name().to_ascii_lowercase();
                    self.stream_tracker.unwatch_channel(
                        &login,
                        crust_core::notifications::Platform::Twitch,
                    );
                    self.stream_statuses.remove(&login);
                    self.live_map_cache.remove(&login);
                    self.stream_status_fetched.remove(&login);
                    self.stream_status_fetch_inflight.remove(&login);
                }
                if self
                    .pending_reply
                    .as_ref()
                    .map(|r| r.channel == channel)
                    .unwrap_or(false)
                {
                    self.pending_reply = None;
                }
            }
            AppEvent::ChannelRedirected {
                old_channel,
                new_channel,
            } => {
                self.state.redirect_channel(&old_channel, &new_channel);
                if let Some(cached) = self.sorted_chatters.remove(&old_channel) {
                    self.sorted_chatters.insert(new_channel.clone(), cached);
                }
                if let Some(queue) = self.automod_queue.remove(&old_channel) {
                    self.automod_queue.insert(new_channel.clone(), queue);
                }
                if let Some(requests) = self.unban_requests.remove(&old_channel) {
                    self.unban_requests.insert(new_channel.clone(), requests);
                }
                if let Some(reply) = self.pending_reply.as_mut() {
                    if reply.channel == old_channel {
                        reply.channel = new_channel;
                    }
                }
            }
            AppEvent::IrcTopicChanged { channel, topic } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.topic = Some(topic);
                }
            }
            AppEvent::MessageReceived {
                channel,
                mut message,
            } => {
                if channel.is_irc() && !self.state.channels.contains_key(&channel) {
                    // IRC can deliver messages on targets we haven't opened yet
                    // (e.g. direct messages or status-targeted channel forms).
                    // Create the tab first so inbound messages are never dropped.
                    self.state.join_channel(channel.clone());
                    self.sorted_chatters.entry(channel.clone()).or_default();
                }

                // Channel points redemption update events should patch the
                // original redemption row in place instead of adding a second
                // line for the same redemption lifecycle.
                let terminal_redemption_update = match &message.msg_kind {
                    MsgKind::ChannelPointsReward {
                        redemption_id,
                        status,
                        ..
                    } => {
                        let rid = redemption_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let st = status
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let is_terminal = st
                            .map(|s| {
                                s.eq_ignore_ascii_case("fulfilled")
                                    || s.eq_ignore_ascii_case("canceled")
                                    || s.eq_ignore_ascii_case("cancelled")
                            })
                            .unwrap_or(false);
                        rid.zip(st).filter(|_| is_terminal)
                    }
                    _ => None,
                };

                if let Some((rid, st)) = terminal_redemption_update {
                    if let Some(ch) = self.state.channels.get_mut(&channel) {
                        if ch.update_redemption_status(rid, st) {
                            return;
                        }
                    }
                }

                if let Some(server_id) = message.server_id.as_deref() {
                    let duplicate = self
                        .state
                        .channels
                        .get(&channel)
                        .map(|ch| {
                            ch.messages
                                .iter()
                                .any(|m| m.server_id.as_deref() == Some(server_id))
                        })
                        .unwrap_or(false);
                    if duplicate {
                        return;
                    }
                }
                let is_active = self.state.active_channel.as_ref() == Some(&channel);

                let highlight_match = crust_core::highlight::first_match_context(
                    &self.highlight_rules,
                    &message.raw_text,
                    &message.sender.login,
                    &message.sender.display_name,
                    channel.display_name(),
                    message.flags.is_mention,
                );
                if highlight_match.is_some() {
                    message.flags.is_highlighted = true;
                }
                let (highlight_mentions, _highlight_alert, highlight_sound) = highlight_match
                    .map(|(_, show_in_mentions, has_alert, has_sound)| {
                        (show_in_mentions, has_alert, has_sound)
                    })
                    .unwrap_or((false, false, false));

                // Generate a short-lived event toast for high-visibility events.
                // Only pop banners for the channel the user is watching.
                let maybe_toast: Option<EventToast> = if !is_active {
                    None
                } else {
                    match &message.msg_kind {
                        MsgKind::Sub {
                            display_name,
                            months,
                            is_gift,
                            plan,
                            ..
                        } => {
                            let gifted_to_me = *is_gift && message.flags.is_mention;
                            let text = if gifted_to_me {
                                format!("🎉🎊  You received a gifted {} sub!", plan)
                            } else if *is_gift {
                                format!("🎁  {} received a gifted {} sub!", display_name, plan)
                            } else if *months <= 1 {
                                format!("⭐  {} just subscribed with {}!", display_name, plan)
                            } else {
                                format!("⭐  {} resubscribed x{}!", display_name, months)
                            };
                            Some(EventToast {
                                text,
                                hue: if gifted_to_me {
                                    t::raid_cyan()
                                } else {
                                    t::gold()
                                },
                                confetti: gifted_to_me,
                                born: std::time::Instant::now(),
                            })
                        }
                        MsgKind::Raid {
                            display_name,
                            viewer_count,
                        } => Some(EventToast {
                            text: format!(
                                "🚀  {} is raiding with {} viewers!",
                                display_name, viewer_count
                            ),
                            hue: t::raid_cyan(),
                            confetti: false,
                            born: std::time::Instant::now(),
                        }),
                        MsgKind::Bits { amount } if *amount >= 100 => Some(EventToast {
                            text: format!(
                                "💎  {} cheered {} bits!",
                                message.sender.display_name, amount
                            ),
                            hue: t::bits_orange(),
                            confetti: false,
                            born: std::time::Instant::now(),
                        }),
                        _ if message.flags.is_pinned => Some(EventToast {
                            text: format!(
                                "📌  {} sent a pinned message",
                                message.sender.display_name
                            ),
                            hue: t::gold(),
                            confetti: false,
                            born: std::time::Instant::now(),
                        }),
                        _ => None,
                    }
                };
                if let Some(toast) = maybe_toast {
                    self.enqueue_event_toast(toast);
                }

                let mut rebuilt_chatters: Option<Vec<String>> = None;
                let mut request_attention: Option<egui::UserAttentionType> = None;
                let mut desktop_notification: Option<(String, String, bool)> = None;
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    // Track the sender for @username autocomplete.
                    // Only real user messages (Chat, Bits, Sub with text) are
                    // worth tracking; system notices and mod actions are not.
                    match message.msg_kind {
                        MsgKind::Chat | MsgKind::Bits { .. } => {
                            let display_name = message.sender.display_name.trim();
                            if !display_name.is_empty() {
                                let should_insert = ch.chatters.contains(display_name)
                                    || ch.chatters.len() < MAX_TRACKED_CHATTERS;
                                if should_insert && ch.chatters.insert(display_name.to_owned()) {
                                    rebuilt_chatters = Some(sorted_chatters_vec(&ch.chatters));
                                }
                            }
                        }
                        _ => {}
                    }

                    // If this is Twitch's echo of our own sent message, update
                    // the existing local echo in-place instead of adding a
                    // duplicate entry.  absorb_own_echo returns true when an
                    // unconfirmed local echo was found and stamped with the
                    // real server_id; in that case we skip the normal push.
                    let absorbed = message.flags.is_self
                        && message.server_id.is_some()
                        && ch.absorb_own_echo(&message);
                    if !absorbed {
                        // Only count unreads for live messages in background channels.
                        if !is_active && !message.flags.is_history {
                            ch.unread_count += 1;
                            if message.flags.is_mention
                                || message.flags.is_highlighted
                                || message.flags.is_first_msg
                                || message.flags.is_pinned
                                || highlight_mentions
                            {
                                ch.unread_mentions += 1;
                            }
                        }

                        if self.desktop_notifications_enabled
                            && !message.flags.is_history
                            && (message.flags.is_mention || highlight_mentions || highlight_sound)
                        {
                            let sender = if message.sender.display_name.trim().is_empty() {
                                message.sender.login.as_str()
                            } else {
                                message.sender.display_name.trim()
                            };
                            let context = if message.flags.is_mention {
                                "Mention"
                            } else {
                                "Highlight"
                            };
                            let title = Self::truncate_notification_text(
                                &format!("{} - {context}", channel.display_name()),
                                80,
                            );
                            let body = if message.raw_text.trim().is_empty() {
                                format!("{sender} sent a highlighted message")
                            } else {
                                format!("{sender}: {}", message.raw_text.trim())
                            };
                            let body = Self::truncate_notification_text(&body, 220);
                            let notification_sound =
                                highlight_sound || message.flags.is_mention || highlight_mentions;
                            desktop_notification = Some((title, body, notification_sound));

                            request_attention = Some(if highlight_sound {
                                egui::UserAttentionType::Critical
                            } else {
                                egui::UserAttentionType::Informational
                            });
                        }

                        ch.push_message(message);
                    }
                }
                if let Some(attention) = request_attention {
                    self.request_user_attention(ctx, attention);
                }
                if let Some((title, body, with_sound)) = desktop_notification {
                    self.dispatch_desktop_notification(&title, &body, with_sound);
                }
                if let Some(cached) = rebuilt_chatters {
                    self.sorted_chatters.insert(channel, cached);
                }
            }
            AppEvent::MessageDeleted { channel, server_id } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.delete_message(&server_id);
                }
            }
            AppEvent::SystemNotice(_) => {
                // Converted to MessageReceived with MsgKind::SystemInfo in the reducer;
                // the raw event is kept for compatibility but nothing more to do.
            }
            AppEvent::EmoteImageReady {
                uri,
                width,
                height,
                raw_bytes,
            } => {
                // Stub events (empty bytes) are emitted by failed fetches just
                // to advance the loading-screen image counter; skip actual insert.
                if !raw_bytes.is_empty() {
                    let byte_len = raw_bytes.len();
                    self.emote_bytes.entry(uri).or_insert_with(|| {
                        self.emote_ram_bytes += byte_len;
                        (width, height, Arc::from(raw_bytes.as_slice()))
                    });
                }
            }
            AppEvent::EmoteCatalogUpdated { mut emotes } => {
                emotes.sort_by(|a, b| a.code.to_lowercase().cmp(&b.code.to_lowercase()));
                self.emote_catalog = emotes;

                // Re-tokenize existing messages across ALL channels so that
                // emotes that loaded after the messages arrived (e.g. global
                // BTTV/FFZ/7TV emotes like LUL) get resolved.
                let emote_map = build_emote_lookup(&self.emote_catalog);
                if !emote_map.is_empty() {
                    for ch in self.state.channels.values_mut() {
                        for msg in ch.messages.iter_mut() {
                            if !matches!(msg.msg_kind, MsgKind::Chat | MsgKind::Bits { .. }) {
                                continue;
                            }
                            let new_spans = crust_core::format::tokenize(
                                &msg.raw_text,
                                msg.flags.is_action,
                                &msg.twitch_emotes,
                                &|code| {
                                    emote_map.get(code).map(|e| {
                                        (
                                            e.code.clone(),
                                            e.code.clone(),
                                            e.url.clone(),
                                            e.provider.clone(),
                                            None,
                                        )
                                    })
                                },
                            );

                            let old_emote_count = msg
                                .spans
                                .iter()
                                .filter(|s| matches!(s, crust_core::Span::Emote { .. }))
                                .count();
                            let new_emote_count = new_spans
                                .iter()
                                .filter(|s| matches!(s, crust_core::Span::Emote { .. }))
                                .count();

                            if new_emote_count > old_emote_count {
                                for span in &new_spans {
                                    if let crust_core::Span::Emote { url, .. } = span {
                                        if !self.emote_bytes.contains_key(url.as_str()) {
                                            let _ = self.cmd_tx.try_send(AppCommand::FetchImage {
                                                url: url.clone(),
                                            });
                                        }
                                    }
                                }
                                msg.spans = new_spans;
                            }
                        }
                    }
                }
            }
            AppEvent::Authenticated { username, user_id } => {
                self.auth_refresh_inflight = false;
                // Clear the previous account's avatar so it doesn't flash
                // while the new one is fetched.
                self.state.auth.avatar_url = None;
                self.state.auth.logged_in = true;
                self.state.auth.username = Some(username);
                self.state.auth.user_id = Some(user_id);
            }
            AppEvent::LoggedOut => {
                self.auth_refresh_inflight = false;
                self.state.auth.logged_in = false;
                self.state.auth.username = None;
                self.state.auth.user_id = None;
                self.state.auth.avatar_url = None;
            }
            AppEvent::Error { context, message } => {
                tracing::error!("[{context}] {message}");
                // Inject a visible error notice into the active channel so the
                // user doesn't have to watch the terminal to see what went wrong.
                if let Some(ch_id) = self.state.active_channel.clone() {
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel: ch_id,
                        text: format!("[{context}] {message}"),
                    });
                }
            }
            AppEvent::HistoryLoaded { channel, messages } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    // Scroll to the seam between history and live chat so the
                    // user sees context instead of waking up at the bottom.
                    // Only scroll when few live messages have accumulated (fresh
                    // joins / startup), not on mid-session reconnects where the
                    // user is already watching a full backlog.
                    let live_count_before = ch.messages.len();
                    let seam_id = if live_count_before < 100 {
                        ch.messages
                            .front()
                            .and_then(|m| m.server_id.clone())
                            .or_else(|| messages.last().and_then(|m| m.server_id.clone()))
                    } else {
                        None
                    };

                    ch.prepend_history(messages);

                    if let Some(sid) = seam_id {
                        let scroll_key = egui::Id::new("ml_scroll_to").with(channel.as_str());
                        ctx.data_mut(|d| d.insert_temp(scroll_key, sid));
                    }
                }
            }
            AppEvent::UserProfileLoaded { profile } => {
                // Cache stream status.
                let login = profile.login.to_lowercase();
                self.stream_statuses.insert(
                    login.clone(),
                    StreamStatusInfo {
                        is_live: profile.is_live,
                        title: profile.stream_title.clone(),
                        game: profile.stream_game.clone(),
                        viewers: profile.stream_viewers,
                    },
                );
                // Keep the cheap live-map cache in sync.
                self.live_map_cache.insert(login.clone(), profile.is_live);
                self.stream_status_fetch_inflight.remove(&login);
                self.stream_status_fetched
                    .insert(login.clone(), std::time::Instant::now());

                self.handle_stream_status_transition(
                    ctx,
                    &login,
                    profile.is_live,
                    profile.stream_title.clone(),
                    profile.stream_game.clone(),
                    profile.stream_viewers,
                );
                // This event is also used for channel live-status refresh.
                // Only drive the popup when it explicitly requested this login.
                if self.user_profile_popup.accepts_profile(&profile.login) {
                    // Collect this user's recent messages from the channel the
                    // popup was opened for (most-recent first, capped at 200).
                    let ch = self.user_profile_popup.channel.clone();
                    let login_lc = profile.login.to_lowercase();
                    let logs: Vec<_> = ch
                        .as_ref()
                        .and_then(|c| self.state.channels.get(c))
                        .map(|s| {
                            s.messages
                                .iter()
                                .rev()
                                .filter(|m| {
                                    m.sender.login.to_lowercase() == login_lc
                                        && matches!(
                                            m.msg_kind,
                                            crust_core::model::MsgKind::Chat
                                                | crust_core::model::MsgKind::Bits { .. }
                                        )
                                })
                                .take(200)
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    self.user_profile_popup.set_logs(logs);

                    let mod_logs: Vec<_> = ch
                        .as_ref()
                        .and_then(|c| self.state.channels.get(c))
                        .map(|s| {
                            s.messages
                                .iter()
                                .rev()
                                .filter(|m| {
                                    matches!(
                                        &m.msg_kind,
                                        crust_core::model::MsgKind::Timeout { login, .. }
                                            if login.eq_ignore_ascii_case(&profile.login)
                                    ) || matches!(
                                        &m.msg_kind,
                                        crust_core::model::MsgKind::Ban { login }
                                            if login.eq_ignore_ascii_case(&profile.login)
                                    )
                                })
                                .take(120)
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    self.user_profile_popup.set_mod_logs(mod_logs);

                    // Build shared-channel context from all locally-known tabs.
                    let mut shared_channels: Vec<String> = self
                        .state
                        .channels
                        .iter()
                        .filter_map(|(cid, state)| {
                            let seen_in_chatters = state
                                .chatters
                                .iter()
                                .any(|name| name.eq_ignore_ascii_case(&profile.login));
                            let seen_in_messages = state
                                .messages
                                .iter()
                                .rev()
                                .take(400)
                                .any(|m| m.sender.login.eq_ignore_ascii_case(&profile.login));
                            if !(seen_in_chatters || seen_in_messages) {
                                return None;
                            }
                            let label = if cid.is_kick() {
                                format!("kick:{}", cid.display_name())
                            } else if cid.is_irc() {
                                format!("irc:{}", cid.display_name())
                            } else {
                                format!("#{}", cid.display_name())
                            };
                            Some(label)
                        })
                        .collect();
                    shared_channels.sort_by_key(|name| name.to_ascii_lowercase());
                    shared_channels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
                    self.user_profile_popup.set_shared_channels(shared_channels);

                    // If we have a 7TV animated avatar for this user, ensure
                    // its bytes are prefetched so it renders immediately.
                    if let Some(stv_url) = self.stv_avatars.get(&profile.id) {
                        if !self.emote_bytes.contains_key(stv_url.as_str()) {
                            self.send_cmd(AppCommand::FetchImage {
                                url: stv_url.clone(),
                            });
                        }
                    }
                    self.user_profile_popup.set_profile(profile);
                }
            }
            AppEvent::StreamStatusUpdated { login, is_live } => {
                let login = login.to_ascii_lowercase();
                let (title, game, viewers) = {
                    let entry = self
                        .stream_statuses
                        .entry(login.clone())
                        .or_insert(StreamStatusInfo {
                            is_live,
                            title: None,
                            game: None,
                            viewers: None,
                        });
                    entry.is_live = is_live;
                    if !is_live {
                        entry.viewers = None;
                    }
                    (entry.title.clone(), entry.game.clone(), entry.viewers)
                };

                self.live_map_cache.insert(login.clone(), is_live);
                self.stream_status_fetch_inflight.remove(&login);
                self.stream_status_fetched
                    .insert(login.clone(), std::time::Instant::now());

                self.handle_stream_status_transition(
                    ctx,
                    &login,
                    is_live,
                    title,
                    game,
                    viewers,
                );
            }
            AppEvent::UserProfileUnavailable { login } => {
                let login_lc = login.to_ascii_lowercase();
                self.stream_status_fetch_inflight.remove(&login_lc);
                self.stream_status_fetched
                    .insert(login_lc.clone(), std::time::Instant::now());
                if self.user_profile_popup.accepts_profile(&login) {
                    self.user_profile_popup.set_unavailable(&login);
                }
            }
            AppEvent::IvrLogsLoaded { username, messages } => {
                if self.user_profile_popup.accepts_profile(&username) {
                    self.user_profile_popup.set_ivr_logs(messages);
                }
            }
            AppEvent::IvrLogsFailed { username, error } => {
                if self.user_profile_popup.accepts_profile(&username) {
                    self.user_profile_popup.set_ivr_logs_error(error);
                }
            }
            AppEvent::UserMessagesCleared { channel, login } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.delete_messages_from(&login);
                }
            }
            AppEvent::UserStateUpdated {
                channel, is_mod, ..
            } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.is_mod = is_mod;
                }
            }
            AppEvent::ChannelMessagesCleared { channel } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.messages.clear();
                }
            }
            AppEvent::SelfAvatarLoaded { avatar_url } => {
                self.state.auth.avatar_url = Some(avatar_url);
            }
            AppEvent::LinkPreviewReady {
                url,
                title,
                description,
                thumbnail_url,
                site_name,
            } => {
                self.link_previews.insert(
                    url,
                    LinkPreview {
                        title,
                        description,
                        thumbnail_url,
                        site_name,
                        fetched: true,
                    },
                );
            }
            AppEvent::AccountListUpdated {
                accounts,
                active,
                default,
            } => {
                self.state.accounts = accounts.clone();
                self.login_dialog.update_accounts(accounts, active, default);
            }
            AppEvent::BetaFeaturesUpdated {
                kick_enabled,
                irc_enabled,
                irc_nickserv_user,
                irc_nickserv_pass,
                always_on_top,
            } => {
                self.kick_beta_enabled = kick_enabled;
                self.irc_beta_enabled = irc_enabled;
                self.irc_nickserv_user = irc_nickserv_user;
                self.irc_nickserv_pass = irc_nickserv_pass;
                self.always_on_top = always_on_top;
                // Apply the persisted always-on-top preference.
                let level = if always_on_top {
                    egui::viewport::WindowLevel::AlwaysOnTop
                } else {
                    egui::viewport::WindowLevel::Normal
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
            }
            AppEvent::ChatUiBehaviorUpdated {
                prevent_overlong_twitch_messages,
                collapse_long_messages,
                collapse_long_message_lines,
                animations_when_focused,
            } => {
                self.prevent_overlong_twitch_messages = prevent_overlong_twitch_messages;
                self.collapse_long_messages = collapse_long_messages;
                self.collapse_long_message_lines = collapse_long_message_lines.max(1);
                self.animations_when_focused = animations_when_focused;
            }
            AppEvent::GeneralSettingsUpdated {
                show_timestamps,
                show_timestamp_seconds,
                use_24h_timestamps,
                local_log_indexing_enabled,
                auto_join,
                highlights,
                ignores,
                desktop_notifications_enabled,
            } => {
                self.show_timestamps = show_timestamps;
                self.show_timestamp_seconds = show_timestamp_seconds;
                self.use_24h_timestamps = use_24h_timestamps;
                self.local_log_indexing_enabled = local_log_indexing_enabled;
                self.desktop_notifications_enabled = desktop_notifications_enabled;
                self.auto_join_channels = auto_join;
                self.highlights = highlights;
                self.ignores = ignores;
                self.ignores_set = self
                    .ignores
                    .iter()
                    .map(|s| s.to_ascii_lowercase())
                    .collect();
                self.auto_join_buf = self.auto_join_channels.join("\n");
                self.highlights_buf = self.highlights.join("\n");
                self.ignores_buf = self.ignores.join("\n");
            }
            AppEvent::SlashUsageCountsUpdated { usage_counts } => {
                self.slash_usage_counts = usage_counts.into_iter().collect();
            }
            AppEvent::EmotePickerPreferencesUpdated {
                favorites,
                recent,
                provider_boost,
            } => {
                let prefs = EmotePickerPreferences {
                    favorites,
                    recent,
                    provider_boost,
                };
                self.emote_picker.apply_preferences(&prefs);
                self.emote_picker_prefs_last_saved = Some(prefs);
            }
            AppEvent::AppearanceSettingsUpdated {
                channel_layout,
                sidebar_visible,
                analytics_visible,
                irc_status_visible,
                tab_style,
                show_tab_close_buttons,
                show_tab_live_indicators,
                split_header_show_title,
                split_header_show_game,
                split_header_show_viewer_count,
            } => {
                self.channel_layout = ChannelLayout::from_settings(&channel_layout);
                self.sidebar_visible = sidebar_visible;
                self.analytics_visible = analytics_visible;
                self.irc_status_visible = irc_status_visible;
                self.tab_style = TabVisualStyle::from_settings(&tab_style);
                self.show_tab_close_buttons = show_tab_close_buttons;
                self.show_tab_live_indicators = show_tab_live_indicators;
                self.split_header_show_title = split_header_show_title;
                self.split_header_show_game = split_header_show_game;
                self.split_header_show_viewer_count = split_header_show_viewer_count;
            }
            AppEvent::HighlightRulesUpdated { rules } => {
                // Rebuild the compiled rule set used by message rendering.
                self.highlight_rules = crust_core::highlight::compile_rules(&rules);
                // Keep the settings page state in sync so re-opening the
                // dialog shows the current rules without lag.
                self.settings_highlight_rules = rules.clone();
                self.settings_highlight_rule_bufs = rules
                    .iter()
                    .map(|r| r.pattern.clone())
                    .collect();
            }
            AppEvent::FilterRecordsUpdated { records } => {
                self.filter_records = crust_core::model::filters::compile_filters(&records);
                self.settings_filter_records = records.clone();
                self.settings_filter_record_bufs = records
                    .iter()
                    .map(|r| r.pattern.clone())
                    .collect();
            }
            AppEvent::ModActionPresetsUpdated { presets } => {
                self.mod_action_presets = presets.clone();
                self.settings_mod_action_presets = presets;
            }
            AppEvent::AuthExpired => {
                warn!("Auth expired — checking refresh path");
                let now = std::time::Instant::now();
                let can_retry = self
                    .last_auth_refresh_attempt
                    .map(|last| now.duration_since(last) >= AUTH_REFRESH_RETRY_INTERVAL)
                    .unwrap_or(true);

                if !self.auth_refresh_inflight && can_retry {
                    self.auth_refresh_inflight = true;
                    self.last_auth_refresh_attempt = Some(now);
                    self.send_cmd(AppCommand::RefreshAuth);
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel: self
                            .state
                            .active_channel
                            .clone()
                            .unwrap_or_else(|| ChannelId::new("system")),
                        text: "\u{26a0}\u{fe0f} Twitch auth check failed. Trying token refresh...".into(),
                    });
                } else {
                    self.auth_refresh_inflight = false;
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel: self
                            .state
                            .active_channel
                            .clone()
                            .unwrap_or_else(|| ChannelId::new("system")),
                        text: "\u{26a0}\u{fe0f} Your Twitch token has expired. Please re-authenticate in Settings \u{2192} Account.".into(),
                    });
                }
            }
            AppEvent::ChannelEmotesLoaded { .. } => {
                // Re-tokenization is now handled by EmoteCatalogUpdated which
                // fires for every emote load (global, channel, personal 7TV).
            }
            AppEvent::ImagePrefetchQueued { .. } => {
                // Handled in the loading-screen pre-pass above; nothing else to do.
            }
            AppEvent::RoomStateUpdated {
                channel,
                emote_only,
                followers_only,
                slow,
                subs_only,
                r9k,
            } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    if let Some(v) = emote_only {
                        ch.room_state.emote_only = v;
                    }
                    if let Some(v) = followers_only {
                        ch.room_state.followers_only = Some(v);
                    }
                    if let Some(v) = slow {
                        ch.room_state.slow_mode = Some(v);
                    }
                    if let Some(v) = subs_only {
                        ch.room_state.subscribers_only = v;
                    }
                    if let Some(v) = r9k {
                        ch.room_state.r9k = v;
                    }
                }
            }
            AppEvent::AutoModQueueAppend { channel, item } => {
                let queue = self.automod_queue.entry(channel).or_default();
                if let Some(existing) = queue
                    .iter_mut()
                    .find(|existing| existing.message_id == item.message_id)
                {
                    *existing = item;
                } else {
                    queue.push(item);
                }
            }
            AppEvent::AutoModQueueRemove {
                channel,
                message_id,
                action,
            } => {
                if let Some(queue) = self.automod_queue.get_mut(&channel) {
                    queue.retain(|item| item.message_id != message_id);
                }
                if let Some(action) = action {
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel,
                        text: format!("[AutoMod] Message {message_id} resolved: {action}"),
                    });
                }
            }
            AppEvent::UnbanRequestsLoaded { channel, requests } => {
                let mut requests = requests;
                Self::normalize_unban_requests(&mut requests);
                let keep_ids: HashSet<String> =
                    requests.iter().map(|item| item.request_id.clone()).collect();
                let prefix = format!("{}::", channel.as_str());
                self.unban_resolution_drafts.retain(|k, _| {
                    if !k.starts_with(&prefix) {
                        return true;
                    }
                    let request_id = &k[prefix.len()..];
                    keep_ids.contains(request_id)
                });
                self.unban_requests.insert(channel, requests);
            }
            AppEvent::UnbanRequestsFailed { channel, error } => {
                self.send_cmd(AppCommand::InjectLocalMessage {
                    channel,
                    text: format!("[Unban Requests] {error}"),
                });
            }
            AppEvent::UnbanRequestUpsert { channel, request } => {
                let requests = self.unban_requests.entry(channel).or_default();
                if let Some(existing) = requests
                    .iter_mut()
                    .find(|existing| existing.request_id == request.request_id)
                {
                    *existing = request;
                } else {
                    requests.push(request);
                }
                Self::normalize_unban_requests(requests);
            }
            AppEvent::UnbanRequestResolved {
                channel,
                request_id,
                status: _,
            } => {
                if let Some(requests) = self.unban_requests.get_mut(&channel) {
                    requests.retain(|request| request.request_id != request_id);
                }
                self.unban_resolution_drafts
                    .remove(&Self::unban_draft_key(&channel, &request_id));
            }
            AppEvent::OpenModerationTools { channel } => {
                if let Some(channel) = channel {
                    if self.state.channels.contains_key(&channel) {
                        self.activate_channel(channel.clone());
                    }
                    if channel.is_twitch() {
                        self.send_cmd(AppCommand::FetchUnbanRequests { channel });
                    }
                }
                self.mod_tools_open = true;
            }
            AppEvent::SenderCosmeticsUpdated {
                user_id,
                color,
                name_paint: _name_paint,
                badge,
                avatar_url,
            } => {
                let normalize_external_url = |url: &str| -> Option<String> {
                    let trimmed = url.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    if trimmed.starts_with("//") {
                        return Some(format!("https:{trimmed}"));
                    }
                    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                        return Some(trimmed.to_owned());
                    }
                    None
                };

                if user_id.is_empty() {
                    return;
                }

                // Store 7TV animated avatar URL for this user.
                if let Some(ref url) = avatar_url {
                    let normalized = normalize_external_url(url).unwrap_or_else(|| url.clone());
                    self.stv_avatars.insert(user_id.clone(), normalized.clone());
                    // Prefetch the avatar bytes so they're ready for the popup.
                    if !self.emote_bytes.contains_key(normalized.as_str()) {
                        self.send_cmd(AppCommand::FetchImage { url: normalized });
                    }
                }

                let mut _updated = 0u32;
                for ch in self.state.channels.values_mut() {
                    for msg in &mut ch.messages {
                        if msg.sender.user_id.0 != user_id {
                            continue;
                        }

                        if let Some(ref c) = color {
                            msg.sender.color = Some(c.clone());
                        }
                        msg.sender.name_paint = None;

                        if let Some(ref b) = badge {
                            if let Some(existing) = msg
                                .sender
                                .badges
                                .iter_mut()
                                .find(|x| x.name.eq_ignore_ascii_case("7tv"))
                            {
                                *existing = b.clone();
                            } else {
                                msg.sender.badges.insert(0, b.clone());
                            }
                        }
                        _updated += 1;
                    }
                }
            }
        }
    }

    fn send_cmd(&self, cmd: AppCommand) {
        if self.cmd_tx.try_send(cmd).is_err() {
            warn!("Command channel full/closed");
        }
    }

    fn active_search_target(&self) -> Option<ChannelId> {
        if self.split_panes.panes.len() > 1 {
            self.split_panes
                .focused_channel()
                .cloned()
                .or_else(|| self.state.active_channel.clone())
        } else {
            self.state.active_channel.clone()
        }
    }

    fn message_search_mut(&mut self, channel: &ChannelId) -> &mut MessageSearchState {
        self.message_search.entry(channel.clone()).or_default()
    }

    fn request_older_local_history(&self, channel: &ChannelId, oldest_loaded_ts_ms: i64) {
        self.send_cmd(AppCommand::LoadOlderLocalHistory {
            channel: channel.clone(),
            before_ts_ms: oldest_loaded_ts_ms,
            limit: LOCAL_HISTORY_SEARCH_PAGE,
        });
    }

    fn static_avatar_texture_for(
        &mut self,
        ui: &egui::Ui,
        url: &str,
        raw: &Arc<[u8]>,
    ) -> Option<egui::TextureHandle> {
        let is_animated = is_likely_animated_image_url(url) || is_likely_animated_image_bytes(raw);
        if !is_animated {
            return None;
        }

        if let Some(tex) = self.static_avatar_frames.get(url) {
            return Some(tex.clone());
        }

        let img = decode_static_image_frame(raw)?;
        let tex = ui.ctx().load_texture(
            format!("app-avatar-static://{url}"),
            img,
            egui::TextureOptions::LINEAR,
        );
        self.static_avatar_frames
            .insert(url.to_owned(), tex.clone());
        Some(tex)
    }

    fn show_topbar_account_button(
        &mut self,
        ui: &mut egui::Ui,
        compact_account: bool,
        ultra_compact: bool,
    ) {
        if self.state.auth.logged_in {
            let name = self
                .state
                .auth
                .username
                .as_deref()
                .unwrap_or("User")
                .to_owned();
            let display_name = truncate_with_ellipsis(&name, if compact_account { 14 } else { 20 });
            let initial = name
                .chars()
                .next()
                .unwrap_or('?')
                .to_uppercase()
                .next()
                .unwrap_or('?');

            if compact_account {
                if chrome::icon_button(
                    ui,
                    ChromeIcon::Account,
                    "Account",
                    IconButtonState {
                        compact: ultra_compact,
                        ..Default::default()
                    },
                )
                .clicked()
                {
                    self.login_dialog.toggle();
                }
            } else {
                let btn_h = t::BAR_H;
                let name_galley =
                    ui.painter()
                        .layout_no_wrap(display_name.clone(), t::small(), t::text_primary());
                let pill_w = (btn_h + 6.0 + name_galley.size().x + 10.0).clamp(btn_h + 28.0, 230.0);
                let (rect, resp) = ui
                    .allocate_exact_size(egui::vec2(pill_w, btn_h), egui::Sense::click());
                resp.clone().on_hover_text("Account");

                if ui.is_rect_visible(rect) {
                    let bg = if resp.hovered() {
                        t::bg_raised()
                    } else {
                        t::bg_surface()
                    };
                    let border = if resp.hovered() {
                        t::border_accent()
                    } else {
                        t::border_subtle()
                    };
                    ui.painter().rect(
                        rect,
                        t::RADIUS,
                        bg,
                        egui::Stroke::new(1.0, border),
                        egui::StrokeKind::Middle,
                    );

                    // Avatar circle
                    let avatar_r = btn_h * 0.34;
                    let avatar_c = egui::pos2(rect.left() + btn_h * 0.5, rect.center().y);

                    // Try to render the real avatar image; fall back to initial letter.
                    // Prefer 7TV animated avatar if available.
                    let avatar_bytes = self
                        .state
                        .auth
                        .user_id
                        .as_deref()
                        .and_then(|uid| self.stv_avatars.get(uid))
                        .and_then(|url| {
                            self.emote_bytes
                                .get(url.as_str())
                                .map(|(_, _, raw)| (url.clone(), raw.clone()))
                        })
                        .or_else(|| {
                            self.state.auth.avatar_url.as_deref().and_then(|url| {
                                self.emote_bytes
                                    .get(url)
                                    .map(|(_, _, raw)| (url.to_owned(), raw.clone()))
                            })
                        });

                    if let Some((logo, raw)) = avatar_bytes {
                        let av_size = avatar_r * 2.0;
                        let av_rect =
                            egui::Rect::from_center_size(avatar_c, egui::vec2(av_size, av_size));
                        ui.painter().circle_filled(avatar_c, avatar_r, t::bg_raised());
                        if let Some(tex) = self.static_avatar_texture_for(ui, &logo, &raw) {
                            ui.put(
                                av_rect,
                                egui::Image::new((tex.id(), egui::vec2(av_size, av_size)))
                                    .corner_radius(egui::CornerRadius::same(avatar_r as u8)),
                            );
                        } else {
                            let uri = bytes_uri(&logo, raw.as_ref());
                            ui.put(
                                av_rect,
                                egui::Image::from_bytes(uri, egui::load::Bytes::Shared(raw))
                                    .fit_to_exact_size(egui::vec2(av_size, av_size))
                                    .corner_radius(egui::CornerRadius::same(avatar_r as u8)),
                            );
                        }
                    } else {
                        ui.painter().circle_filled(avatar_c, avatar_r, t::accent_dim());
                        ui.painter().text(
                            avatar_c,
                            egui::Align2::CENTER_CENTER,
                            initial.to_string(),
                            egui::FontId::proportional(avatar_r * 1.15),
                            t::text_primary(),
                        );
                    }

                    // Username
                    ui.painter().text(
                        egui::pos2(avatar_c.x + btn_h * 0.5 + 4.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        &display_name,
                        t::small(),
                        t::text_primary(),
                    );
                }

                if resp.clicked() {
                    self.login_dialog.toggle();
                }
            }
        } else {
            let (login_label, login_w) = if compact_account {
                ("", t::BAR_H)
            } else if self.state.accounts.is_empty() {
                ("Log in", 68.0)
            } else {
                ("Accounts", 68.0)
            };
            if compact_account {
                if chrome::icon_button(
                    ui,
                    ChromeIcon::Account,
                    "Log in with a Twitch OAuth token",
                    IconButtonState {
                        compact: ultra_compact,
                        ..Default::default()
                    },
                )
                .clicked()
                {
                    self.login_dialog.toggle();
                }
            } else if ui
                .add_sized(
                    [login_w, t::BAR_H],
                    egui::Button::new(RichText::new(login_label).font(t::small())),
                )
                .on_hover_text("Log in with a Twitch OAuth token")
                .clicked()
            {
                self.login_dialog.toggle();
            }
        }
    }

    fn handle_search_shortcuts(&mut self, ctx: &Context) {
        let open_search = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F));
        let close_search =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let Some(channel) = self.active_search_target() else {
            return;
        };

        if open_search {
            self.message_search_mut(&channel).request_open();
        }
        if close_search {
            let search = self.message_search_mut(&channel);
            if search.open {
                search.close();
            }
        }
    }

    fn record_slash_usage_from_text(&mut self, text: &str) {
        let trimmed = text.trim_start();
        if !trimmed.starts_with('/') {
            return;
        }
        let without_slash = &trimmed[1..];
        let Some(token) = without_slash.split_whitespace().next() else {
            return;
        };
        let cmd = token.trim().to_ascii_lowercase();
        if cmd.is_empty() {
            return;
        }
        let entry = self.slash_usage_counts.entry(cmd).or_insert(0);
        *entry = entry.saturating_add(1);
        self.send_cmd(AppCommand::SetSlashUsageCounts {
            usage_counts: self
                .slash_usage_counts
                .iter()
                .map(|(name, count)| (name.clone(), *count))
                .collect(),
        });
    }

    fn activate_channel(&mut self, channel: ChannelId) {
        if let Some(state) = self.state.channels.get_mut(&channel) {
            state.mark_read();
        }
        if !self.split_panes.panes.is_empty() {
            let focused = self.split_panes.focused;
            if let Some(pane) = self.split_panes.panes.get_mut(focused) {
                pane.channel = channel.clone();
                pane.input_buf.clear();
            }
        }
        self.state.active_channel = Some(channel);
    }

    fn next_channel_target(&self, reverse: bool) -> Option<ChannelId> {
        let len = self.state.channel_order.len();
        if len == 0 {
            return None;
        }

        let current_idx = self
            .state
            .active_channel
            .as_ref()
            .and_then(|active| self.state.channel_order.iter().position(|ch| ch == active))
            .unwrap_or(0);

        let find_by = |predicate: &dyn Fn(&ChannelId) -> bool| -> Option<ChannelId> {
            for step in 1..=len {
                let idx = if reverse {
                    (current_idx + len - (step % len)) % len
                } else {
                    (current_idx + step) % len
                };
                let ch = &self.state.channel_order[idx];
                if predicate(ch) {
                    return Some(ch.clone());
                }
            }
            None
        };

        find_by(&|ch| {
            self.state
                .channels
                .get(ch)
                .map(|s| s.unread_mentions > 0)
                .unwrap_or(false)
        })
        .or_else(|| {
            find_by(&|ch| {
                self.state
                    .channels
                    .get(ch)
                    .map(|s| s.unread_count > 0)
                    .unwrap_or(false)
            })
        })
        .or_else(|| {
            let idx = if reverse {
                (current_idx + len - 1) % len
            } else {
                (current_idx + 1) % len
            };
            self.state.channel_order.get(idx).cloned()
        })
    }

    fn handle_channel_shortcuts(&mut self, ctx: &Context) {
        let (next, prev, direct_idx) = ctx.input_mut(|i| {
            let ctrl_shift = egui::Modifiers {
                ctrl: true,
                shift: true,
                ..Default::default()
            };
            let next = i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::PageDown)
                || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowRight);
            let prev = i.consume_key(ctrl_shift, egui::Key::Tab)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::PageUp)
                || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowLeft);
            let direct_idx = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
                egui::Key::Num9,
            ]
            .iter()
            .position(|key| i.consume_key(egui::Modifiers::CTRL, *key));
            (next, prev, direct_idx)
        });

        if let Some(idx) = direct_idx {
            if let Some(target) = self.state.channel_order.get(idx).cloned() {
                self.activate_channel(target);
            }
            return;
        }

        let reverse = if next {
            Some(false)
        } else if prev {
            Some(true)
        } else {
            None
        };

        let Some(reverse) = reverse else {
            return;
        };

        if let Some(target) = self.next_channel_target(reverse) {
            self.activate_channel(target);
        }
    }
}

// eframe::App implementation

impl eframe::App for CrustApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.auth_refresh_inflight
            && self
                .last_auth_refresh_attempt
                .map(|t| t.elapsed() >= AUTH_REFRESH_INFLIGHT_TIMEOUT)
                .unwrap_or(false)
        {
            self.auth_refresh_inflight = false;
        }

        self.handle_channel_shortcuts(ctx);
        self.handle_search_shortcuts(ctx);

        let events = self.drain_events(ctx);
        let had_events = events > 0;

        // Repaint policy:
        // - Event-driven wakeups from the runtime call `ctx.request_repaint()`
        //   as soon as new events arrive.
        // - Keep fast ticking only while UI animations are active.
        // - Keep a slow housekeeping poll for periodic maintenance paths.
        if had_events {
            ctx.request_repaint(); // drain the next batch ASAP
        }
        // Keep a fast repaint cadence only while an on-screen animation is active.
        let has_animated_popup = self.user_profile_popup.open
            && self
                .user_profile_popup
                .profile_id()
                .and_then(|id| self.stv_avatars.get(id))
                .and_then(|url| self.emote_bytes.get(url.as_str()))
                .is_some();
        let window_focused = ctx.input(|i| i.focused);
        let animations_allowed = !self.animations_when_focused || window_focused;
        let has_active_animation =
            animations_allowed && (!self.event_toasts.is_empty() || has_animated_popup);
        let repaint_ms = if has_active_animation {
            REPAINT_ANIM_MS
        } else {
            REPAINT_HOUSEKEEPING_MS
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint_ms));

        // Loading overlay: shown until connection + emotes + history are ready.
        if self.loading_screen.is_active() {
            self.loading_screen.show(ctx);
            return;
        }
        let appearance_before = self.appearance_snapshot();

        self.perf.emote_count = self.emote_bytes.len();
        self.perf.emote_ram_kb = self.emote_ram_bytes / 1024;
        self.perf.record_frame(events, had_events);
        let mut frame_chat_stats = ChatPerfStats::default();

        // Only recompute analytics while the panel is visible.
        if self.analytics_visible {
            if let Some(ref ch) = self.state.active_channel {
                if let Some(ch_state) = self.state.channels.get(ch) {
                    self.analytics_panel.tick(ch_state);
                }
            }
        }

        // Periodic stream-status refresh: re-fetch the active Twitch channel
        // at a higher cadence and all joined channels on a slower interval.
        // Throttle the stale-scan itself to avoid per-frame channel iteration.
        if self.last_active_stream_refresh.elapsed() >= STREAM_REFRESH_ACTIVE_INTERVAL {
            self.last_active_stream_refresh = std::time::Instant::now();
            let active_login = self
                .state
                .active_channel
                .as_ref()
                .filter(|ch| ch.is_twitch())
                .map(|ch| ch.display_name().to_ascii_lowercase());
            if let Some(login) = active_login {
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= STREAM_REFRESH_ACTIVE_INTERVAL)
                    .unwrap_or(true);
                if is_stale {
                    self.request_stream_status_refresh(&login);
                }
            }
        }

        if self.last_stream_refresh_scan.elapsed() >= STREAM_REFRESH_SCAN_INTERVAL {
            self.last_stream_refresh_scan = std::time::Instant::now();
            let mut stale: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for ch in &self.state.channel_order {
                if !ch.is_twitch() {
                    continue;
                }
                let login = ch.display_name().to_ascii_lowercase();
                if !is_valid_twitch_login(&login) {
                    continue;
                }
                if !seen.insert(login.clone()) {
                    continue;
                }
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= STREAM_REFRESH_BACKGROUND_INTERVAL)
                    .unwrap_or(true);
                if is_stale {
                    stale.push(login);
                }
            }

            for watched in self
                .stream_tracker
                .get_watched_channels(crust_core::notifications::Platform::Twitch)
            {
                let login = watched.channel_name.to_ascii_lowercase();
                if !is_valid_twitch_login(&login) {
                    continue;
                }
                if !seen.insert(login.clone()) {
                    continue;
                }
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= STREAM_REFRESH_BACKGROUND_INTERVAL)
                    .unwrap_or(true);
                if is_stale {
                    stale.push(login);
                }
            }
            for login in stale {
                self.request_stream_status_refresh(&login);
            }
        }

        // Render profile popup and dispatch any actions.
        for action in self
            .user_profile_popup
            .show(ctx, &self.emote_bytes, &self.stv_avatars, &self.mod_action_presets)
        {
            match action {
                PopupAction::Timeout {
                    channel,
                    login,
                    user_id,
                    seconds,
                    reason,
                } => {
                    self.send_cmd(AppCommand::TimeoutUser {
                        channel,
                        login,
                        user_id,
                        seconds,
                        reason,
                    });
                }
                PopupAction::Ban {
                    channel,
                    login,
                    user_id,
                    reason,
                } => {
                    self.send_cmd(AppCommand::BanUser {
                        channel,
                        login,
                        user_id,
                        reason,
                    });
                }
                PopupAction::Unban {
                    channel,
                    login,
                    user_id,
                } => {
                    self.send_cmd(AppCommand::UnbanUser {
                        channel,
                        login,
                        user_id,
                    });
                }
                PopupAction::FetchIvrLogs { channel, username } => {
                    self.user_profile_popup.set_ivr_logs_loading();
                    self.send_cmd(AppCommand::FetchIvrLogs { channel, username });
                }
                PopupAction::OpenUrl { url } => {
                    self.send_cmd(AppCommand::OpenUrl { url });
                }
                PopupAction::OpenModerationTools { channel } => {
                    self.send_cmd(AppCommand::OpenModerationTools {
                        channel: Some(channel),
                    });
                }
                PopupAction::ExecuteCommand { channel, command } => {
                    self.send_cmd(AppCommand::SendMessage {
                        channel,
                        text: command,
                        reply_to_msg_id: None,
                        reply: None,
                    });
                }
            }
        }

        // -- Dialogs -----------------------------------------------------------
        if let Some(ch) = self
            .join_dialog
            .show(ctx, self.kick_beta_enabled, self.irc_beta_enabled)
        {
            self.send_cmd(AppCommand::JoinChannel { channel: ch });
        }
        // For the login dialog, prefer 7TV animated avatar if available.
        let login_avatar_url: Option<&str> = self
            .state
            .auth
            .user_id
            .as_deref()
            .and_then(|uid| self.stv_avatars.get(uid))
            .map(|s| s.as_str())
            .or(self.state.auth.avatar_url.as_deref());
        if let Some(action) = self.login_dialog.show(
            ctx,
            self.state.auth.logged_in,
            self.state.auth.username.as_deref(),
            login_avatar_url,
            &self.emote_bytes,
        ) {
            match action {
                LoginAction::Login(token) => self.send_cmd(AppCommand::Login { token }),
                LoginAction::Logout => self.send_cmd(AppCommand::Logout),
                LoginAction::SwitchAccount(username) => {
                    self.send_cmd(AppCommand::SwitchAccount { username });
                }
                LoginAction::RemoveAccount(username) => {
                    self.send_cmd(AppCommand::RemoveAccount { username });
                }
                LoginAction::SetDefaultAccount(username) => {
                    self.send_cmd(AppCommand::SetDefaultAccount { username });
                }
            }
        }

        if self.settings_open {
            let mut settings_open = self.settings_open;
            let mut settings_section = self.settings_section;
            let mut state = SettingsPageState {
                kick_beta_enabled: self.kick_beta_enabled,
                irc_beta_enabled: self.irc_beta_enabled,
                irc_nickserv_user: self.irc_nickserv_user.clone(),
                irc_nickserv_pass: self.irc_nickserv_pass.clone(),
                always_on_top: self.always_on_top,
                prevent_overlong_twitch_messages: self.prevent_overlong_twitch_messages,
                collapse_long_messages: self.collapse_long_messages,
                collapse_long_message_lines: self.collapse_long_message_lines,
                animations_when_focused: self.animations_when_focused,
                show_timestamps: self.show_timestamps,
                show_timestamp_seconds: self.show_timestamp_seconds,
                use_24h_timestamps: self.use_24h_timestamps,
                local_log_indexing_enabled: self.local_log_indexing_enabled,
                highlights_buf: self.highlights_buf.clone(),
                ignores_buf: self.ignores_buf.clone(),
                auto_join_buf: self.auto_join_buf.clone(),
                light_theme: t::is_light(),
                channel_layout: self.channel_layout,
                sidebar_visible: self.sidebar_visible,
                analytics_visible: self.analytics_visible,
                irc_status_visible: self.irc_status_visible,
                tab_style: self.tab_style,
                show_tab_close_buttons: self.show_tab_close_buttons,
                show_tab_live_indicators: self.show_tab_live_indicators,
                split_header_show_title: self.split_header_show_title,
                split_header_show_game: self.split_header_show_game,
                split_header_show_viewer_count: self.split_header_show_viewer_count,
                desktop_notifications_enabled: self.desktop_notifications_enabled,
                highlight_rules: self.settings_highlight_rules.clone(),
                highlight_rule_bufs: self.settings_highlight_rule_bufs.clone(),
                filter_records: self.settings_filter_records.clone(),
                filter_record_bufs: self.settings_filter_record_bufs.clone(),
                mod_action_presets: self.settings_mod_action_presets.clone(),
            };
            let appearance_before = self.appearance_snapshot();
            let stats = SettingsStats {
                highlights_count: self.settings_highlight_rules.len(),
                ignores_count: self.ignores.len(),
                auto_join_count: self.auto_join_channels.len(),
            };

            show_settings_page(
                ctx,
                &mut settings_open,
                &mut settings_section,
                &mut state,
                stats,
            );

            self.settings_open = settings_open;
            self.settings_section = settings_section;
            if state.light_theme != t::is_light() {
                if state.light_theme {
                    t::set_light();
                } else {
                    t::set_dark();
                }
                apply_theme_visuals(ctx);
                let theme = if state.light_theme { "light" } else { "dark" };
                self.send_cmd(AppCommand::SetTheme {
                    theme: theme.to_owned(),
                });
            }
            if state.always_on_top != self.always_on_top {
                self.always_on_top = state.always_on_top;
                let level = if state.always_on_top {
                    egui::viewport::WindowLevel::AlwaysOnTop
                } else {
                    egui::viewport::WindowLevel::Normal
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                self.send_cmd(AppCommand::SetAlwaysOnTop {
                    enabled: state.always_on_top,
                });
            }
            if state.kick_beta_enabled != self.kick_beta_enabled
                || state.irc_beta_enabled != self.irc_beta_enabled
            {
                self.kick_beta_enabled = state.kick_beta_enabled;
                self.irc_beta_enabled = state.irc_beta_enabled;
                if !self.irc_beta_enabled {
                    self.irc_status_visible = false;
                    state.irc_status_visible = false;
                }
                self.send_cmd(AppCommand::SetBetaFeatures {
                    kick_enabled: state.kick_beta_enabled,
                    irc_enabled: state.irc_beta_enabled,
                });
            }
            if state.irc_nickserv_user != self.irc_nickserv_user
                || state.irc_nickserv_pass != self.irc_nickserv_pass
            {
                self.irc_nickserv_user = state.irc_nickserv_user.clone();
                self.irc_nickserv_pass = state.irc_nickserv_pass.clone();
                self.send_cmd(AppCommand::SetIrcAuth {
                    nickserv_user: state.irc_nickserv_user,
                    nickserv_pass: state.irc_nickserv_pass,
                });
            }
            if state.prevent_overlong_twitch_messages != self.prevent_overlong_twitch_messages
                || state.collapse_long_messages != self.collapse_long_messages
                || state.collapse_long_message_lines != self.collapse_long_message_lines
                || state.animations_when_focused != self.animations_when_focused
            {
                self.prevent_overlong_twitch_messages = state.prevent_overlong_twitch_messages;
                self.collapse_long_messages = state.collapse_long_messages;
                self.collapse_long_message_lines = state.collapse_long_message_lines.max(1);
                self.animations_when_focused = state.animations_when_focused;
                self.send_cmd(AppCommand::SetChatUiBehavior {
                    prevent_overlong_twitch_messages: self.prevent_overlong_twitch_messages,
                    collapse_long_messages: self.collapse_long_messages,
                    collapse_long_message_lines: self.collapse_long_message_lines,
                    animations_when_focused: self.animations_when_focused,
                });
            }
            self.channel_layout = state.channel_layout;
            self.sidebar_visible = state.sidebar_visible;
            self.analytics_visible = state.analytics_visible;
            self.irc_status_visible = if self.irc_beta_enabled {
                state.irc_status_visible
            } else {
                false
            };
            self.tab_style = state.tab_style;
            self.show_tab_close_buttons = state.show_tab_close_buttons;
            self.show_tab_live_indicators = state.show_tab_live_indicators;
            self.split_header_show_title = state.split_header_show_title;
            self.split_header_show_game = state.split_header_show_game;
            self.split_header_show_viewer_count = state.split_header_show_viewer_count;
            if self.appearance_snapshot() != appearance_before {
                self.send_appearance_settings();
            }
            if state.highlight_rules != self.settings_highlight_rules {
                self.send_cmd(crust_core::events::AppCommand::SetHighlightRules {
                    rules: state.highlight_rules.clone(),
                });
            }
            if state.filter_records != self.settings_filter_records {
                self.send_cmd(crust_core::events::AppCommand::SetFilterRecords {
                    records: state.filter_records.clone(),
                });
            }
            if state.mod_action_presets != self.settings_mod_action_presets {
                self.send_cmd(crust_core::events::AppCommand::SetModActionPresets {
                    presets: state.mod_action_presets.clone(),
                });
            }
            if state.desktop_notifications_enabled != self.desktop_notifications_enabled {
                self.desktop_notifications_enabled = state.desktop_notifications_enabled;
                self.send_cmd(AppCommand::SetNotificationSettings {
                    desktop_notifications_enabled: self.desktop_notifications_enabled,
                });
            }
            if state.show_timestamps != self.show_timestamps
                || state.show_timestamp_seconds != self.show_timestamp_seconds
                || state.use_24h_timestamps != self.use_24h_timestamps
                || state.local_log_indexing_enabled != self.local_log_indexing_enabled
                || state.highlights_buf != self.highlights_buf
                || state.ignores_buf != self.ignores_buf
                || state.auto_join_buf != self.auto_join_buf
            {
                let highlights = parse_settings_lines(&state.highlights_buf, false);
                let ignores = parse_settings_lines(&state.ignores_buf, true);
                let auto_join = parse_settings_lines(&state.auto_join_buf, false);

                self.show_timestamps = state.show_timestamps;
                self.show_timestamp_seconds = state.show_timestamp_seconds;
                self.use_24h_timestamps = state.use_24h_timestamps;
                self.local_log_indexing_enabled = state.local_log_indexing_enabled;
                self.highlights = highlights.clone();
                self.ignores = ignores.clone();
                self.ignores_set = ignores.iter().cloned().collect();
                self.auto_join_channels = auto_join.clone();
                self.highlights_buf = state.highlights_buf;
                self.ignores_buf = state.ignores_buf;
                self.auto_join_buf = state.auto_join_buf;

                self.send_cmd(AppCommand::SetGeneralSettings {
                    show_timestamps: self.show_timestamps,
                    show_timestamp_seconds: self.show_timestamp_seconds,
                    use_24h_timestamps: self.use_24h_timestamps,
                    local_log_indexing_enabled: self.local_log_indexing_enabled,
                    auto_join,
                    highlights,
                    ignores,
                });
            }
        }

        let moderation_channel = self.active_moderation_channel();

        if self.mod_tools_open {
            let mut window_open = self.mod_tools_open;
            let mut refresh_channel: Option<ChannelId> = None;
            let mut automod_actions: Vec<(String, String, String)> = Vec::new();
            let mut unban_actions: Vec<(String, bool, Option<String>)> = Vec::new();

            egui::Window::new("Moderation Tools")
                .open(&mut window_open)
                .default_size(egui::vec2(540.0, 520.0))
                .show(ctx, |ui| {
                    let Some(channel) = moderation_channel.clone() else {
                        ui.label(
                            RichText::new(
                                "Open a Twitch channel where you are a moderator to use moderation tools.",
                            )
                            .color(t::text_muted())
                            .font(t::small()),
                        );
                        return;
                    };

                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Channel: #{}", channel.display_name()))
                                .font(t::small())
                                .strong(),
                        );
                        if ui
                            .button(RichText::new("Refresh unban requests").font(t::small()))
                            .clicked()
                        {
                            refresh_channel = Some(channel.clone());
                        }
                    });

                    ui.separator();
                    ui.label(RichText::new("AutoMod Queue").font(t::small()).strong());

                    let queue_items = self
                        .automod_queue
                        .get(&channel)
                        .cloned()
                        .unwrap_or_default();
                    if queue_items.is_empty() {
                        ui.label(
                            RichText::new("No held AutoMod messages.")
                                .color(t::text_muted())
                                .font(t::small()),
                        );
                    } else {
                        egui::ScrollArea::vertical().max_height(210.0).show(ui, |ui| {
                            for item in queue_items {
                                chrome::card_frame().show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(format!("@{}", item.sender_login))
                                                .font(t::small())
                                                .strong(),
                                        );
                                        ui.separator();
                                        ui.label(
                                            RichText::new(format!("id: {}", item.message_id))
                                                .font(t::tiny())
                                                .color(t::text_muted()),
                                        );
                                    });

                                    if let Some(reason) =
                                        item.reason.as_deref().filter(|s| !s.trim().is_empty())
                                    {
                                        ui.label(
                                            RichText::new(format!(" AutoMod: {reason} "))
                                                .font(t::small())
                                                .color(Color32::WHITE)
                                                .background_color(t::red()),
                                        );
                                    }

                                    ui.label(RichText::new(item.text.clone()).font(t::small()));

                                    ui.horizontal(|ui| {
                                        if ui
                                            .button(
                                                RichText::new("Allow")
                                                    .font(t::small())
                                                    .color(t::green()),
                                            )
                                            .clicked()
                                        {
                                            automod_actions.push((
                                                item.message_id.clone(),
                                                item.sender_user_id.clone(),
                                                "ALLOW".to_owned(),
                                            ));
                                        }
                                        if ui
                                            .button(
                                                RichText::new("Deny")
                                                    .font(t::small())
                                                    .color(t::red()),
                                            )
                                            .clicked()
                                        {
                                            automod_actions.push((
                                                item.message_id.clone(),
                                                item.sender_user_id.clone(),
                                                "DENY".to_owned(),
                                            ));
                                        }
                                    });
                                });
                                ui.add_space(6.0);
                            }
                        });
                    }

                    ui.separator();
                    ui.label(
                        RichText::new("Pending Unban Requests")
                            .font(t::small())
                            .strong(),
                    );

                    let requests = self
                        .unban_requests
                        .get(&channel)
                        .cloned()
                        .unwrap_or_default();
                    if requests.is_empty() {
                        ui.label(
                            RichText::new("No pending unban requests loaded.")
                                .color(t::text_muted())
                                .font(t::small()),
                        );
                    } else {
                        egui::ScrollArea::vertical().max_height(230.0).show(ui, |ui| {
                            for request in requests {
                                let key = Self::unban_draft_key(&channel, &request.request_id);
                                let draft = self.unban_resolution_drafts.entry(key).or_default();

                                chrome::card_frame().show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(format!("@{}", request.user_login))
                                                .font(t::small())
                                                .strong(),
                                        );
                                        if let Some(created_at) = request
                                            .created_at
                                            .as_deref()
                                            .filter(|s| !s.trim().is_empty())
                                        {
                                            ui.separator();
                                            ui.label(
                                                RichText::new(created_at)
                                                    .font(t::tiny())
                                                    .color(t::text_muted()),
                                            );
                                        }
                                    });

                                    if let Some(text) =
                                        request.text.as_deref().filter(|s| !s.trim().is_empty())
                                    {
                                        ui.label(RichText::new(text).font(t::small()));
                                    }

                                    ui.add(
                                        egui::TextEdit::singleline(draft)
                                            .hint_text("Resolution text (optional)")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.horizontal(|ui| {
                                        if ui
                                            .button(
                                                RichText::new("Approve")
                                                    .font(t::small())
                                                    .color(t::green()),
                                            )
                                            .clicked()
                                        {
                                            let resolution = draft.trim();
                                            unban_actions.push((
                                                request.request_id.clone(),
                                                true,
                                                if resolution.is_empty() {
                                                    None
                                                } else {
                                                    Some(resolution.to_owned())
                                                },
                                            ));
                                        }
                                        if ui
                                            .button(
                                                RichText::new("Deny")
                                                    .font(t::small())
                                                    .color(t::red()),
                                            )
                                            .clicked()
                                        {
                                            let resolution = draft.trim();
                                            unban_actions.push((
                                                request.request_id.clone(),
                                                false,
                                                if resolution.is_empty() {
                                                    None
                                                } else {
                                                    Some(resolution.to_owned())
                                                },
                                            ));
                                        }
                                    });
                                });
                                ui.add_space(6.0);
                            }
                        });
                    }
                });

            self.mod_tools_open = window_open;

            if let Some(channel) = refresh_channel {
                self.send_cmd(AppCommand::FetchUnbanRequests { channel });
            }
            if let Some(channel) = moderation_channel {
                for (message_id, sender_user_id, action) in automod_actions {
                    self.send_cmd(AppCommand::ResolveAutoModMessage {
                        channel: channel.clone(),
                        message_id,
                        sender_user_id,
                        action,
                    });
                }
                for (request_id, approve, resolution_text) in unban_actions {
                    self.send_cmd(AppCommand::ResolveUnbanRequest {
                        channel: channel.clone(),
                        request_id,
                        approve,
                        resolution_text,
                    });
                }
            }
        }

        // -- Top bar -----------------------------------------------------------
        // Auto-collapse sidebar into top tabs when window is very narrow so
        // the chat area always has usable space for a super-thin layout.
        let window_width = ctx.screen_rect().width();
        let responsive = responsive_layout(window_width);
        let effective_channel_layout = if responsive.force_top_tabs
            && self.channel_layout == ChannelLayout::Sidebar
            && self.sidebar_visible
        {
            ChannelLayout::TopTabs
        } else {
            self.channel_layout
        };
        let moderation_channel_toolbar = self.active_moderation_channel();
        let moderation_available = moderation_channel_toolbar.is_some();

        TopBottomPanel::top("status_bar")
            .exact_height(responsive.status_bar_height)
            .frame(
                Frame::new()
                    .fill(t::bg_surface())
                    .inner_margin(Margin::symmetric(10, 4))
                    .stroke(egui::Stroke::new(1.0, t::border_subtle())),
            )
            .show(ctx, |ui| {
                let bar_width = ui.available_width();
                let visibility = toolbar_visibility(bar_width, self.irc_beta_enabled);
                let compact_account = visibility.compact_account;
                let sidebar_open =
                    self.channel_layout == ChannelLayout::Sidebar && self.sidebar_visible;
                let (dot_color, conn_label) =
                    connection_indicator(&self.state.connection, self.state.auth.logged_in);
                let row_size = ui.available_size_before_wrap();
                ui.allocate_ui_with_layout(
                    row_size,
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                    ui.spacing_mut().item_spacing = if visibility.compact_controls {
                        egui::vec2(4.0, 0.0)
                    } else {
                        egui::vec2(8.0, 0.0)
                    };

                    if visibility.show_logo {
                        let logo_font = egui::FontId::proportional(15.0);
                        ui.label(
                            RichText::new("crust")
                                .font(logo_font)
                                .strong()
                                .color(t::accent()),
                        );
                    }

                    chrome::toolbar_group_frame().show(ui, |ui| {
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
                            let dot_r = 4.0_f32;
                            let (dot_rect, _) = ui.allocate_exact_size(
                                egui::vec2(dot_r * 2.0 + 2.0, dot_r * 2.0),
                                egui::Sense::hover(),
                            );
                            ui.painter()
                                .circle_filled(dot_rect.center(), dot_r, dot_color);
                            if visibility.show_connection_label {
                                ui.label(
                                    RichText::new(conn_label)
                                        .font(t::small())
                                        .color(t::text_secondary()),
                                );
                            }
                            if visibility.show_join_button
                                && chrome::icon_button(
                                    ui,
                                    ChromeIcon::Join,
                                    "Join a channel",
                                    IconButtonState {
                                        compact: visibility.compact_controls,
                                        ..Default::default()
                                    },
                                )
                                .clicked()
                            {
                                self.join_dialog.toggle();
                            }
                            if visibility.show_join_button && visibility.show_join_text {
                                ui.label(
                                    RichText::new("Join")
                                        .font(t::small())
                                        .color(t::text_secondary()),
                                );
                            }
                        });
                    });

                    if visibility.show_sidebar_actions {
                        chrome::toolbar_group_frame().show(ui, |ui| {
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                                if chrome::icon_button(
                                    ui,
                                    ChromeIcon::Sidebar,
                                    if sidebar_open {
                                        "Hide channel sidebar"
                                    } else {
                                        "Show channel sidebar"
                                    },
                                    IconButtonState {
                                        selected: sidebar_open,
                                        compact: visibility.compact_controls,
                                        ..Default::default()
                                    },
                                )
                                .clicked()
                                {
                                    match self.channel_layout {
                                        ChannelLayout::TopTabs => {
                                            self.channel_layout = ChannelLayout::Sidebar;
                                            self.sidebar_visible = true;
                                        }
                                        ChannelLayout::Sidebar => {
                                            self.sidebar_visible = !self.sidebar_visible;
                                        }
                                    }
                                }
                                if chrome::icon_button(
                                    ui,
                                    ChromeIcon::Tabs,
                                    if effective_channel_layout == ChannelLayout::Sidebar {
                                        "Move channels to top tabs"
                                    } else {
                                        "Move channels to the sidebar"
                                    },
                                    IconButtonState {
                                        selected: effective_channel_layout == ChannelLayout::TopTabs,
                                        compact: visibility.compact_controls,
                                        ..Default::default()
                                    },
                                )
                                .clicked()
                                {
                                    if self.channel_layout == ChannelLayout::Sidebar {
                                        self.channel_layout = ChannelLayout::TopTabs;
                                    } else {
                                        self.channel_layout = ChannelLayout::Sidebar;
                                        self.sidebar_visible = true;
                                    }
                                }
                                },
                            );
                        });
                    }

                    // Right-side items
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing = if visibility.compact_controls {
                            egui::vec2(3.0, 0.0)
                        } else {
                            t::TOOLBAR_SPACING
                        };

                        self.show_topbar_account_button(
                            ui,
                            compact_account,
                            visibility.ultra_compact_account,
                        );
                        if !visibility.compact_controls
                            || visibility.show_emote_count
                            || visibility.show_overflow_menu
                        {
                            ui.separator();
                        }

                        if visibility.show_overflow_menu {
                            ui.menu_button(RichText::new("⋯").font(t::small()), |ui| {
                                if visibility.show_join_in_overflow
                                    && ui
                                        .button(RichText::new("Join channel").font(t::small()))
                                        .clicked()
                                {
                                    self.join_dialog.toggle();
                                    ui.close_menu();
                                }

                                if ui
                                    .button(RichText::new("Settings").font(t::small()))
                                    .clicked()
                                {
                                    self.settings_open = true;
                                    ui.close_menu();
                                }

                                if moderation_available
                                    && ui
                                        .button(RichText::new("Moderation tools").font(t::small()))
                                        .clicked()
                                {
                                    self.mod_tools_open = true;
                                    if let Some(channel) = moderation_channel_toolbar.clone() {
                                        self.send_cmd(AppCommand::FetchUnbanRequests { channel });
                                    }
                                    ui.close_menu();
                                }

                                ui.separator();

                                let sidebar_open = self.channel_layout == ChannelLayout::Sidebar
                                    && self.sidebar_visible;
                                let sidebar_label = if sidebar_open {
                                    "Hide sidebar"
                                } else {
                                    "Show sidebar"
                                };
                                if ui
                                    .button(RichText::new(sidebar_label).font(t::small()))
                                    .clicked()
                                {
                                    match self.channel_layout {
                                        ChannelLayout::TopTabs => {
                                            self.channel_layout = ChannelLayout::Sidebar;
                                            self.sidebar_visible = true;
                                        }
                                        ChannelLayout::Sidebar => {
                                            self.sidebar_visible = !self.sidebar_visible;
                                        }
                                    }
                                    ui.close_menu();
                                }

                                let mode_label = if self.channel_layout == ChannelLayout::Sidebar {
                                    "Use top tabs"
                                } else {
                                    "Use sidebar"
                                };
                                if ui
                                    .button(RichText::new(mode_label).font(t::small()))
                                    .clicked()
                                {
                                    if self.channel_layout == ChannelLayout::Sidebar {
                                        self.channel_layout = ChannelLayout::TopTabs;
                                    } else {
                                        self.channel_layout = ChannelLayout::Sidebar;
                                        self.sidebar_visible = true;
                                    }
                                    ui.close_menu();
                                }

                                ui.separator();

                                if visibility.show_perf_in_overflow
                                    && ui
                                        .button(RichText::new("Perf overlay").font(t::small()))
                                        .clicked()
                                {
                                    self.perf.visible = !self.perf.visible;
                                    ui.close_menu();
                                }

                                if visibility.show_stats_in_overflow
                                    && ui
                                        .button(RichText::new("Analytics").font(t::small()))
                                        .clicked()
                                {
                                    self.analytics_visible = !self.analytics_visible;
                                    ui.close_menu();
                                }

                                if visibility.show_irc_in_overflow
                                    && ui.button(RichText::new("IRC status").font(t::small())).clicked()
                                {
                                    self.irc_status_visible = !self.irc_status_visible;
                                    ui.close_menu();
                                }
                            });
                            if bar_width > 520.0 {
                                ui.separator();
                            }
                        }

                        if visibility.show_emote_count {
                            ui.label(
                                RichText::new(format!("{} emotes", self.emote_bytes.len()))
                                    .font(t::small())
                                    .color(t::text_muted()),
                            );
                            ui.separator();
                        }

                        chrome::toolbar_group_frame().show(ui, |ui| {
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                                let mod_button = ui.add_enabled(
                                    moderation_available,
                                    egui::Button::new(RichText::new("Mod").font(t::tiny())),
                                );
                                if mod_button.clicked() {
                                    self.mod_tools_open = !self.mod_tools_open;
                                    if self.mod_tools_open {
                                        if let Some(channel) = moderation_channel_toolbar.clone() {
                                            self.send_cmd(AppCommand::FetchUnbanRequests { channel });
                                        }
                                    }
                                }
                                if chrome::icon_button(
                                        ui,
                                        ChromeIcon::Settings,
                                        "Open application settings",
                                        IconButtonState {
                                            compact: visibility.compact_controls,
                                            ..Default::default()
                                        },
                                    )
                                    .clicked()
                                {
                                    self.settings_open = true;
                                }
                                if visibility.show_perf_toggle
                                    && chrome::icon_button(
                                        ui,
                                        ChromeIcon::Perf,
                                        "Toggle performance overlay",
                                        IconButtonState {
                                            selected: self.perf.visible,
                                            compact: visibility.compact_controls,
                                            ..Default::default()
                                        },
                                    )
                                    .clicked()
                                {
                                    self.perf.visible = !self.perf.visible;
                                }
                                if visibility.show_stats_toggle
                                    && chrome::icon_button(
                                        ui,
                                        ChromeIcon::Analytics,
                                        "Toggle chatter analytics",
                                        IconButtonState {
                                            selected: self.analytics_visible,
                                            compact: visibility.compact_controls,
                                            ..Default::default()
                                        },
                                    )
                                    .clicked()
                                {
                                    self.analytics_visible = !self.analytics_visible;
                                }
                                if visibility.show_irc_toggle
                                    && chrome::icon_button(
                                        ui,
                                        ChromeIcon::Irc,
                                        "Toggle IRC status window",
                                        IconButtonState {
                                            selected: self.irc_status_visible,
                                            compact: visibility.compact_controls,
                                            ..Default::default()
                                        },
                                    )
                                    .clicked()
                                {
                                    self.irc_status_visible = !self.irc_status_visible;
                                }
                            });
                        });
                    });
                },
            );
            });

        show_channel_info_bars(ctx, &self.state, &self.stream_statuses);

        // -- Channel list: left sidebar OR top tab strip ----------------------
        // Accumulate actions outside the panel closure so we can call &mut self
        // methods after the panel is done drawing.
        let mut ch_selected: Option<ChannelId> = None;
        let mut ch_closed: Option<ChannelId> = None;
        let mut ch_reordered: Option<Vec<ChannelId>> = None;
        let mut ch_drag_split: Option<ChannelId> = None;
        let mut show_split_drop_zone = false;

        match effective_channel_layout {
            // ── Top-tab strip ────────────────────────────────────────────────
            ChannelLayout::TopTabs => {
                let tab_metrics = top_tab_metrics(window_width, self.tab_style);
                TopBottomPanel::top("channel_tabs")
                    .exact_height(tab_metrics.strip_height)
                    .frame(
                        Frame::new()
                            .fill(t::bg_header())
                            .inner_margin(t::TAB_STRIP_MARGIN)
                            .stroke(egui::Stroke::new(1.0, t::border_subtle())),
                    )
                    .show(ctx, |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("channel_tabs_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for ch in self.state.channel_order.iter() {
                                        let is_active =
                                            self.state.active_channel.as_ref() == Some(ch);
                                        let (unread, mentions) = self
                                            .state
                                            .channels
                                            .get(ch)
                                            .map(|s| (s.unread_count, s.unread_mentions))
                                            .unwrap_or((0, 0));

                                        let display = ch.display_name();
                                        let prefix = if ch.is_kick() || ch.is_irc_server_tab() {
                                            ""
                                        } else {
                                            "#"
                                        };
                                        let label = format!("{prefix}{display}");

                                        let is_live = ch.is_twitch()
                                            && self
                                                .live_map_cache
                                                .get(&display.to_ascii_lowercase())
                                            .copied()
                                            .unwrap_or(false);

                                        let (fg, bg) = if is_active {
                                            (t::text_primary(), t::tab_selected_bg())
                                        } else if mentions > 0 {
                                            (t::accent(), t::bg_surface())
                                        } else if unread > 0 {
                                            (t::text_primary(), t::bg_card())
                                        } else {
                                            (t::text_secondary(), t::bg_surface())
                                        };

                                        let tab_stroke = if is_active {
                                            egui::Stroke::new(1.0, t::border_accent())
                                        } else {
                                            egui::Stroke::new(1.0, t::border_subtle())
                                        };

                                        let mut close_clicked = false;
                                        let tab_frame = egui::Frame::new()
                                            .fill(bg)
                                            .stroke(tab_stroke)
                                            .corner_radius(t::RADIUS_SM)
                                            .inner_margin(egui::Margin::symmetric(
                                                tab_metrics.chip_pad_x,
                                                tab_metrics.chip_pad_y,
                                            ))
                                            .show(ui, |ui| {
                                                ui.set_height(tab_metrics.chip_height);
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 4.0;

                                                    if self.show_tab_live_indicators && is_live {
                                                        ui.label(
                                                            RichText::new("●")
                                                                .font(t::small())
                                                                .color(t::red()),
                                                        );
                                                    }

                                                    ui.add_sized(
                                                        [
                                                            tab_metrics.label_width,
                                                            tab_metrics.chip_height - 2.0,
                                                        ],
                                                        egui::Label::new(
                                                            RichText::new(&label)
                                                                .font(t::small())
                                                                .color(fg),
                                                        )
                                                        .truncate(),
                                                    );

                                                    if mentions > 0 {
                                                        channel_tab_badge(
                                                            ui,
                                                            compact_badge_count(mentions),
                                                            t::yellow(),
                                                            Color32::from_rgba_unmultiplied(
                                                                200, 160, 20, 36,
                                                            ),
                                                        );
                                                    } else if unread > 0 {
                                                        channel_tab_badge(
                                                            ui,
                                                            compact_badge_count(unread),
                                                            t::text_secondary(),
                                                            t::bg_raised(),
                                                        );
                                                    }

                                                    if self.show_tab_close_buttons && is_active {
                                                        let close = ui
                                                            .add_sized(
                                                                [
                                                                    tab_metrics.close_button_size,
                                                                    tab_metrics.close_button_size,
                                                                ],
                                                                egui::Button::new(
                                                                    RichText::new("×")
                                                                        .font(t::small())
                                                                        .color(t::text_secondary()),
                                                                )
                                                                .frame(false),
                                                            )
                                                            .on_hover_text("Close channel");
                                                        if close.clicked() {
                                                            close_clicked = true;
                                                        }
                                                    } else {
                                                        ui.allocate_space(egui::vec2(
                                                            tab_metrics.close_button_size,
                                                            tab_metrics.close_button_size,
                                                        ));
                                                    }
                                                });
                                            });

                                        let resp = ui.interact(
                                            tab_frame.response.rect,
                                            egui::Id::new("channel_tab_chip").with(ch.as_str()),
                                            egui::Sense::click_and_drag(),
                                        );

                                        if close_clicked {
                                            ch_closed = Some(ch.clone());
                                        } else if resp.clicked() {
                                            ch_selected = Some(ch.clone());
                                        }

                                        // Drag tab downward → split pane
                                        if resp.dragged() {
                                            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                                                let tab_bottom = ui.max_rect().bottom();
                                                let is_outside = pos.y > tab_bottom + 20.0;
                                                if is_outside {
                                                    show_split_drop_zone = true;
                                                }
                                                // Floating ghost following cursor
                                                let layer_id = egui::LayerId::new(
                                                    egui::Order::Tooltip,
                                                    egui::Id::new("tab_drag_ghost"),
                                                );
                                                let ghost_rect = egui::Rect::from_min_size(
                                                    egui::pos2(pos.x + 10.0, pos.y + 10.0),
                                                    egui::vec2(120.0, 26.0),
                                                );
                                                let painter = ui.ctx().layer_painter(layer_id);
                                                let fill = if is_outside {
                                                    Color32::from_rgba_unmultiplied(
                                                        60, 140, 90, 210,
                                                    )
                                                } else {
                                                    let ac = t::accent();
                                                    Color32::from_rgba_unmultiplied(
                                                        ac.r(),
                                                        ac.g(),
                                                        ac.b(),
                                                        200,
                                                    )
                                                };
                                                painter.rect_filled(
                                                    ghost_rect,
                                                    egui::CornerRadius::same(5),
                                                    fill,
                                                );
                                                painter.text(
                                                    ghost_rect.center(),
                                                    egui::Align2::CENTER_CENTER,
                                                    ch.display_name(),
                                                    t::small(),
                                                    Color32::WHITE,
                                                );
                                                if is_outside {
                                                    painter.text(
                                                        egui::pos2(
                                                            ghost_rect.center().x,
                                                            ghost_rect.bottom() + 2.0,
                                                        ),
                                                        egui::Align2::CENTER_TOP,
                                                        "Split view",
                                                        t::small(),
                                                        Color32::from_rgba_unmultiplied(
                                                            200, 255, 200, 180,
                                                        ),
                                                    );
                                                }
                                            }
                                            ui.ctx().request_repaint();
                                        }
                                        if resp.drag_stopped() {
                                            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                                                let tab_bottom = ui.max_rect().bottom();
                                                if pos.y > tab_bottom + 20.0 {
                                                    ch_drag_split = Some(ch.clone());
                                                }
                                            }
                                        }

                                        // Context menu: common channel actions
                                        resp.context_menu(|ui| {
                                            if ui
                                                .button(
                                                    RichText::new("Switch to channel")
                                                        .font(t::small()),
                                                )
                                                .clicked()
                                            {
                                                ch_selected = Some(ch.clone());
                                                ui.close_menu();
                                            }
                                            if ui
                                                .button(
                                                    RichText::new("Open in split").font(t::small()),
                                                )
                                                .clicked()
                                            {
                                                ch_drag_split = Some(ch.clone());
                                                ui.close_menu();
                                            }
                                            if ui
                                                .button(
                                                    RichText::new("Copy channel").font(t::small()),
                                                )
                                                .clicked()
                                            {
                                                let copy = if ch.is_kick() {
                                                    format!("kick:{}", ch.display_name())
                                                } else if ch.is_irc() {
                                                    if let Some(t) = ch.irc_target() {
                                                        let scheme =
                                                            if t.tls { "ircs" } else { "irc" };
                                                        format!(
                                                            "{scheme}://{}:{}/{}",
                                                            t.host, t.port, t.channel
                                                        )
                                                    } else {
                                                        ch.as_str().to_owned()
                                                    }
                                                } else {
                                                    format!("twitch:{}", ch.display_name())
                                                };
                                                ui.ctx().copy_text(copy);
                                                ui.close_menu();
                                            }
                                            ui.separator();
                                            if ui
                                                .button(
                                                    RichText::new("Remove channel")
                                                        .font(t::small()),
                                                )
                                                .clicked()
                                            {
                                                ch_closed = Some(ch.clone());
                                                ui.close_menu();
                                            }
                                        });
                                    }
                                });
                            });
                    });
            }

            // ── Left sidebar (default) ────────────────────────────────────────
            ChannelLayout::Sidebar if self.sidebar_visible => {
                // Dynamically cap sidebar width so the central panel always gets
                // at least some usable space - allows super-narrow layouts.
                let sidebar_max = (ctx.screen_rect().width() - responsive.min_central_width)
                    .clamp(responsive.sidebar_min_width, t::SIDEBAR_MAX_W);

                SidePanel::left("channel_list")
                    .resizable(true)
                    .default_width(responsive.sidebar_default_width)
                    .min_width(responsive.sidebar_min_width)
                    .max_width(sidebar_max)
                    .frame(
                        Frame::new()
                            .fill(t::bg_surface())
                            .inner_margin(t::SIDEBAR_MARGIN)
                            .stroke(egui::Stroke::new(1.0, t::border_subtle())),
                    )
                    .show(ctx, |ui| {
                        ui.label(
                            RichText::new("CHANNELS")
                                .font(t::heading())
                                .strong()
                                .color(t::text_muted()),
                        );
                        ui.add_space(4.0);
                        ui.add(egui::Separator::default().spacing(6.0));

                        let mut list = ChannelList {
                            channels: &self.state.channel_order,
                            active: self.state.active_channel.as_ref(),
                            channel_states: &self.state.channels,
                            live_channels: Some(&self.live_map_cache),
                            show_live_indicator: self.show_tab_live_indicators,
                            show_close_button: self.show_tab_close_buttons,
                        };
                        let res = list.show(ui);
                        ch_selected = res.selected;
                        ch_closed = res.closed;
                        ch_reordered = res.reordered;
                        ch_drag_split = res.drag_split;
                        show_split_drop_zone = res.dragging_outside;
                    });
            }

            // Sidebar hidden - render nothing; CentralPanel fills the space.
            ChannelLayout::Sidebar => {}
        }

        // Apply channel-list actions gathered above.
        if let Some(ch) = ch_selected {
            self.activate_channel(ch);
        }
        if let Some(ch) = ch_closed {
            if self
                .pending_reply
                .as_ref()
                .map(|r| r.channel == ch)
                .unwrap_or(false)
            {
                self.pending_reply = None;
            }
            // Remove any split pane showing this channel.
            if let Some(idx) = self.split_panes.panes.iter().position(|p| p.channel == ch) {
                self.split_panes.remove_pane(idx);
                if self.split_panes.panes.len() <= 1 {
                    if let Some(p) = self.split_panes.panes.first() {
                        self.state.active_channel = Some(p.channel.clone());
                        self.chat_input_buf =
                            std::mem::take(&mut self.split_panes.panes[0].input_buf);
                    }
                    self.split_panes.panes.clear();
                    self.split_panes.focused = 0;
                } else {
                    self.split_panes.clamp_focus();
                }
            }
            self.send_cmd(AppCommand::LeaveChannel {
                channel: ch.clone(),
            });
            self.state.leave_channel(&ch);
            self.sorted_chatters.remove(&ch);
            self.message_search.remove(&ch);
        }
        if let Some(new_order) = ch_reordered {
            self.state.channel_order = new_order;
        }

        // Drag-to-split: create a new pane for the dragged channel.
        if let Some(ch) = ch_drag_split {
            if let Some(existing_idx) = self.split_panes.panes.iter().position(|p| p.channel == ch)
            {
                self.split_panes.focused = existing_idx;
                self.state.active_channel = Some(ch);
            } else {
                // If not yet in split mode, seed pane 0 with the current active channel.
                if self.split_panes.panes.is_empty() {
                    if let Some(ref active) = self.state.active_channel {
                        if active != &ch {
                            self.split_panes.add_pane(active.clone(), None);
                        }
                    }
                }
                self.split_panes.add_pane(ch.clone(), None);
                self.split_panes.focused = self.split_panes.panes.len().saturating_sub(1);
                self.state.active_channel = Some(ch);
            }
        }

        // -- Analytics right panel -------------------------------------------
        if self.analytics_visible {
            if let Some(active_ch) = self.state.active_channel.clone() {
                if let Some(ch_state) = self.state.channels.get(&active_ch) {
                    SidePanel::right("analytics_panel")
                        .resizable(true)
                        .default_width(responsive.analytics_default_width)
                        .min_width(responsive.analytics_min_width)
                        .max_width(responsive.analytics_max_width)
                        .frame(
                            Frame::new()
                                .fill(t::bg_surface())
                                .inner_margin(t::SIDEBAR_MARGIN)
                                .stroke(egui::Stroke::new(1.0, t::border_subtle())),
                        )
                        .show(ctx, |ui| {
                            self.analytics_panel.show(ui, ch_state);
                        });
                }
            }
        }

        // -- Central area: messages + input ------------------------------------
        CentralPanel::default()
            .frame(Frame::new().fill(t::bg_base()).inner_margin(Margin::ZERO))
            .show(ctx, |ui| {
                // ── Split-pane mode ──────────────────────────────────────
                if self.split_panes.panes.len() > 1 {
                    let n = self.split_panes.panes.len();
                    let total = ui.available_rect_before_wrap();
                    let sep_w = 1.0_f32; // 1px visible divider line
                    let drag_w = 8.0_f32; // wider invisible drag hit-zone
                    let pane_inner_pad = 2.0_f32;
                    let usable_w =
                        total.width() - sep_w * (n as f32 - 1.0);
                    let mut close_pane: Option<usize> = None;
                    let mut close_other_panes: Option<usize> = None;

                    // ── Draggable separators ─────────────────────
                    // Compute cumulative x positions first so we can
                    // place the separator hit-rects.
                    {
                        let mut cx = total.left();
                        for si in 0..(n - 1) {
                            cx += self.split_panes.panes[si].frac * usable_w + sep_w;
                            // Centre the wider drag zone on the 1px line.
                            let drag_rect = egui::Rect::from_min_size(
                                egui::pos2(cx - sep_w * 0.5 - drag_w * 0.5, total.top()),
                                egui::vec2(drag_w, total.height()),
                            );
                            let sep_resp = ui.interact(
                                drag_rect,
                                egui::Id::new("pane_sep").with(si),
                                egui::Sense::drag(),
                            );
                            if sep_resp.hovered() || sep_resp.dragged() {
                                ui.ctx().set_cursor_icon(
                                    egui::CursorIcon::ResizeHorizontal,
                                );
                                // Highlight a thin strip when hovered or dragged.
                                let highlight_w = if sep_resp.dragged() { 3.0 } else { 2.0 };
                                let highlight_alpha = if sep_resp.dragged() { 180_u8 } else { 100 };
                                let ac = t::accent();
                                let highlight_rect = egui::Rect::from_min_size(
                                    egui::pos2(cx - sep_w * 0.5 - highlight_w * 0.5, total.top()),
                                    egui::vec2(highlight_w, total.height()),
                                );
                                ui.painter().rect_filled(
                                    highlight_rect,
                                    egui::CornerRadius::ZERO,
                                    Color32::from_rgba_unmultiplied(ac.r(), ac.g(), ac.b(), highlight_alpha),
                                );
                            }
                            if sep_resp.dragged() {
                                let dx = sep_resp.drag_delta().x;
                                if dx.abs() > 0.0 {
                                    let dfrac = dx / usable_w;
                                    let a = &mut self.split_panes.panes[si];
                                    let new_a = (a.frac + dfrac).max(0.10);
                                    let delta = new_a - a.frac;
                                    self.split_panes.panes[si].frac = new_a;
                                    self.split_panes.panes[si + 1].frac =
                                        (self.split_panes.panes[si + 1].frac - delta)
                                            .max(0.10);
                                    self.split_panes.normalize_fractions();
                                }
                                ui.ctx().request_repaint();
                            }
                        }
                    }

                    for pi in 0..n {
                        let ch = self.split_panes.panes[pi].channel.clone();
                        let is_focused = pi == self.split_panes.focused;

                        // Compute left edge from cumulative fractions so
                        // panes tile perfectly with no float-rounding gaps.
                        let pane_left: f32 = total.left()
                            + (0..pi)
                                .map(|i| {
                                    self.split_panes.panes[i].frac * usable_w
                                        + sep_w
                                })
                                .sum::<f32>();
                        let pane_right: f32 = if pi + 1 < n {
                            // Right edge = next pane's left minus the separator.
                            total.left()
                                + (0..=pi)
                                    .map(|i| {
                                        self.split_panes.panes[i].frac
                                            * usable_w
                                            + sep_w
                                    })
                                    .sum::<f32>()
                                - sep_w
                        } else {
                            // Last pane stretches to the container's right edge.
                            total.right()
                        };
                        let pane_w = pane_right - pane_left;
                        let pane_rect = egui::Rect::from_min_max(
                            egui::pos2(pane_left, total.top()),
                            egui::pos2(pane_right, total.bottom()),
                        );

                        // Separator line (1px divider)
                        if pi > 0 {
                            ui.painter().vline(
                                pane_left - sep_w * 0.5,
                                total.y_range(),
                                egui::Stroke::new(1.0, t::border_subtle()),
                            );
                        }

                        // Click-to-focus
                        let bg_resp = ui.interact(
                            pane_rect,
                            egui::Id::new("split_pane_bg").with(pi),
                            egui::Sense::click(),
                        );
                        if bg_resp.clicked() && !is_focused {
                            self.split_panes.focused = pi;
                            self.state.active_channel = Some(ch.clone());
                        }

                        let mut pane_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(pane_rect)
                                .layout(egui::Layout::top_down(egui::Align::LEFT)),
                        );
                        pane_ui.set_clip_rect(pane_rect);
                        pane_ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

                        // ── Pane header ────────────────────────────────────
                        let (unread_count, unread_mentions) = self
                            .state
                            .channels
                            .get(&ch)
                            .map(|s| (s.unread_count, s.unread_mentions))
                            .unwrap_or((0, 0));
                        let search_open = self
                            .message_search
                            .get(&ch)
                            .map(|s| s.open)
                            .unwrap_or(false);
                        let split_meta = if ch.is_twitch() {
                            let login = ch.display_name().to_ascii_lowercase();
                            split_header_meta_text(
                                self.stream_statuses.get(&login),
                                self.split_header_show_viewer_count,
                                self.split_header_show_game,
                                self.split_header_show_title,
                            )
                        } else {
                            None
                        };
                        let header = show_split_header(
                            &mut pane_ui,
                            pane_rect,
                            &ch,
                            is_focused,
                            unread_count,
                            unread_mentions,
                            search_open,
                            split_meta.as_deref(),
                        );
                        if header.close_clicked {
                            close_pane = Some(pi);
                        }
                        if header.close_others_clicked {
                            close_other_panes = Some(pi);
                        }
                        if header.toggle_search_clicked {
                            let search = self.message_search_mut(&ch);
                            if search.open {
                                search.close();
                            } else {
                                search.request_open();
                            }
                        }

                        // ── Pane chat input (bottom) ─────────────
                        let input_h = t::BAR_H
                            + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom)
                                as f32;
                        let input_rect = egui::Rect::from_min_max(
                            egui::pos2(
                                pane_rect.left() + pane_inner_pad,
                                pane_rect.bottom() - input_h,
                            ),
                            egui::pos2(
                                pane_rect.right() - pane_inner_pad,
                                pane_rect.bottom() - pane_inner_pad,
                            ),
                        );
                        // Paint input background edge-to-edge.
                        ui.painter().rect_filled(
                            input_rect,
                            egui::CornerRadius::ZERO,
                            t::bg_surface(),
                        );
                        ui.painter().hline(
                            input_rect.x_range(),
                            input_rect.top(),
                            egui::Stroke::new(1.0, t::border_subtle()),
                        );
                        {
                        let mut inp_ui = pane_ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(input_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        inp_ui.set_clip_rect(input_rect);
                            let chatters_sorted: &[String] = self
                                .sorted_chatters
                                .get(&ch)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]);
                            let chat = ChatInput {
                                channel: &ch,
                                logged_in: self.state.auth.logged_in,
                                username: self
                                    .state
                                    .auth
                                    .username
                                    .as_deref(),
                                emote_catalog: &self.emote_catalog,
                                emote_bytes: &self.emote_bytes,
                                pending_reply: None,
                                message_history: &self.message_history,
                                slash_usage_counts: &self.slash_usage_counts,
                                known_channels: &self.state.channel_order,
                                chatters: chatters_sorted,
                                prevent_overlong_twitch_messages: self
                                    .prevent_overlong_twitch_messages,
                                animate_emotes: animations_allowed,
                            };
                            let inp = chat.show(
                                &mut inp_ui,
                                &mut self.split_panes.panes[pi].input_buf,
                            );
                            if let Some(text) = inp.send {
                                if self
                                    .message_history
                                    .last()
                                    .map(|s| s.as_str())
                                    != Some(&text)
                                {
                                    self.message_history
                                        .push(text.clone());
                                    if self.message_history.len() > 100 {
                                        self.message_history.remove(0);
                                    }
                                }
                                self.record_slash_usage_from_text(&text);
                                let is_mod = self
                                    .state
                                    .channels
                                    .get(&ch)
                                    .map(|c| c.is_mod)
                                    .unwrap_or(false);
                                let is_bc = self
                                    .state
                                    .auth
                                    .username
                                    .as_deref()
                                    .map(|u| {
                                        u.eq_ignore_ascii_case(
                                            ch.display_name(),
                                        )
                                    })
                                    .unwrap_or(false);
                                let can_mod = is_mod || is_bc;
                                let cc = self
                                    .state
                                    .channels
                                    .get(&ch)
                                    .map(|c| {
                                        c.chatters.len().max(
                                            estimate_chatter_count(c),
                                        )
                                    })
                                    .unwrap_or(0);
                                let pcmd = parse_slash_command(
                                    &text,
                                    &ch,
                                    None,
                                    None,
                                    can_mod,
                                    cc,
                                    self.kick_beta_enabled,
                                    self.irc_beta_enabled,
                                );
                                if let Some(cmd) = pcmd {
                                    if let AppCommand::SendMessage {
                                        text: ref out,
                                        ..
                                    } = cmd
                                    {
                                        if ch.is_irc() {
                                            self.irc_status_panel
                                                .note_outgoing(&ch, out);
                                        }
                                    }
                                    if let AppCommand::ShowUserCard {
                                        ref login,
                                        ref channel,
                                    } = cmd
                                    {
                                        self.user_profile_popup
                                            .set_loading(
                                                login,
                                                vec![],
                                                Some(channel.clone()),
                                                can_mod,
                                            );
                                    }
                                    let _ = self.cmd_tx.try_send(cmd);
                                } else {
                                    if ch.is_irc() {
                                        self.irc_status_panel
                                            .note_outgoing(&ch, &text);
                                    }
                                    let _ = self.cmd_tx.try_send(
                                        AppCommand::SendMessage {
                                            channel: ch.clone(),
                                            text,
                                            reply_to_msg_id: None,
                                            reply: None,
                                        },
                                    );
                                }
                            }
                            if inp.toggle_emote_picker {
                                self.emote_picker.toggle();
                            }
                        }

                        // ── Message list (remaining space) ───────
                        // Region between header bottom and input top.
                        let mut search_h = 0.0;
                        let content_top = pane_rect.top() + SPLIT_HEADER_HEIGHT + pane_inner_pad;
                        let content_bottom = input_rect.top() - pane_inner_pad;
                        if let Some(ch_state) = self.state.channels.get(&ch) {
                            let search_open = self
                                .message_search
                                .get(&ch)
                                .map(|s| s.open)
                                .unwrap_or(false);
                            if search_open {
                                if let Some(search) = self.message_search.get_mut(&ch) {
                                    if should_use_search_window(pane_w) {
                                        show_message_search_window(
                                            ctx,
                                            &ch,
                                            &ch_state.messages,
                                            search,
                                            self.always_on_top,
                                        );
                                    } else {
                                        let search_rect = egui::Rect::from_min_max(
                                            egui::pos2(input_rect.left(), content_top),
                                            egui::pos2(input_rect.right(), content_bottom),
                                        );
                                        let mut search_ui = pane_ui.new_child(
                                            egui::UiBuilder::new()
                                                .max_rect(search_rect)
                                                .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                        );
                                        search_ui.set_clip_rect(search_rect);
                                        search_h = show_message_search_inline(
                                            &mut search_ui,
                                            &ch,
                                            &ch_state.messages,
                                            search,
                                        ) + pane_inner_pad;
                                    }
                                    if search.take_load_more_local_request() {
                                        let oldest_ts = ch_state
                                            .messages
                                            .front()
                                            .map(|m| m.timestamp.timestamp_millis())
                                            .unwrap_or(i64::MAX);
                                        self.request_older_local_history(&ch, oldest_ts);
                                    }
                                }
                            }
                        }
                        let msg_rect = egui::Rect::from_min_max(
                            egui::pos2(input_rect.left(), content_top + search_h),
                            egui::pos2(input_rect.right(), content_bottom),
                        );
                        if msg_rect.height() > 8.0 {
                            if let Some(ch_state) = self.state.channels.get(&ch) {
                            let is_bc = self
                                .state
                                .auth
                                .username
                                .as_deref()
                                .map(|u| {
                                    u.eq_ignore_ascii_case(
                                        ch.display_name(),
                                    )
                                })
                                .unwrap_or(false);
                            let is_mod = ch_state.is_mod || is_bc;
                            let mut msg_ui = pane_ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(msg_rect)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                            );
                            msg_ui.set_clip_rect(msg_rect);
                            let ml = MessageList::new(
                                &ch_state.messages,
                                &self.emote_bytes,
                                &self.cmd_tx,
                                &ch,
                                &self.link_previews,
                                self.message_search.get(&ch),
                                self.collapse_long_messages,
                                self.collapse_long_message_lines,
                                animations_allowed,
                                self.show_timestamps,
                                self.show_timestamp_seconds,
                                self.use_24h_timestamps,
                                is_mod,
                                &self.ignores_set,
                                &self.highlight_rules,
                                &self.filter_records,
                                &self.mod_action_presets,
                            )
                            .show(&mut msg_ui);
                            frame_chat_stats.accumulate(&ml.perf_stats);
                            if let Some(r) = ml.reply {
                                self.pending_reply = Some(PendingReply {
                                    channel: ch.clone(),
                                    info: r,
                                });
                            }
                            if let Some((login, badges)) =
                                ml.profile_request
                            {
                                self.user_profile_popup.set_loading(
                                    &login,
                                    badges,
                                    Some(ch.clone()),
                                    ch_state.is_mod || is_bc,
                                );
                            }
                            }
                        }
                    }

                    // Close-pane actions
                    if let Some(keep_idx) = close_other_panes {
                        if let Some(pane) = self.split_panes.panes.get_mut(keep_idx) {
                            let keep_channel = pane.channel.clone();
                            let keep_input = std::mem::take(&mut pane.input_buf);
                            self.state.active_channel = Some(keep_channel);
                            self.chat_input_buf = keep_input;
                            self.split_panes.panes.clear();
                            self.split_panes.focused = 0;
                        }
                    } else if let Some(idx) = close_pane {
                        self.split_panes.remove_pane(idx);
                        if self.split_panes.panes.len() <= 1 {
                            if let Some(p) =
                                self.split_panes.panes.first()
                            {
                                self.state.active_channel =
                                    Some(p.channel.clone());
                                self.chat_input_buf = std::mem::take(
                                    &mut self.split_panes.panes[0]
                                        .input_buf,
                                );
                            }
                            self.split_panes.panes.clear();
                            self.split_panes.focused = 0;
                        } else {
                            self.split_panes.clamp_focus();
                            if let Some(ch) =
                                self.split_panes.focused_channel()
                            {
                                self.state.active_channel =
                                    Some(ch.clone());
                            }
                        }
                    }

                    // Emote picker → focused pane
                    if let Some(code) = self.emote_picker.show(
                        ctx,
                        &self.emote_catalog,
                        &self.emote_bytes,
                        &self.cmd_tx,
                        animations_allowed,
                    ) {
                        if let Some(pane) = self
                            .split_panes
                            .panes
                            .get_mut(self.split_panes.focused)
                        {
                            if !pane.input_buf.is_empty()
                                && !pane.input_buf.ends_with(' ')
                            {
                                pane.input_buf.push(' ');
                            }
                            pane.input_buf.push_str(&code);
                            pane.input_buf.push(' ');
                        }
                    }
                // ── Classic single-channel mode ──────────────────────────
                } else if let Some(active_ch) = self.state.active_channel.clone() {
                    let active_reply = self
                        .pending_reply
                        .as_ref()
                        .filter(|r| r.channel == active_ch)
                        .map(|r| r.info.clone());

                    // Input tray pinned to bottom
                    let input_panel_h = if active_reply.is_some() {
                        64.0
                    } else {
                        t::BAR_H + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom) as f32
                    };
                    TopBottomPanel::bottom("chat_input_panel")
                        .resizable(false)
                        .exact_height(input_panel_h)
                        .frame(
                            Frame::new()
                                .fill(t::bg_surface())
                                .inner_margin(Margin::ZERO)
                                .stroke(egui::Stroke::new(1.0, t::border_subtle())),
                        )
                        .show_inside(ui, |ui| {
                            // Collect sorted chatters for @username autocomplete
                            let chatters_sorted: &[String] = self
                                .sorted_chatters
                                .get(&active_ch)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]);
                            let chat = ChatInput {
                                channel: &active_ch,
                                logged_in: self.state.auth.logged_in,
                                username: self.state.auth.username.as_deref(),
                                emote_catalog: &self.emote_catalog,
                                emote_bytes: &self.emote_bytes,
                                pending_reply: active_reply.as_ref(),
                                message_history: &self.message_history,
                                slash_usage_counts: &self.slash_usage_counts,
                                known_channels: &self.state.channel_order,
                                chatters: chatters_sorted,
                                prevent_overlong_twitch_messages: self
                                    .prevent_overlong_twitch_messages,
                                animate_emotes: animations_allowed,
                            };
                            let result = chat.show(ui, &mut self.chat_input_buf);
                            if result.dismiss_reply && active_reply.is_some() {
                                self.pending_reply = None;
                            }
                            if let Some(text) = result.send {
                                // Push to history (cap at 100)
                                if self.message_history.last().map(|s| s.as_str()) != Some(&text) {
                                    self.message_history.push(text.clone());
                                    if self.message_history.len() > 100 {
                                        self.message_history.remove(0);
                                    }
                                }
                                self.record_slash_usage_from_text(&text);
                                let reply_to_msg_id =
                                    active_reply.as_ref().map(|r| r.parent_msg_id.clone());
                                if active_reply.is_some() {
                                    self.pending_reply = None;
                                }
                                let is_mod = self
                                    .state
                                    .channels
                                    .get(&active_ch)
                                    .map(|c| c.is_mod)
                                    .unwrap_or(false);
                                // Broadcaster has full mod powers in their own channel.
                                let is_broadcaster = self
                                    .state
                                    .auth
                                    .username
                                    .as_deref()
                                    .map(|u| u.eq_ignore_ascii_case(active_ch.display_name()))
                                    .unwrap_or(false);
                                let can_moderate = is_mod || is_broadcaster;
                                let chatters_count = self
                                    .state
                                    .channels
                                    .get(&active_ch)
                                    .map(|c| c.chatters.len().max(estimate_chatter_count(c)))
                                    .unwrap_or(0);

                                let parsed_cmd = parse_slash_command(
                                    &text,
                                    &active_ch,
                                    reply_to_msg_id.clone(),
                                    active_reply.clone(),
                                    can_moderate,
                                    chatters_count,
                                    self.kick_beta_enabled,
                                    self.irc_beta_enabled,
                                );

                                if !self.state.auth.logged_in {
                                    match parsed_cmd {
                                        Some(cmd) if is_anonymous_local_command(&cmd) => {
                                            // Some slash commands manipulate the popup directly.
                                            if let AppCommand::ShowUserCard {
                                                ref login,
                                                ref channel,
                                            } = cmd
                                            {
                                                self.user_profile_popup.set_loading(
                                                    login,
                                                    vec![],
                                                    Some(channel.clone()),
                                                    can_moderate,
                                                );
                                            }
                                            self.send_cmd(cmd);
                                        }
                                        Some(_) => {
                                            self.send_cmd(AppCommand::InjectLocalMessage {
                                                channel: active_ch.clone(),
                                                text: "Anonymous mode allows local slash commands only. Log in to run server commands or send chat messages. Try /help.".to_owned(),
                                            });
                                        }
                                        None => {
                                            let text = if text.trim_start().starts_with('/') {
                                                "That slash command is not available in anonymous mode. Use /help for local commands.".to_owned()
                                            } else {
                                                "Anonymous mode cannot send chat messages. Log in to chat, or run local commands like /help.".to_owned()
                                            };
                                            self.send_cmd(AppCommand::InjectLocalMessage {
                                                channel: active_ch.clone(),
                                                text,
                                            });
                                        }
                                    }
                                } else if let Some(cmd) = parsed_cmd {
                                    if let AppCommand::SendMessage { text: ref outgoing_text, .. } = cmd {
                                        if active_ch.is_irc() {
                                            self.irc_status_panel.note_outgoing(&active_ch, outgoing_text);
                                        }
                                    }
                                    // Some slash commands manipulate the popup directly.
                                    if let AppCommand::ShowUserCard {
                                        ref login,
                                        ref channel,
                                    } = cmd
                                    {
                                        self.user_profile_popup.set_loading(
                                            login,
                                            vec![],
                                            Some(channel.clone()),
                                            can_moderate,
                                        );
                                    }
                                    self.send_cmd(cmd);
                                } else {
                                    if active_ch.is_irc() {
                                        self.irc_status_panel.note_outgoing(&active_ch, &text);
                                    }
                                    self.send_cmd(AppCommand::SendMessage {
                                        channel: active_ch.clone(),
                                        text,
                                        reply_to_msg_id,
                                        reply: active_reply,
                                    });
                                }
                            }
                            if result.toggle_emote_picker {
                                self.emote_picker.toggle();
                            }
                        });

                    // Emote picker floating window
                    if let Some(code) = self.emote_picker.show(
                        ctx,
                        &self.emote_catalog,
                        &self.emote_bytes,
                        &self.cmd_tx,
                        animations_allowed,
                    ) {
                        if !self.chat_input_buf.is_empty() && !self.chat_input_buf.ends_with(' ') {
                            self.chat_input_buf.push(' ');
                        }
                        self.chat_input_buf.push_str(&code);
                        self.chat_input_buf.push(' ');
                    }

                    // Messages above the input
                    if let Some(state) = self.state.channels.get(&active_ch) {
                        if self
                            .message_search
                            .get(&active_ch)
                            .map(|s| s.open)
                            .unwrap_or(false)
                        {
                            if let Some(search) = self.message_search.get_mut(&active_ch) {
                                if should_use_search_window(ui.available_width()) {
                                    show_message_search_window(
                                        ctx,
                                        &active_ch,
                                        &state.messages,
                                        search,
                                        self.always_on_top,
                                    );
                                } else {
                                    let search_rect = egui::Rect::from_min_max(
                                        egui::pos2(ui.min_rect().left() + 6.0, ui.min_rect().top() + 6.0),
                                        egui::pos2(ui.max_rect().right() - 6.0, ui.max_rect().bottom()),
                                    );
                                    let mut search_ui = ui.new_child(
                                        egui::UiBuilder::new()
                                            .max_rect(search_rect)
                                            .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                    );
                                    search_ui.set_clip_rect(search_rect);
                                    let search_h = show_message_search_inline(
                                        &mut search_ui,
                                        &active_ch,
                                        &state.messages,
                                        search,
                                    ) + 10.0;
                                    ui.allocate_space(egui::vec2(0.0, search_h));
                                }
                                if search.take_load_more_local_request() {
                                    let oldest_ts = state
                                        .messages
                                        .front()
                                        .map(|m| m.timestamp.timestamp_millis())
                                        .unwrap_or(i64::MAX);
                                    self.request_older_local_history(&active_ch, oldest_ts);
                                }
                            }
                        }
                        let is_broadcaster = self
                            .state
                            .auth
                            .username
                            .as_deref()
                            .map(|u| u.eq_ignore_ascii_case(active_ch.display_name()))
                            .unwrap_or(false);
                        let is_mod = state.is_mod || is_broadcaster;
                        // Small left inset so messages aren't flush against the sidebar.
                        ui.add_space(0.0); // force cursor
                        let msg_rect = ui.available_rect_before_wrap();
                        let inset_rect = egui::Rect::from_min_max(
                            egui::pos2(msg_rect.left() + 6.0, msg_rect.top()),
                            msg_rect.max,
                        );
                        let mut msg_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(inset_rect)
                                .layout(egui::Layout::top_down(egui::Align::LEFT)),
                        );
                        msg_ui.set_clip_rect(inset_rect);
                        let ml_result = MessageList::new(
                            &state.messages,
                            &self.emote_bytes,
                            &self.cmd_tx,
                            &active_ch,
                            &self.link_previews,
                            self.message_search.get(&active_ch),
                            self.collapse_long_messages,
                            self.collapse_long_message_lines,
                            animations_allowed,
                            self.show_timestamps,
                            self.show_timestamp_seconds,
                            self.use_24h_timestamps,
                            is_mod,
                            &self.ignores_set,
                            &self.highlight_rules,
                            &self.filter_records,
                            &self.mod_action_presets,
                        )
                        .show(&mut msg_ui);
                        frame_chat_stats.accumulate(&ml_result.perf_stats);
                        if let Some(r) = ml_result.reply {
                            self.pending_reply = Some(PendingReply {
                                channel: active_ch.clone(),
                                info: r,
                            });
                        }
                        if let Some((login, badges)) = ml_result.profile_request {
                            self.user_profile_popup.set_loading(
                                &login,
                                badges,
                                Some(active_ch.clone()),
                                is_mod,
                            );
                        }
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("Click \"+ Join\" to open a Twitch channel.")
                                .color(t::text_muted())
                                .font(t::body()),
                        );
                    });
                }
            });

        let picker_prefs = self.emote_picker.preferences();
        if self.emote_picker_prefs_last_saved.as_ref() != Some(&picker_prefs) {
            self.send_cmd(AppCommand::SetEmotePickerPreferences {
                favorites: picker_prefs.favorites.clone(),
                recent: picker_prefs.recent.clone(),
                provider_boost: picker_prefs.provider_boost.clone(),
            });
            self.emote_picker_prefs_last_saved = Some(picker_prefs);
        }

        // -- Split drop-zone overlay -----------------------------------------
        // Pulsing translucent overlay shown over the central area when a
        // channel is being dragged outside the sidebar / tab strip.
        if show_split_drop_zone {
            let time = ctx.input(|i| i.time) as f32;
            let pulse = (time * 3.0).sin() * 0.5 + 0.5; // 0..1
            let alpha = (30.0 + pulse * 35.0) as u8;
            let border_alpha = (80.0 + pulse * 80.0) as u8;
            let ac = t::accent();

            egui::Area::new(egui::Id::new("split_drop_zone"))
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    let screen = ctx.screen_rect();
                    // Cover most of the central area.
                    let zone_rect = screen.shrink(4.0);
                    ui.painter().rect(
                        zone_rect,
                        egui::CornerRadius::same(8),
                        Color32::from_rgba_unmultiplied(ac.r(), ac.g(), ac.b(), alpha),
                        egui::Stroke::new(
                            2.0,
                            Color32::from_rgba_unmultiplied(ac.r(), ac.g(), ac.b(), border_alpha),
                        ),
                        egui::epaint::StrokeKind::Outside,
                    );
                    // Center label.
                    ui.painter().text(
                        zone_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drop to split",
                        t::heading(),
                        Color32::from_rgba_unmultiplied(
                            255,
                            255,
                            255,
                            (120.0 + pulse * 100.0) as u8,
                        ),
                    );
                });
            ctx.request_repaint();
        }

        // -- Event toast overlay ---------------------------------------------
        // Expire toasts older than 5 s, then render remaining ones as stacked
        // floating banners anchored to the top-right of the screen.
        self.flush_event_toast_queue();
        self.event_toasts
            .retain(|t| t.born.elapsed().as_secs_f32() < EVENT_TOAST_TTL_SECS);
        for (i, toast) in self.event_toasts.iter().enumerate() {
            let age = toast.born.elapsed().as_secs_f32();
            let opacity = if age < 0.25 {
                age / 0.25
            } else if age > 4.0 {
                1.0 - (age - 4.0)
            } else {
                1.0_f32
            };
            // Slide in from the right on entry.
            let slide_x = if age < 0.25 {
                (1.0 - age / 0.25) * 28.0
            } else {
                0.0
            };
            egui::Area::new(egui::Id::new("event_toast").with(i))
                .anchor(
                    egui::Align2::RIGHT_TOP,
                    egui::vec2(-14.0 - slide_x, 58.0 + i as f32 * 50.0),
                )
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    let border_col = Color32::from_rgba_unmultiplied(
                        toast.hue.r(),
                        toast.hue.g(),
                        toast.hue.b(),
                        (160.0 * opacity) as u8,
                    );
                    let fill_col = {
                        let o = t::overlay_fill();
                        Color32::from_rgba_unmultiplied(
                            o.r(),
                            o.g(),
                            o.b(),
                            (225.0 * opacity) as u8,
                        )
                    };
                    let frame_resp = egui::Frame::new()
                        .fill(fill_col)
                        .stroke(egui::Stroke::new(1.5, border_col))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(14, 8))
                        .show(ui, |ui| {
                            ui.set_opacity(opacity);
                            ui.label(
                                RichText::new(&toast.text)
                                    .font(t::body())
                                    .color(Color32::WHITE),
                            );
                        });

                    if toast.confetti {
                        let rect = frame_resp.response.rect.expand(4.0);
                        let painter = ui.painter();
                        for n in 0..14 {
                            let seed = (n as f32) * 17.0 + (i as f32) * 5.0;
                            let base_x = rect.left() + ((seed * 0.37).fract() * rect.width());
                            let drop = ((seed * 0.11) + age * 0.85).fract();
                            let y = rect.top() - 3.0 + drop * (rect.height() + 10.0);
                            let drift = ((age * 5.2) + seed * 0.23).sin() * 3.2;
                            let x = (base_x + drift).clamp(rect.left(), rect.right());
                            let c = match n % 4 {
                                0 => t::raid_cyan(),
                                1 => t::gold(),
                                2 => t::accent(),
                                _ => t::bits_orange(),
                            };
                            let col = Color32::from_rgba_unmultiplied(
                                c.r(),
                                c.g(),
                                c.b(),
                                (180.0 * opacity) as u8,
                            );
                            painter.circle_filled(
                                egui::pos2(x, y),
                                1.6 + (n % 3) as f32 * 0.45,
                                col,
                            );
                        }
                    }
                });
        }
        // Keep animating while toasts are live.
        if !self.event_toasts.is_empty() || !self.event_toast_queue.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }

        self.perf.set_chat_stats(frame_chat_stats);
        self.perf.show(ctx);

        if self.irc_status_visible {
            self.irc_status_visible = self.irc_status_panel.show(
                ctx,
                self.irc_status_visible,
                self.state.active_channel.as_ref(),
            );
        }

        if self.appearance_snapshot() != appearance_before {
            if !self.irc_beta_enabled {
                self.irc_status_visible = false;
            }
            self.send_appearance_settings();
        }
    }
}

// Helper functions

fn channel_tab_badge(ui: &mut egui::Ui, label: String, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .stroke(egui::Stroke::new(1.0, fg.gamma_multiply(0.35)))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(5, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(label).font(t::tiny()).strong().color(fg));
        });
}

fn compact_badge_count(count: u32) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

fn truncate_with_ellipsis(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = input.chars().collect();
    if chars.len() <= max_chars {
        return input.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let head: String = chars.into_iter().take(max_chars - 1).collect();
    format!("{head}…")
}

fn connection_indicator(state: &ConnectionState, logged_in: bool) -> (Color32, &'static str) {
    match state {
        ConnectionState::Connected if logged_in => (t::green(), "Connected"),
        ConnectionState::Connected => (t::green(), "Connected (anon)"),
        ConnectionState::Connecting => (t::yellow(), "Connecting..."),
        ConnectionState::Reconnecting { .. } => (t::yellow(), "Reconnecting..."),
        ConnectionState::Disconnected => (t::red(), "Disconnected"),
        ConnectionState::Error(_) => (t::red(), "Error"),
    }
}

fn split_header_meta_text(
    status: Option<&StreamStatusInfo>,
    show_viewers: bool,
    show_game: bool,
    show_title: bool,
) -> Option<String> {
    let status = status?;
    let mut parts: Vec<String> = Vec::new();
    if status.is_live {
        parts.push("LIVE".to_owned());
    }
    if show_viewers {
        if let Some(viewers) = status.viewers {
            parts.push(format!("{} viewers", format_viewers_short(viewers)));
        }
    }
    if show_game {
        if let Some(game) = status.game.as_deref().filter(|game| !game.is_empty()) {
            parts.push(game.to_owned());
        }
    }
    if show_title {
        if let Some(title) = status.title.as_deref().filter(|title| !title.is_empty()) {
            parts.push(truncate_with_ellipsis(title, 32));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn format_viewers_short(viewers: u64) -> String {
    match viewers {
        0..=999 => viewers.to_string(),
        1_000..=999_999 => {
            let raw = format!("{:.1}", viewers as f64 / 1_000.0);
            format!("{}k", raw.trim_end_matches('0').trim_end_matches('.'))
        }
        _ => {
            let raw = format!("{:.1}", viewers as f64 / 1_000_000.0);
            format!("{}m", raw.trim_end_matches('0').trim_end_matches('.'))
        }
    }
}

fn is_likely_animated_image_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains(".gif") || lower.contains(".webp")
}

fn is_likely_animated_image_bytes(raw: &[u8]) -> bool {
    let is_gif = raw.len() >= 6 && (&raw[..6] == b"GIF87a" || &raw[..6] == b"GIF89a");
    if is_gif {
        let frame_markers = raw.iter().filter(|&&b| b == 0x2C).take(2).count();
        if frame_markers >= 2 {
            return true;
        }
    }

    let is_webp = raw.len() >= 12 && &raw[..4] == b"RIFF" && &raw[8..12] == b"WEBP";
    is_webp && raw.windows(4).any(|w| w == b"ANIM")
}

fn decode_static_image_frame(raw: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(raw).ok()?;
    dynamic_image_to_color_image(img)
}

fn dynamic_image_to_color_image(img: DynamicImage) -> Option<egui::ColorImage> {
    let rgba = img.to_rgba8();
    let w = usize::try_from(rgba.width()).ok()?;
    let h = usize::try_from(rgba.height()).ok()?;
    let pixels = rgba.into_raw();
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels))
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

fn install_system_fallback_fonts(ctx: &Context) {
    // Ordered by Unicode coverage breadth. We load ALL that exist and push
    // them as fallbacks so glyphs missing in one font are found in the next.
    const CANDIDATES: &[(&str, &str)] = &[
        // DejaVu - good Latin/Greek/Cyrillic/symbols coverage
        ("dejavu", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        ("dejavu", "/usr/share/fonts/TTF/DejaVuSans.ttf"),
        // Noto Sans - broad multilingual coverage (Latin/Greek/Cyrillic/etc.)
        (
            "noto",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        ),
        ("noto", "/usr/share/fonts/noto/NotoSans-Regular.ttf"),
        // Noto CJK - Japanese / Chinese / Korean (separate name so it loads
        // even when plain NotoSans was already found above)
        (
            "noto_cjk",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        ),
        ("noto_cjk", "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc"),
        (
            "noto_cjk",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ),
        (
            "noto_cjk",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        ),
        // Noto Emoji - colour emoji fallback
        (
            "noto_emoji",
            "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        ),
        ("noto_emoji", "/usr/share/fonts/noto/NotoColorEmoji.ttf"),
        ("noto_emoji", "/usr/share/fonts/noto/NotoEmoji-Regular.ttf"),
        // GNU Unifont - near-complete BMP coverage as last resort
        ("unifont", "/usr/share/fonts/truetype/unifont/unifont.ttf"),
        ("unifont", "/usr/share/fonts/unifont/unifont.ttf"),
        ("unifont", "/usr/share/fonts/misc/unifont.ttf"),
        // GNU FreeFont
        ("freesans", "/usr/share/fonts/gnu-free/FreeSans.ttf"),
        // macOS
        (
            "arial_unicode",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ),
        // macOS CJK
        ("mac_cjk", "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc"),
        ("mac_cjk", "/System/Library/Fonts/Hiragino Sans GB.ttc"),
        ("mac_cjk", "/Library/Fonts/Arial Unicode.ttf"),
        // Windows - Latin / symbols
        ("seguisym", "C:\\Windows\\Fonts\\seguisym.ttf"),
        ("arial", "C:\\Windows\\Fonts\\arial.ttf"),
        // Windows - Japanese  (Yu Gothic is the modern default JP font)
        ("win_jp", "C:\\Windows\\Fonts\\YuGothR.ttc"),
        ("win_jp", "C:\\Windows\\Fonts\\YuGothM.ttc"),
        ("win_jp", "C:\\Windows\\Fonts\\msgothic.ttc"),
        ("win_jp", "C:\\Windows\\Fonts\\meiryo.ttc"),
        // Windows - Chinese Simplified
        ("win_sc", "C:\\Windows\\Fonts\\msyh.ttc"),
        ("win_sc", "C:\\Windows\\Fonts\\simsun.ttc"),
        // Windows - Chinese Traditional
        ("win_tc", "C:\\Windows\\Fonts\\msjh.ttc"),
        ("win_tc", "C:\\Windows\\Fonts\\mingliu.ttc"),
        // Windows - Korean
        ("win_kr", "C:\\Windows\\Fonts\\malgun.ttf"),
        ("win_kr", "C:\\Windows\\Fonts\\gulim.ttc"),
        // Windows - Thai / Arabic / Hebrew / Devanagari
        ("win_tahoma", "C:\\Windows\\Fonts\\tahoma.ttf"),
    ];

    // Start from egui defaults so built-in Ubuntu font is preserved.
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = 0usize;
    let mut seen_names = std::collections::HashSet::new();

    for (name, path) in CANDIDATES {
        // Only load the first hit for each logical name (e.g. skip duplicate
        // dejavu paths once one is found).
        if seen_names.contains(name) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(path) {
            tracing::info!("Loaded fallback font [{name}]: {path}");
            let key = format!("fallback_{name}");
            fonts
                .font_data
                .insert(key.clone(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(key.clone());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push(key);
            seen_names.insert(name);
            loaded += 1;
        }
    }

    if loaded == 0 {
        tracing::warn!("No system fallback fonts found; some Unicode glyphs may render as boxes");
    }
    ctx.set_fonts(fonts);
}

// Slash-command parser

/// Parse a typed message that starts with `/`.  Returns an `AppCommand` to
/// dispatch for known commands, or `None` to fall through as a normal chat
/// message (so Twitch's IRC server can handle standard commands like /ban,
/// /timeout, /clear, /slow, etc.).
///
/// `reply_to_msg_id` is forwarded for commands that end up as `SendMessage`.
fn parse_slash_command(
    text: &str,
    channel: &ChannelId,
    reply_to_msg_id: Option<String>,
    reply: Option<ReplyInfo>,
    is_mod: bool,
    chatters_count: usize,
    kick_beta_enabled: bool,
    irc_beta_enabled: bool,
) -> Option<AppCommand> {
    if !text.starts_with('/') {
        return None;
    }

    // Split into /<cmd> [<rest>]
    let without_slash = &text[1..];
    let (cmd, rest) = without_slash
        .split_once(char::is_whitespace)
        .map(|(c, r)| (c, r.trim()))
        .unwrap_or((without_slash, ""));
    let cmd_lower = cmd.to_ascii_lowercase();

    match cmd_lower.as_str() {
        // Purely local commands
        "help" => {
            let msg = render_help_message();
            Some(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text: msg,
            })
        }

        "clearmessages" => Some(AppCommand::ClearLocalMessages {
            channel: channel.clone(),
        }),

        "chatters" => {
            let msg = format!("There are {} chatters currently connected.", chatters_count);
            Some(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text: msg,
            })
        }

        // Twitch Helix poll management.
        "poll" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage = "Usage: /poll <title> | <choice 1> | <choice 2> [| ...] [--duration <15..1800>] OR /poll --title \"<title>\" --choice \"<choice 1>\" --choice \"<choice 2>\" [--duration <15..1800>|<60s|1m>] [--points <n>]";
            if rest.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            let parsed = if rest.contains("--title") || rest.contains("--choice") {
                parse_poll_flag_args(rest)
            } else {
                let (spec, duration_opt) = extract_duration_flag(rest);
                let (spec, points_opt) = extract_points_flag(&spec);
                let duration_secs = duration_opt.unwrap_or(60).clamp(15, 1800);
                let parts = parse_pipe_args(&spec);
                if parts.len() < 3 {
                    None
                } else {
                    Some(ParsedPollSpec {
                        title: parts[0].clone(),
                        choices: parts[1..].to_vec(),
                        duration_secs,
                        channel_points_per_vote: points_opt,
                    })
                }
            };

            let Some(parsed) = parsed else {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            };

            let choices = parsed.choices;
            if choices.len() > 5 {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Twitch polls support 2 to 5 choices.".to_owned(),
                });
            }

            Some(AppCommand::CreatePoll {
                channel: channel.clone(),
                title: parsed.title,
                choices,
                duration_secs: parsed.duration_secs,
                channel_points_per_vote: parsed.channel_points_per_vote,
            })
        }

        "endpoll" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            Some(AppCommand::EndPoll {
                channel: channel.clone(),
                status: "ARCHIVED".to_owned(),
            })
        }

        "cancelpoll" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            Some(AppCommand::EndPoll {
                channel: channel.clone(),
                status: "TERMINATED".to_owned(),
            })
        }

        // Twitch viewer participation commands.
        "vote" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Poll voting is only supported for Twitch channels.".to_owned(),
                });
            }
            if rest.trim().is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /vote <choice number>".to_owned(),
                });
            }
            Some(AppCommand::SendMessage {
                channel: channel.clone(),
                text: text.to_owned(),
                reply_to_msg_id: None,
                reply: None,
            })
        }

        "redeem" | "reward" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Channel points redemption is only supported for Twitch channels."
                        .to_owned(),
                });
            }
            if rest.trim().is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /redeem <reward name>".to_owned(),
                });
            }
            Some(AppCommand::SendMessage {
                channel: channel.clone(),
                text: format!("/redeem {}", rest.trim()),
                reply_to_msg_id: None,
                reply: None,
            })
        }

        // Twitch Helix prediction management.
        "prediction" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage = "Usage: /prediction <title> | <outcome 1> | <outcome 2> [| ...] [--duration <30..1800>]";
            if rest.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            let (spec, duration_opt) = extract_duration_flag(rest);
            let duration_secs = duration_opt.unwrap_or(120).clamp(30, 1800);
            let parts = parse_pipe_args(&spec);
            if parts.len() < 3 {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }
            let title = parts[0].clone();
            let outcomes = parts[1..].to_vec();
            if outcomes.len() > 10 {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Twitch predictions support 2 to 10 outcomes.".to_owned(),
                });
            }

            Some(AppCommand::CreatePrediction {
                channel: channel.clone(),
                title,
                outcomes,
                duration_secs,
            })
        }

        "lockprediction" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            Some(AppCommand::LockPrediction {
                channel: channel.clone(),
            })
        }

        "endprediction" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            let idx: usize = match rest.trim().parse() {
                Ok(v) if v >= 1 => v,
                _ => {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: "Usage: /endprediction <winning outcome index starting at 1>"
                            .to_owned(),
                    });
                }
            };
            Some(AppCommand::ResolvePrediction {
                channel: channel.clone(),
                winning_outcome_index: idx,
            })
        }

        "cancelprediction" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Prediction commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            Some(AppCommand::CancelPrediction {
                channel: channel.clone(),
            })
        }

        "commercial" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Commercial commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Commercial commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage = "Usage: /commercial [30|60|90|120|150|180]";
            let length_secs = if rest.is_empty() {
                30
            } else {
                match rest.trim().parse::<u32>() {
                    Ok(v) if matches!(v, 30 | 60 | 90 | 120 | 150 | 180) => v,
                    _ => {
                        return Some(AppCommand::InjectLocalMessage {
                            channel: channel.clone(),
                            text: usage.to_owned(),
                        });
                    }
                }
            };

            Some(AppCommand::StartCommercial {
                channel: channel.clone(),
                length_secs,
            })
        }

        "marker" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Marker commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Marker commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let description = if rest.is_empty() {
                None
            } else {
                let desc = rest.trim();
                if desc.chars().count() > 140 {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: "Usage: /marker [description up to 140 characters]".to_owned(),
                    });
                }
                Some(desc.to_owned())
            };

            Some(AppCommand::CreateStreamMarker {
                channel: channel.clone(),
                description,
            })
        }

        "announce" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Announcement commands are only supported for Twitch channels."
                        .to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Announcement commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage =
                "Usage: /announce <message> [--color primary|blue|green|orange|purple]";
            let (message, color) = extract_color_flag(rest);
            let message = message.trim();
            if message.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }
            if message.chars().count() > 500 {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Announcement message must be 500 characters or fewer.".to_owned(),
                });
            }

            Some(AppCommand::SendAnnouncement {
                channel: channel.clone(),
                message: message.to_owned(),
                color,
            })
        }

        "shoutout" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Shoutout commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Shoutout commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage = "Usage: /shoutout <channel>";
            let target = rest
                .split_whitespace()
                .next()
                .map(|raw| raw.trim_start_matches('@').trim_start_matches('#'))
                .unwrap_or("")
                .trim();
            if target.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }
            let target_login = target.to_ascii_lowercase();
            let is_valid_login = {
                let len = target_login.len();
                (3..=25).contains(&len)
                    && target_login
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            };
            if !is_valid_login {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            Some(AppCommand::SendShoutout {
                channel: channel.clone(),
                target_login,
            })
        }

        "unbanrequests" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Unban request commands are only supported for Twitch channels."
                        .to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Unban request commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            Some(AppCommand::FetchUnbanRequests {
                channel: channel.clone(),
            })
        }

        "resolveunban" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Unban request commands are only supported for Twitch channels."
                        .to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Unban request commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage = "Usage: /resolveunban <request_id> <approve|deny> [reason]";
            let mut parts = rest.split_whitespace();
            let request_id = parts.next().unwrap_or("").trim();
            let action = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            if request_id.is_empty() || action.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            let approve = match action.as_str() {
                "approve" | "approved" | "allow" => true,
                "deny" | "denied" | "reject" => false,
                _ => {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: usage.to_owned(),
                    });
                }
            };

            let resolution_text = parts.collect::<Vec<_>>().join(" ");
            Some(AppCommand::ResolveUnbanRequest {
                channel: channel.clone(),
                request_id: request_id.to_owned(),
                approve,
                resolution_text: if resolution_text.trim().is_empty() {
                    None
                } else {
                    Some(resolution_text)
                },
            })
        }

        "automod" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "AutoMod commands are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "AutoMod commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }

            let usage = "Usage: /automod <allow|deny> <message_id> <sender_user_id>";
            let mut parts = rest.split_whitespace();
            let action_raw = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            let message_id = parts.next().unwrap_or("").trim();
            let sender_user_id = parts.next().unwrap_or("").trim();

            if action_raw.is_empty() || message_id.is_empty() || sender_user_id.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            let action = match action_raw.as_str() {
                "allow" | "approve" => "ALLOW",
                "deny" | "reject" => "DENY",
                _ => {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: usage.to_owned(),
                    });
                }
            };

            Some(AppCommand::ResolveAutoModMessage {
                channel: channel.clone(),
                message_id: message_id.to_owned(),
                sender_user_id: sender_user_id.to_owned(),
                action: action.to_owned(),
            })
        }

        "requests" => {
            let usage = "Usage: /requests [channel]";
            let target = if let Some(raw_target) = rest.split_whitespace().next() {
                parse_twitch_channel_login_arg(raw_target)
            } else if channel.is_twitch() {
                parse_twitch_channel_login_arg(channel.display_name())
            } else {
                None
            };

            let Some(target) = target else {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            };

            Some(AppCommand::OpenUrl {
                url: format!("https://www.twitch.tv/popout/{target}/reward-queue"),
            })
        }

        "modtools" | "lowtrust" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Moderation tools are only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Moderation tools require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            Some(AppCommand::OpenModerationTools {
                channel: Some(channel.clone()),
            })
        }

        "fakemsg" if !rest.is_empty() => {
            // Inject the raw text as a local system notice (no IRC parsing).
            Some(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text: rest.to_owned(),
            })
        }

        "openurl" if !rest.is_empty() => Some(AppCommand::OpenUrl {
            url: rest.to_owned(),
        }),

        // IRC-only: set nickname used by generic IRC servers.
        "nick" if channel.is_irc() => {
            if rest.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /nick <nickname>".to_owned(),
                })
            } else {
                Some(AppCommand::SetIrcNick {
                    nick: rest.to_owned(),
                })
            }
        }

        // Connect to an IRC server tab: /server <host[:port]> or /connect <host[:port]>
        "server" | "connect" => {
            if rest.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /server <host[:port]>".to_owned(),
                })
            } else if !irc_beta_enabled {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "IRC compatibility is disabled in Settings (beta).".to_owned(),
                })
            } else if let Some(server_tab) = parse_irc_server_arg(rest) {
                Some(AppCommand::JoinChannel {
                    channel: server_tab,
                })
            } else {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Invalid IRC server. Try /server irc.libera.chat:6697".to_owned(),
                })
            }
        }

        // IRC-only: join/create another channel on the same IRC server.
        "join" if channel.is_irc() => {
            if !irc_beta_enabled {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "IRC compatibility is disabled in Settings (beta).".to_owned(),
                });
            }
            if rest.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /join <#channel> [key]".to_owned(),
                })
            } else if let Some((target, key)) = parse_irc_join_args(channel, rest) {
                if key.is_some() {
                    Some(AppCommand::JoinIrcChannel {
                        channel: target,
                        key,
                    })
                } else {
                    Some(AppCommand::JoinChannel { channel: target })
                }
            } else {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Invalid channel. Try /join #channel [key]".to_owned(),
                })
            }
        }

        // IRC-only: leave a channel (current channel if omitted).
        "part" if channel.is_irc() => {
            if !irc_beta_enabled {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "IRC compatibility is disabled in Settings (beta).".to_owned(),
                });
            }
            if rest.is_empty() {
                Some(AppCommand::LeaveChannel {
                    channel: channel.clone(),
                })
            } else if let Some(target) = parse_irc_channel_arg(channel, rest) {
                Some(AppCommand::LeaveChannel { channel: target })
            } else {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Invalid channel. Try /part #channel".to_owned(),
                })
            }
        }

        // /popout [channel]  - opens popout chat in the browser.
        "popout" => {
            let target = if rest.is_empty() {
                channel.display_name()
            } else {
                rest
            };
            let url = if channel.is_kick() {
                if !kick_beta_enabled {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: "Kick compatibility is disabled in Settings (beta).".to_owned(),
                    });
                }
                format!("https://kick.com/{target}/chatroom")
            } else if channel.is_irc() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Popout is only available for Twitch/Kick channels.".to_owned(),
                });
            } else {
                format!("https://www.twitch.tv/popout/{target}/chat?popout=")
            };
            Some(AppCommand::OpenUrl { url })
        }

        // /user has two meanings:
        // - IRC: /user <username> [realname] registration command (forwarded).
        // - Twitch/Kick: open user profile in browser.
        "user" => {
            if channel.is_irc() {
                return Some(AppCommand::SendMessage {
                    channel: channel.clone(),
                    text: text.to_owned(),
                    reply_to_msg_id,
                    reply: reply.clone(),
                });
            }
            let login = rest
                .split_whitespace()
                .next()
                .unwrap_or(channel.display_name());
            let url = if channel.is_kick() {
                if !kick_beta_enabled {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: "Kick compatibility is disabled in Settings (beta).".to_owned(),
                    });
                }
                format!("https://kick.com/{login}")
            } else if channel.is_irc() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "User profile links are only available for Twitch/Kick channels."
                        .to_owned(),
                });
            } else {
                format!("https://twitch.tv/{login}")
            };
            Some(AppCommand::OpenUrl { url })
        }

        // /usercard <user> [channel]  - show our profile popup.
        "usercard" => {
            let login = rest.split_whitespace().next().unwrap_or("");
            if login.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /usercard <user>".to_owned(),
                })
            } else {
                Some(AppCommand::ShowUserCard {
                    login: login.to_owned(),
                    channel: channel.clone(),
                })
            }
        }

        // /streamlink [channel]  - open stream in streamlink via URL scheme.
        "streamlink" => {
            let target = if rest.is_empty() {
                channel.as_str()
            } else {
                rest
            };
            if channel.is_irc() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Streamlink only supports Twitch channels.".to_owned(),
                });
            }
            // Try the streamlink:// URI scheme; if unregistered the OS ignores it gracefully.
            let url = format!("streamlink://twitch.tv/{target}");
            Some(AppCommand::OpenUrl { url })
        }

        // Mod-only shorthand helpers (validated client-side)
        // NOTE: the actual enforcement is server-side; we just show a
        // usage hint so non-mods don't waste a round-trip.
        "banid" if !rest.is_empty() => {
            // /banid <userID>  →  forward as /ban to IRC (uses ID not name).
            let fwd = format!("/ban {rest}");
            Some(AppCommand::SendMessage {
                channel: channel.clone(),
                text: fwd,
                reply_to_msg_id,
                reply: reply.clone(),
            })
        }

        "untimeout" => {
            let target = rest.trim();
            if target.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /untimeout <user>".to_owned(),
                })
            } else {
                let fwd = format!("/unban {target}");
                Some(AppCommand::SendMessage {
                    channel: channel.clone(),
                    text: fwd,
                    reply_to_msg_id,
                    reply: reply.clone(),
                })
            }
        }

        // /w <user> <message>  - Twitch whisper (pass straight through).
        "w" | "whisper" => Some(AppCommand::SendMessage {
            channel: channel.clone(),
            text: text.to_owned(),
            reply_to_msg_id,
            reply: reply.clone(),
        }),

        // Everything else falls through to IRC
        // Standard Twitch chat commands (/ban, /timeout, /unban, /slow,
        // /subscribers, /emoteonly, /clear, /mod, /vip, /color, /delete,
        // /raid, /host, /uniquechat, /block, /unblock,
        // /r, /w, etc.) are handled server-side.
        _ => None,
    }
}

fn is_anonymous_local_command(cmd: &AppCommand) -> bool {
    matches!(
        cmd,
        AppCommand::InjectLocalMessage { .. }
            | AppCommand::ClearLocalMessages { .. }
            | AppCommand::OpenUrl { .. }
            | AppCommand::ShowUserCard { .. }
    )
}

fn sorted_chatters_vec(chatters: &std::collections::HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = chatters.iter().cloned().collect();
    // Cache lowercased keys once per rebuild instead of per-compare allocation.
    out.sort_by_cached_key(|name| name.to_ascii_lowercase());
    out
}

fn estimate_chatter_count(ch: &ChannelState) -> usize {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in &ch.messages {
        if msg.flags.is_deleted
            || matches!(
                msg.msg_kind,
                MsgKind::SystemInfo
                    | MsgKind::ChatCleared
                    | MsgKind::Timeout { .. }
                    | MsgKind::Ban { .. }
            )
        {
            continue;
        }
        let login = msg.sender.login.trim();
        if !login.is_empty() {
            seen.insert(login.to_ascii_lowercase());
        }
    }
    seen.len()
}

fn parse_irc_channel_arg(current: &ChannelId, raw: &str) -> Option<ChannelId> {
    let arg = raw.split_whitespace().next()?.trim();
    if arg.is_empty() {
        return None;
    }
    if arg.starts_with("irc://") || arg.starts_with("ircs://") {
        return ChannelId::parse_user_input(arg);
    }
    let t = current.irc_target()?;
    // Strip exactly one leading '#' for internal storage.
    let ch = arg.strip_prefix('#').unwrap_or(arg);
    Some(ChannelId::irc(t.host, t.port, t.tls, ch))
}

fn parse_pipe_args(spec: &str) -> Vec<String> {
    spec.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

struct ParsedPollSpec {
    title: String,
    choices: Vec<String>,
    duration_secs: u32,
    channel_points_per_vote: Option<u32>,
}

fn split_quoted_args(input: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        if in_quotes && ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }

        if ch.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
            continue;
        }

        current.push(ch);
    }

    if !current.is_empty() {
        args.push(current);
    }

    args
}

fn parse_poll_duration_token(raw: &str) -> Option<u32> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }

    if let Some(minutes) = value.strip_suffix('m') {
        let mins = minutes.parse::<u32>().ok()?;
        return mins.checked_mul(60);
    }

    if let Some(seconds) = value.strip_suffix('s') {
        return seconds.parse::<u32>().ok();
    }

    value.parse::<u32>().ok()
}

fn parse_poll_flag_args(input: &str) -> Option<ParsedPollSpec> {
    let tokens = split_quoted_args(input);
    if tokens.is_empty() {
        return None;
    }

    let mut title: Option<String> = None;
    let mut choices: Vec<String> = Vec::new();
    let mut duration_secs: Option<u32> = None;
    let mut channel_points_per_vote: Option<u32> = None;

    let mut i = 0usize;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "--title" => {
                let value = tokens.get(i + 1)?.trim();
                if value.is_empty() {
                    return None;
                }
                title = Some(value.to_owned());
                i += 2;
            }
            "--choice" => {
                let value = tokens.get(i + 1)?.trim();
                if value.is_empty() {
                    return None;
                }
                choices.push(value.to_owned());
                i += 2;
            }
            "--duration" => {
                let value = tokens.get(i + 1)?;
                duration_secs = parse_poll_duration_token(value);
                if duration_secs.is_none() {
                    return None;
                }
                i += 2;
            }
            "--points" => {
                let value = tokens.get(i + 1)?;
                let parsed = value.parse::<u32>().ok().filter(|v| *v > 0)?;
                channel_points_per_vote = Some(parsed);
                i += 2;
            }
            _ => return None,
        }
    }

    let title = title?.trim().to_owned();
    if title.is_empty() {
        return None;
    }
    if choices.len() < 2 {
        return None;
    }

    Some(ParsedPollSpec {
        title,
        choices,
        duration_secs: duration_secs.unwrap_or(60).clamp(15, 1800),
        channel_points_per_vote,
    })
}

fn extract_duration_flag(input: &str) -> (String, Option<u32>) {
    let mut cleaned_tokens: Vec<&str> = Vec::new();
    let mut duration: Option<u32> = None;

    let tokens: Vec<&str> = input.split_whitespace().collect();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i] == "--duration" {
            if let Some(next) = tokens.get(i + 1) {
                if let Ok(v) = next.parse::<u32>() {
                    duration = Some(v);
                    i += 2;
                    continue;
                }
            }
        }
        cleaned_tokens.push(tokens[i]);
        i += 1;
    }

    (cleaned_tokens.join(" "), duration)
}

fn extract_points_flag(input: &str) -> (String, Option<u32>) {
    let mut cleaned_tokens: Vec<&str> = Vec::new();
    let mut points: Option<u32> = None;

    let tokens: Vec<&str> = input.split_whitespace().collect();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i] == "--points" {
            if let Some(next) = tokens.get(i + 1) {
                if let Ok(v) = next.parse::<u32>() {
                    points = Some(v.max(1));
                    i += 2;
                    continue;
                }
            }
        }
        cleaned_tokens.push(tokens[i]);
        i += 1;
    }

    (cleaned_tokens.join(" "), points)
}

fn parse_twitch_channel_login_arg(raw: &str) -> Option<String> {
    let mut login = raw.trim();
    if login.is_empty() {
        return None;
    }

    if let Some(stripped) = login.strip_prefix("https://www.twitch.tv/") {
        login = stripped;
    } else if let Some(stripped) = login.strip_prefix("http://www.twitch.tv/") {
        login = stripped;
    } else if let Some(stripped) = login.strip_prefix("https://twitch.tv/") {
        login = stripped;
    } else if let Some(stripped) = login.strip_prefix("http://twitch.tv/") {
        login = stripped;
    }

    let login = login
        .trim_start_matches('#')
        .trim_start_matches('@')
        .split('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if is_valid_twitch_login(&login) {
        Some(login)
    } else {
        None
    }
}

fn extract_color_flag(input: &str) -> (String, Option<String>) {
    let mut cleaned_tokens: Vec<&str> = Vec::new();
    let mut color: Option<String> = None;

    let tokens: Vec<&str> = input.split_whitespace().collect();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i] == "--color" {
            if let Some(next) = tokens.get(i + 1) {
                let candidate = next.trim().to_ascii_lowercase();
                if matches!(candidate.as_str(), "primary" | "blue" | "green" | "orange" | "purple") {
                    color = Some(candidate);
                    i += 2;
                    continue;
                }
            }
        }
        cleaned_tokens.push(tokens[i]);
        i += 1;
    }

    (cleaned_tokens.join(" "), color)
}

fn parse_irc_join_args(current: &ChannelId, raw: &str) -> Option<(ChannelId, Option<String>)> {
    let mut parts = raw.split_whitespace();
    let channel_arg = parts.next()?.trim();
    if channel_arg.is_empty() {
        return None;
    }
    let key = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let target = if channel_arg.starts_with("irc://") || channel_arg.starts_with("ircs://") {
        ChannelId::parse_user_input(channel_arg)?
    } else {
        let t = current.irc_target()?;
        // Strip exactly one leading '#' for internal storage.
        let ch = channel_arg.strip_prefix('#').unwrap_or(channel_arg);
        ChannelId::irc(t.host, t.port, t.tls, ch)
    };
    Some((target, key))
}

fn parse_irc_server_arg(raw: &str) -> Option<ChannelId> {
    let first = raw.split_whitespace().next()?.trim();
    if first.is_empty() {
        return None;
    }

    if first.starts_with("irc://") || first.starts_with("ircs://") || first.starts_with("irc:") {
        let parsed = ChannelId::parse_user_input(first)?;
        let t = parsed.irc_target()?;
        return Some(ChannelId::irc(
            t.host,
            t.port,
            t.tls,
            IRC_SERVER_CONTROL_CHANNEL,
        ));
    }

    let (host, port, tls) = if let Some((h, p)) = first.rsplit_once(':') {
        if let Ok(parsed) = p.parse::<u16>() {
            (h.trim(), parsed, parsed == 6697)
        } else {
            (first, 6697, true)
        }
    } else {
        (first, 6697, true)
    };

    if host.trim().is_empty() {
        return None;
    }
    Some(ChannelId::irc(host, port, tls, IRC_SERVER_CONTROL_CHANNEL))
}

/// Build an emote code → catalog entry lookup with provider priority
/// 7TV > BTTV > FFZ > Kick (same order as the backend `resolve_emote`).
fn build_emote_lookup(catalog: &[EmoteCatalogEntry]) -> HashMap<&str, &EmoteCatalogEntry> {
    fn priority(provider: &str) -> u8 {
        match provider {
            "7tv" => 4,
            "bttv" => 3,
            "ffz" => 2,
            "kick" => 1,
            _ => 0,
        }
    }
    let mut map: HashMap<&str, &EmoteCatalogEntry> = HashMap::with_capacity(catalog.len());
    // Insert lowest-priority first so higher-priority overwrites.
    let mut sorted: Vec<&EmoteCatalogEntry> = catalog.iter().collect();
    sorted.sort_by_key(|e| priority(&e.provider));
    for e in sorted {
        map.insert(&e.code, e);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::{
        parse_slash_command, responsive_layout, top_tab_metrics, toolbar_visibility,
        TabVisualStyle,
    };
    use crust_core::events::AppCommand;
    use crust_core::model::ChannelId;
    use crate::theme as t;

    #[test]
    fn responsive_layout_prefers_top_tabs_on_narrow_windows() {
        let layout = responsive_layout(460.0);
        assert!(layout.force_top_tabs);
        assert_eq!(layout.min_central_width, 120.0);
        assert_eq!(layout.sidebar_min_width, t::SIDEBAR_COMPACT_MIN_W);
        assert_eq!(layout.status_bar_height, 36.0);
    }

    #[test]
    fn responsive_layout_compacts_further_on_very_narrow_windows() {
        let layout = responsive_layout(280.0);
        assert!(layout.force_top_tabs);
        assert_eq!(layout.status_bar_height, 30.0);
        assert!(layout.analytics_default_width < 220.0);
    }

    #[test]
    fn compact_tab_metrics_keep_the_strip_dense_on_narrow_windows() {
        let metrics = top_tab_metrics(520.0, TabVisualStyle::Compact);
        assert!(metrics.strip_height <= 30.0);
        assert!(metrics.chip_height < 22.0);
        assert!(metrics.label_width <= 92.0);
    }

    #[test]
    fn normal_tabs_stay_larger_than_compact_tabs() {
        let compact = top_tab_metrics(980.0, TabVisualStyle::Compact);
        let normal = top_tab_metrics(980.0, TabVisualStyle::Normal);
        assert!(normal.strip_height > compact.strip_height);
        assert!(normal.chip_height > compact.chip_height);
        assert!(normal.label_width > compact.label_width);
    }

    #[test]
    fn toolbar_hides_irc_controls_when_irc_beta_is_disabled() {
        let hidden = toolbar_visibility(900.0, false);
        assert!(!hidden.show_irc_toggle);
        assert!(!hidden.show_irc_in_overflow);

        let shown = toolbar_visibility(900.0, true);
        assert!(shown.show_irc_toggle);
    }

    #[test]
    fn toolbar_keeps_regular_icon_size_until_space_is_tight() {
        let visibility = toolbar_visibility(520.0, true);
        assert!(visibility.show_join_button);
        assert!(visibility.show_perf_toggle);
        assert!(visibility.show_stats_toggle);
        assert!(!visibility.compact_controls);
    }

    #[test]
    fn toolbar_hides_diagnostics_before_hiding_core_actions() {
        let visibility = toolbar_visibility(350.0, true);
        assert!(visibility.show_join_button);
        assert!(!visibility.show_perf_toggle);
        assert!(!visibility.show_stats_toggle);
        assert!(!visibility.show_irc_toggle);
        assert!(visibility.show_perf_in_overflow);
        assert!(visibility.show_stats_in_overflow);
        assert!(visibility.show_irc_in_overflow);
        assert!(visibility.show_overflow_menu);
        assert!(!visibility.compact_controls);
    }

    #[test]
    fn slash_commercial_rejects_invalid_duration_argument() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/commercial 45",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(text, "Usage: /commercial [30|60|90|120|150|180]");
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_commercial_accepts_supported_duration_argument() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/commercial 90",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::StartCommercial { length_secs, .. }) => {
                assert_eq!(length_secs, 90);
            }
            other => panic!("expected StartCommercial, got {other:?}"),
        }
    }

    #[test]
    fn slash_marker_rejects_description_over_140_chars() {
        let channel = ChannelId::new("somechannel");
        let long = format!("/marker {}", "x".repeat(141));
        let parsed = parse_slash_command(&long, &channel, None, None, true, 0, true, true);

        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(text, "Usage: /marker [description up to 140 characters]");
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_marker_accepts_trimmed_description() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/marker   clutch moment   ",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::CreateStreamMarker { description, .. }) => {
                assert_eq!(description.as_deref(), Some("clutch moment"));
            }
            other => panic!("expected CreateStreamMarker, got {other:?}"),
        }
    }

    #[test]
    fn slash_poll_pipe_syntax_supports_points_flag() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/poll Best pet? | Cat | Dog --duration 90 --points 250",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::CreatePoll {
                title,
                choices,
                duration_secs,
                channel_points_per_vote,
                ..
            }) => {
                assert_eq!(title, "Best pet?");
                assert_eq!(choices, vec!["Cat".to_owned(), "Dog".to_owned()]);
                assert_eq!(duration_secs, 90);
                assert_eq!(channel_points_per_vote, Some(250));
            }
            other => panic!("expected CreatePoll, got {other:?}"),
        }
    }

    #[test]
    fn slash_poll_chatterino_flags_map_to_create_poll() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/poll --title \"Best fruit\" --choice \"Apple\" --choice \"Pear\" --duration 2m --points 100",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::CreatePoll {
                title,
                choices,
                duration_secs,
                channel_points_per_vote,
                ..
            }) => {
                assert_eq!(title, "Best fruit");
                assert_eq!(choices, vec!["Apple".to_owned(), "Pear".to_owned()]);
                assert_eq!(duration_secs, 120);
                assert_eq!(channel_points_per_vote, Some(100));
            }
            other => panic!("expected CreatePoll, got {other:?}"),
        }
    }

    #[test]
    fn slash_unbanrequests_maps_to_fetch_command() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/unbanrequests",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::FetchUnbanRequests { channel }) => {
                assert_eq!(channel.as_str(), "somechannel");
            }
            other => panic!("expected FetchUnbanRequests, got {other:?}"),
        }
    }

    #[test]
    fn slash_requests_defaults_to_current_channel_queue() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command("/requests", &channel, None, None, false, 0, true, true);

        match parsed {
            Some(AppCommand::OpenUrl { url }) => {
                assert_eq!(url, "https://www.twitch.tv/popout/somechannel/reward-queue");
            }
            other => panic!("expected OpenUrl for reward queue, got {other:?}"),
        }
    }

    #[test]
    fn slash_requests_accepts_channel_argument() {
        let channel = ChannelId::kick("somekickchannel");
        let parsed = parse_slash_command(
            "/requests #targetchannel",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::OpenUrl { url }) => {
                assert_eq!(url, "https://www.twitch.tv/popout/targetchannel/reward-queue");
            }
            other => panic!("expected OpenUrl for reward queue, got {other:?}"),
        }
    }

    #[test]
    fn slash_vote_maps_to_send_message() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/vote 2",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::SendMessage { text, .. }) => {
                assert_eq!(text, "/vote 2");
            }
            other => panic!("expected SendMessage for /vote, got {other:?}"),
        }
    }

    #[test]
    fn slash_redeem_maps_alias_to_redeem_command() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/reward Highlight my message",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::SendMessage { text, .. }) => {
                assert_eq!(text, "/redeem Highlight my message");
            }
            other => panic!("expected SendMessage for /redeem, got {other:?}"),
        }
    }

    #[test]
    fn slash_automod_maps_allow_action() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/automod allow msg-1 user-2",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::ResolveAutoModMessage {
                message_id,
                sender_user_id,
                action,
                ..
            }) => {
                assert_eq!(message_id, "msg-1");
                assert_eq!(sender_user_id, "user-2");
                assert_eq!(action, "ALLOW");
            }
            other => panic!("expected ResolveAutoModMessage, got {other:?}"),
        }
    }

    #[test]
    fn slash_modtools_maps_to_open_tools_command() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command("/modtools", &channel, None, None, true, 0, true, true);

        match parsed {
            Some(AppCommand::OpenModerationTools { channel }) => {
                assert_eq!(channel.as_ref().map(|c| c.as_str()), Some("somechannel"));
            }
            other => panic!("expected OpenModerationTools, got {other:?}"),
        }
    }

    #[test]
    fn slash_resolveunban_maps_with_reason() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/resolveunban req-42 deny appeal rejected",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::ResolveUnbanRequest {
                request_id,
                approve,
                resolution_text,
                ..
            }) => {
                assert_eq!(request_id, "req-42");
                assert!(!approve);
                assert_eq!(resolution_text.as_deref(), Some("appeal rejected"));
            }
            other => panic!("expected ResolveUnbanRequest, got {other:?}"),
        }
    }

    #[test]
    fn slash_untimeout_alias_maps_to_unban_forward() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/untimeout troublemaker",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
        );

        match parsed {
            Some(AppCommand::SendMessage { text, .. }) => {
                assert_eq!(text, "/unban troublemaker");
            }
            other => panic!("expected SendMessage forwarding to /unban, got {other:?}"),
        }
    }
}
