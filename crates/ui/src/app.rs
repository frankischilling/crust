use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use image::DynamicImage;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::warn;

use crust_core::{
    events::{
        AppCommand, AppEvent, AutoModQueueItem, ConnectionState, LinkPreview, UnbanRequestItem,
    },
    model::{
        ChannelId, ChannelState, ChatMessage, EmoteCatalogEntry, MessageId, MsgKind, ReplyInfo,
        Span, TwitchEmotePos, IRC_SERVER_CONTROL_CHANNEL,
    },
    plugin_command_infos, plugin_host,
    plugins::PluginUiHostSlot,
    state::TabVisibilityRule,
    AppState, PluginAuthSnapshot, PluginChannelSnapshot,
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
    crash_viewer::{CrashReportMeta, CrashViewer},
    emote_picker::EmotePicker,
    emote_picker::EmotePickerPreferences,
    global_search::{
        refresh_if_stale, show_global_search_window, GlobalSearchOutput, GlobalSearchState,
    },
    hype_train_banner::{show_hype_train_banner, show_raid_banner},
    shared_chat_banner::show_shared_chat_banner,
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
    plugin_ui::{
        has_host_panels_for_slot, render_host_panels_for_slot, show_plugin_windows,
        PluginUiSessionState,
    },
    settings_page::{
        parse_settings_lines, show_settings_page, SettingsPageState, SettingsSection, SettingsStats,
    },
    split_header::{show_split_header, split_header_height},
    user_profile_popup::{PopupAction, UserProfilePopup},
};

// Channel layout mode

const REPAINT_ANIM_MS: u64 = 33;
const REPAINT_HOUSEKEEPING_MS: u64 = 2_000;
const STREAM_REFRESH_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const STREAM_REFRESH_ACTIVE_LIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(8);
const STREAM_REFRESH_ACTIVE_OFFLINE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
const STREAM_REFRESH_ACTIVE_UNKNOWN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(15);
const STREAM_REFRESH_BACKGROUND_LIVE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(45);
const STREAM_REFRESH_BACKGROUND_OFFLINE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(120);
const STREAM_REFRESH_BACKGROUND_UNKNOWN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);
const STREAM_REFRESH_INFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
const STREAM_NOTIFICATION_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(8);
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
const REGULAR_STATUS_BAR_HEIGHT: f32 = 40.0;
const NARROW_STATUS_BAR_HEIGHT: f32 = 40.0;
const VERY_NARROW_STATUS_BAR_HEIGHT: f32 = 40.0;
const ANALYTICS_DEFAULT_W: f32 = 220.0;
const ANALYTICS_MIN_W: f32 = 180.0;
const ANALYTICS_MAX_W: f32 = 340.0;
const ANALYTICS_COMPACT_DEFAULT_W: f32 = 176.0;
const ANALYTICS_COMPACT_MIN_W: f32 = 140.0;
const ANALYTICS_COMPACT_MAX_W: f32 = 260.0;
const LOCAL_HISTORY_SEARCH_PAGE: usize = 800;
const MAX_WHISPERS_PER_THREAD: usize = 250;
const WHISPER_EMOTE_SIZE: f32 = 20.0;
const QUICK_SWITCH_MAX_ROWS: usize = 10;

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
    let scale = t::font_scale();

    ResponsiveLayout {
        force_top_tabs: narrow,
        status_bar_height: (if very_narrow {
            VERY_NARROW_STATUS_BAR_HEIGHT
        } else if narrow {
            NARROW_STATUS_BAR_HEIGHT
        } else {
            REGULAR_STATUS_BAR_HEIGHT
        }) * scale,
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

#[derive(Clone)]
struct WhisperLine {
    from_login: String,
    from_display_name: String,
    text: String,
    twitch_emotes: Vec<TwitchEmotePos>,
    spans: Vec<Span>,
    timestamp: DateTime<Utc>,
    is_self: bool,
}

// Split-pane state

/// One pane in the split view.
#[derive(Clone)]
struct Pane {
    channel: ChannelId,
    input_buf: String,
    /// Width fraction (0.0-1.0) of available space; all panes sum to ~1.0.
    frac: f32,
}

/// One queued moderation action against a low-trust entry, dispatched after
/// the mod-tools window closes its render closure (separates UI gathering
/// from `&mut self` command dispatch).
enum LowTrustAction {
    Set {
        login: String,
        user_id: String,
        restricted: bool,
    },
    Clear {
        login: String,
        user_id: String,
    },
}

/// Hotkey trigger drained once per frame from the mod-tools window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModHotkey {
    None,
    ApproveFocused,
    DenyFocused,
    FocusNext,
    FocusPrev,
    BulkApprove,
    BulkDeny,
    NextTab,
    PrevTab,
}

/// Active tab inside the moderation tools window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModToolsTab {
    AutoMod,
    LowTrust,
    UnbanRequests,
}

impl ModToolsTab {
    /// Cycle to the next tab (wraps).
    fn next(self) -> Self {
        match self {
            Self::AutoMod => Self::LowTrust,
            Self::LowTrust => Self::UnbanRequests,
            Self::UnbanRequests => Self::AutoMod,
        }
    }
    /// Cycle to the previous tab (wraps).
    fn prev(self) -> Self {
        match self {
            Self::AutoMod => Self::UnbanRequests,
            Self::LowTrust => Self::AutoMod,
            Self::UnbanRequests => Self::LowTrust,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::AutoMod => "AutoMod",
            Self::LowTrust => "Low Trust",
            Self::UnbanRequests => "Unban Requests",
        }
    }
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

    fn move_focused(&mut self, delta: isize) {
        if self.panes.len() < 2 || self.focused >= self.panes.len() {
            return;
        }
        let new_idx = self.focused as isize + delta;
        if new_idx < 0 || new_idx >= self.panes.len() as isize {
            return;
        }
        let idx = self.focused;
        let other = new_idx as usize;
        self.panes.swap(idx, other);
        self.focused = other;
    }

    fn focus_first(&mut self) {
        if !self.panes.is_empty() {
            self.focused = 0;
        }
    }

    fn focus_last(&mut self) {
        if !self.panes.is_empty() {
            self.focused = self.panes.len() - 1;
        }
    }

    fn focus_next(&mut self) {
        if !self.panes.is_empty() {
            self.focused = (self.focused + 1) % self.panes.len();
        }
    }

    fn focus_prev(&mut self) {
        if !self.panes.is_empty() {
            self.focused = if self.focused == 0 {
                self.panes.len() - 1
            } else {
                self.focused - 1
            };
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
    let scale = t::font_scale();
    let tabs_font_h = t::tabs_font_size() + 6.0;
    match style {
        TabVisualStyle::Compact => TopTabMetrics {
            strip_height: ((if narrow { 28.0 } else { 30.0 }) * scale).max(tabs_font_h + 8.0),
            chip_height: ((if narrow { 18.0 } else { 20.0 }) * scale).max(tabs_font_h),
            label_width: (if window_width < 520.0 {
                84.0
            } else if window_width < 860.0 {
                92.0
            } else {
                112.0
            }) * scale,
            chip_pad_x: 6,
            chip_pad_y: 1,
            close_button_size: 12.0 * scale,
        },
        TabVisualStyle::Normal => TopTabMetrics {
            strip_height: ((if narrow { 34.0 } else { 36.0 }) * scale).max(tabs_font_h + 10.0),
            chip_height: ((if narrow { 22.0 } else { 24.0 }) * scale).max(tabs_font_h + 2.0),
            label_width: (if window_width < 720.0 {
                108.0
            } else if window_width < 1020.0 {
                132.0
            } else {
                156.0
            }) * scale,
            chip_pad_x: 8,
            chip_pad_y: 2,
            close_button_size: 14.0 * scale,
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
    show_whispers_toggle: bool,
    show_whispers_in_overflow: bool,
    show_irc_toggle: bool,
    show_irc_in_overflow: bool,
    show_mod_button: bool,
    show_mod_in_overflow: bool,
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
const TOOLBAR_MOD_BUTTON_W: f32 = 42.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolbarDegradeStep {
    HideEmoteCount,
    HideJoinText,
    HideConnectionLabel,
    HideLogo,
    HideWhispersToggle,
    HideModButton,
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
        t::icon_btn_sm()
    } else {
        t::icon_btn()
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

    if !visibility.compact_controls || visibility.show_emote_count || visibility.show_overflow_menu
    {
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
    if visibility.show_whispers_toggle {
        icon_count += 1;
    }
    if visibility.show_irc_toggle {
        icon_count += 1;
    }
    if visibility.show_mod_button {
        width += TOOLBAR_MOD_BUTTON_W + 4.0;
    }
    width + estimate_toolbar_group_width(icon_count, 4.0, visibility.compact_controls)
}

fn apply_toolbar_degrade_step(visibility: &mut ToolbarVisibility, step: ToolbarDegradeStep) {
    match step {
        ToolbarDegradeStep::HideEmoteCount => visibility.show_emote_count = false,
        ToolbarDegradeStep::HideJoinText => visibility.show_join_text = false,
        ToolbarDegradeStep::HideConnectionLabel => visibility.show_connection_label = false,
        ToolbarDegradeStep::HideLogo => visibility.show_logo = false,
        ToolbarDegradeStep::HideWhispersToggle => visibility.show_whispers_toggle = false,
        ToolbarDegradeStep::HideModButton => visibility.show_mod_button = false,
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

fn finalize_toolbar_visibility(
    visibility: &mut ToolbarVisibility,
    irc_beta_enabled: bool,
    moderation_available: bool,
) {
    if !irc_beta_enabled {
        visibility.show_irc_toggle = false;
        visibility.show_irc_in_overflow = false;
    } else {
        visibility.show_irc_in_overflow = !visibility.show_irc_toggle;
    }

    visibility.show_perf_in_overflow = !visibility.show_perf_toggle;
    visibility.show_stats_in_overflow = !visibility.show_stats_toggle;
    visibility.show_whispers_in_overflow = !visibility.show_whispers_toggle;
    if !moderation_available {
        visibility.show_mod_button = false;
        visibility.show_mod_in_overflow = false;
    } else {
        visibility.show_mod_in_overflow = !visibility.show_mod_button;
    }

    if !visibility.show_join_button {
        visibility.show_join_text = false;
        visibility.show_join_in_overflow = true;
    }

    visibility.show_overflow_menu = visibility.show_join_in_overflow
        || visibility.show_perf_in_overflow
        || visibility.show_stats_in_overflow
        || visibility.show_whispers_in_overflow
        || visibility.show_irc_in_overflow
        || visibility.show_mod_in_overflow;
}

fn toolbar_visibility(
    bar_width: f32,
    irc_beta_enabled: bool,
    moderation_available: bool,
) -> ToolbarVisibility {
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
        show_whispers_toggle: true,
        show_whispers_in_overflow: false,
        show_irc_toggle: irc_beta_enabled,
        show_irc_in_overflow: false,
        show_mod_button: moderation_available,
        show_mod_in_overflow: false,
        show_emote_count: true,
    };

    const STEPS: [ToolbarDegradeStep; 14] = [
        ToolbarDegradeStep::HideEmoteCount,
        ToolbarDegradeStep::HideJoinText,
        ToolbarDegradeStep::HideConnectionLabel,
        ToolbarDegradeStep::HideLogo,
        ToolbarDegradeStep::HideWhispersToggle,
        ToolbarDegradeStep::HideModButton,
        ToolbarDegradeStep::HideIrcToggle,
        ToolbarDegradeStep::HideStatsToggle,
        ToolbarDegradeStep::HidePerfToggle,
        ToolbarDegradeStep::CompactAccount,
        ToolbarDegradeStep::CompactControls,
        ToolbarDegradeStep::UltraCompactAccount,
        ToolbarDegradeStep::HideSidebarActions,
        // keep join hidden as the final fallback
        ToolbarDegradeStep::HideJoinButton,
    ];

    for step in STEPS {
        finalize_toolbar_visibility(&mut visibility, irc_beta_enabled, moderation_available);
        if estimate_toolbar_required_width(&visibility) <= bar_width {
            break;
        }
        apply_toolbar_degrade_step(&mut visibility, step);
    }

    finalize_toolbar_visibility(&mut visibility, irc_beta_enabled, moderation_available);

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
        visibility.show_whispers_toggle = false;
        visibility.show_irc_toggle = false;
        visibility.show_mod_button = false;
        if bar_width < 300.0 {
            visibility.show_join_button = false;
            visibility.show_join_in_overflow = true;
        }
        finalize_toolbar_visibility(&mut visibility, irc_beta_enabled, moderation_available);
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

#[derive(Default, Clone)]
struct ChannelQuickSwitch {
    open: bool,
    query: String,
    selected: usize,
    focus_query: bool,
}

#[derive(Clone)]
enum QuickSwitchEntry {
    Channel(ChannelId),
    WhisperThread { login: String },
}

#[derive(Clone)]
struct QuickSwitchCandidate {
    entry: QuickSwitchEntry,
    label: String,
    subtitle: Option<String>,
    unread_count: u32,
    unread_mentions: u32,
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
    quick_switch: ChannelQuickSwitch,
    emote_picker: EmotePicker,
    chat_input_buf: String,
    emote_catalog: Vec<EmoteCatalogEntry>,
    perf: PerfOverlay,
    /// Reply pending for the next send (set by right-click -> Reply).
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
    /// Whether the whisper management window is visible.
    whispers_visible: bool,
    /// Whisper threads keyed by partner login.
    whisper_threads: HashMap<String, VecDeque<WhisperLine>>,
    /// Preferred display name per whisper thread partner.
    whisper_display_names: HashMap<String, String>,
    /// Recency-ordered whisper thread keys.
    whisper_order: Vec<String>,
    /// Per-thread unread whisper counts.
    whisper_unread: HashMap<String, u32>,
    /// Per-thread unread whispers that mention the active account.
    whisper_unread_mentions: HashMap<String, u32>,
    /// Currently selected whisper thread login.
    active_whisper_login: Option<String>,
    /// Deduplicates whisper emote/emoji image fetches while loading.
    whisper_pending_images: HashSet<String>,
    /// Deduplicates live-feed thumbnail fetches.
    live_feed_pending_thumbnails: HashSet<String>,
    /// Startup loading overlay (shown until initial emotes + history are ready).
    loading_screen: LoadingScreen,
    /// Cached stream status per channel (key = channel login, lowercase).
    stream_statuses: HashMap<String, StreamStatusInfo>,
    /// When each channel's stream status was last fetched.
    stream_status_fetched: HashMap<String, std::time::Instant>,
    /// Channel logins currently being fetched for stream status.
    ///
    /// Value stores when the fetch started so stale in-flight entries can be
    /// retried if no event returns.
    stream_status_fetch_inflight: HashMap<String, std::time::Instant>,
    /// Last time we scanned channels to schedule stale stream-status refreshes.
    last_stream_refresh_scan: std::time::Instant,
    /// Last time we forced a refresh for the active Twitch channel.
    last_active_stream_refresh: std::time::Instant,
    /// Cached live-status map derived from `stream_statuses`; rebuilt only on
    /// change rather than every frame.
    live_map_cache: HashMap<String, bool>,
    /// Tracks watched channels and suppresses duplicate live/offline transitions.
    stream_tracker: StreamStatusTracker,
    /// Cross-platform audio ping player for highlight / whisper / sub / raid
    /// / custom-highlight events. See [`crate::sound::SoundController`].
    sound_controller: crate::sound::SoundController,
    /// Per-event sound configuration for the settings-page editor
    /// (mirrors `AppSettings.sounds`; normalised on every update).
    settings_sounds: crust_core::sound::SoundSettings,
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
    /// Host-managed plugin UI form/session state for retained plugin surfaces.
    plugin_ui_session: PluginUiSessionState,
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
    /// Chat body font size in points (drives theme body/small/heading/tiny).
    chat_font_size: f32,
    /// UI scale ratio fed to egui `pixels_per_point`.
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
    /// Inline chip / inline badge size (pt). 0.0 = auto.
    chips_font_size: f32,
    /// User-card heading size (pt). 0.0 = auto.
    usercard_font_size: f32,
    /// Login / dialog helper-text size (pt). 0.0 = auto.
    dialog_font_size: f32,
    /// Tracks the `pixels_per_point` we last pushed to the ctx so we don't spam it.
    applied_pixels_per_point: f32,
    /// Channel key persisted from the previous session. Activated once the
    /// channel finishes joining and appears in `state.channels`.
    pending_restore_channel: Option<String>,
    /// Last channel we persisted to disk (avoids spamming writes).
    last_saved_active_channel: Option<String>,
    /// Channel order from the previous session, applied once channels have joined.
    pending_restore_channel_order: Vec<String>,
    /// Split-pane layout from the previous session, applied once channels have joined.
    pending_restore_split_panes: Vec<(String, f32)>,
    /// Focused-pane index from the previous session.
    pending_restore_split_focused: usize,
    /// Last channel order we persisted to disk (avoids spamming writes).
    last_saved_channel_order: Vec<String>,
    /// Last split-pane snapshot we persisted to disk (avoids spamming writes).
    last_saved_split_panes: Vec<(String, f32)>,
    last_saved_split_focused: usize,
    /// Last window geometry snapshot we persisted (avoids spamming writes).
    last_saved_window_pos: Option<[f32; 2]>,
    last_saved_window_size: Option<[f32; 2]>,
    last_saved_window_max: bool,
    /// When the window geometry last changed; used to debounce saves.
    window_geom_dirty_since: Option<std::time::Instant>,
    /// Whispers panel state we last persisted (avoids spamming writes).
    last_saved_whispers_visible: bool,
    last_saved_whisper_login: String,
    /// Channels that just became active via user intent (tab click, keyboard
    /// nav, etc.). The MessageList widget drains this at render time and
    /// treats it as an explicit "snap to bottom" signal so the user never
    /// opens into the middle of a stale scroll offset.  Without this signal
    /// the re-entry detection based on egui's cumulative_pass_nr can miss
    /// edge cases (rapid click sequences, multi-pass frames, channels whose
    /// scroll state was frozen while the user was scrolled up), and the
    /// channel opens "black" until the user clicks Resume scrolling.
    pending_active_snap: std::collections::HashSet<ChannelId>,
    /// Snapshot of channels that were "visible" (single-pane active
    /// channel, or any split-pane channel) at the end of the previous
    /// update() call.  Used at the top of each frame to detect channel
    /// activations that bypassed `activate_channel` (keyboard shortcuts,
    /// pane focus changes, split-pane bookkeeping, etc.) by diffing
    /// against the current set and snapping any newly-visible channel.
    prev_frame_visible_channels: std::collections::HashSet<ChannelId>,
    /// Clone of the egui `Context` captured at the start of every
    /// `update()`.  Lets us write to ctx temp storage (and request
    /// repaints) from places that otherwise don't carry a ctx reference
    /// - in particular `activate_channel`, which needs to surface the
    /// force-snap flag inside the SAME frame that it runs (not the
    /// following frame) so MessageList sees it before it renders.
    egui_ctx: Option<egui::Context>,
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
    /// Persistent state for the shared filter-expression editor modal
    /// (reused across frames of the Settings dialog).
    settings_filter_editor_modal: crate::widgets::filter_editor::FilterEditorModal,

    /// User-defined moderation action presets
    mod_action_presets: Vec<crust_core::model::mod_actions::ModActionPreset>,
    settings_mod_action_presets: Vec<crust_core::model::mod_actions::ModActionPreset>,

    /// User -> display-name aliases.
    nicknames: Vec<crust_core::model::Nickname>,
    settings_nicknames: Vec<crust_core::model::Nickname>,

    /// Structured ignored-user list (compiled copy for fast per-message checks).
    ignored_users: Vec<crust_core::ignores::IgnoredUser>,
    settings_ignored_users: Vec<crust_core::ignores::IgnoredUser>,
    compiled_ignored_users: crust_core::ignores::CompiledIgnoredUsers,
    /// Ignored-phrase list (phrase actions are applied app-side; UI just owns the editor state).
    ignored_phrases: Vec<crust_core::ignores::IgnoredPhrase>,
    settings_ignored_phrases: Vec<crust_core::ignores::IgnoredPhrase>,

    /// User-defined command aliases (live expansion list).
    command_aliases: Vec<crust_core::commands::CommandAlias>,
    /// Settings-page draft copy (mirrors AppSettings.command_aliases).
    settings_command_aliases: Vec<crust_core::commands::CommandAlias>,

    /// Active hotkey bindings consulted by keyboard shortcut handlers.
    hotkey_bindings: crust_core::HotkeyBindings,
    /// Settings-page draft copy (mirrors AppSettings.hotkey_bindings, merged with defaults).
    settings_hotkey_bindings: crust_core::HotkeyBindings,
    /// Action currently awaiting a captured key press (settings page only).
    hotkey_capture_target: Option<crust_core::HotkeyAction>,

    /// Fetch + show alejo.io pronouns on the user profile popup.
    show_pronouns_in_usercard: bool,
    /// Auto-claim channel-points "Bonus Points" rewards.
    auto_claim_bonus_points: bool,
    /// Last-known login state from the embedded Twitch webview.
    twitch_webview_logged_in: Option<bool>,
    /// Latest channel-points balance per channel (Twitch-only). Keyed by
    /// `ChannelId` so anonymous viewers and Kick/IRC tabs simply have no entry.
    channel_points: std::collections::HashMap<crust_core::model::ChannelId, u64>,

    desktop_notifications_enabled: bool,
    /// Enable startup/background update checks.
    update_checks_enabled: bool,
    /// Last successful/attempted updater check timestamp.
    updater_last_checked_at: Option<String>,
    /// Version currently skipped by user choice.
    updater_skipped_version: String,
    /// Latest available version from updater checks.
    updater_available_version: Option<String>,
    /// Latest available release asset label.
    updater_available_asset: Option<String>,
    /// Latest available release URL.
    updater_available_release_url: Option<String>,
    /// True while runtime is downloading/staging update install artifacts.
    updater_install_inflight: bool,
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
    /// Global cross-channel search popup state.
    global_search: GlobalSearchState,
    /// Messages to scroll to after a global-search jump, keyed by channel.
    pending_scroll_to_message: HashMap<ChannelId, MessageId>,
    /// Sorted chatter names per channel, rebuilt only when membership changes.
    sorted_chatters: HashMap<ChannelId, Vec<String>>,
    /// Channels whose `chatters` membership changed since the last paint, so
    /// the sorted vec can be rebuilt lazily on the next frame instead of on
    /// every new-chatter ingest (the O(n log n) re-sort is a freeze hazard
    /// on busy channels with thousands of chatters).
    chatters_dirty: HashSet<ChannelId>,
    /// Last time we actually performed a chatter-list rebuild.  Used to
    /// throttle re-sorts to at most once every CHATTER_REBUILD_INTERVAL on
    /// a continuously-busy channel.
    chatters_last_rebuild: Option<std::time::Instant>,
    /// Last emote picker preferences acknowledged by runtime settings.
    emote_picker_prefs_last_saved: Option<EmotePickerPreferences>,
    /// Whether chat-input spellchecking is currently enabled. Mirrors
    /// `AppSettings::spellcheck_enabled`; updated via
    /// [`AppEvent::SpellDictionaryUpdated`].
    spellcheck_enabled: bool,
    /// Sorted snapshot of the user's custom spellcheck dictionary. The
    /// authoritative copy is persisted in settings - this is a UI-side
    /// mirror for the settings editor. Updated via
    /// [`AppEvent::SpellDictionaryUpdated`].
    spell_custom_dict: Vec<String>,
    /// Draft "add word" input buffer for the settings-page dictionary editor.
    spell_custom_dict_add_buf: String,
    /// Moderation tools dialog visibility.
    mod_tools_open: bool,
    /// Currently-active tab inside the moderation tools window.
    mod_tools_active_tab: ModToolsTab,
    /// Per-tab focused-row index for keyboard navigation.
    mod_tools_focused_index: usize,
    /// Per-tab text filter applied to the queue (matched against login + body).
    mod_tools_filter: String,
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
    /// Streamer mode user setting: `off`, `auto`, or `on`.
    streamer_mode_setting: String,
    /// Effective streamer-mode active flag (driven by setting + detection).
    streamer_mode_active: bool,
    /// Hide link preview tooltips while active.
    streamer_hide_link_previews: bool,
    /// Hide viewer counts in split headers while active.
    streamer_hide_viewer_counts: bool,
    /// Suppress sound notifications while active.
    streamer_suppress_sounds: bool,
    /// Settings dialog buffers (mirror of the live values, edited by the dialog).
    settings_streamer_mode: String,
    settings_streamer_hide_link_previews: bool,
    settings_streamer_hide_viewer_counts: bool,
    settings_streamer_suppress_sounds: bool,
    /// External tools (Streamlink + custom player)live values pushed from the worker.
    external_streamlink_path: String,
    external_streamlink_quality: String,
    external_streamlink_extra_args: String,
    external_player_template: String,
    external_mpv_path: String,
    external_streamlink_session_token: String,
    /// Modal surfaced on launch when the panic hook left a crash report
    /// behind in the previous session. Populated via
    /// [`CrustApp::set_pending_crash_reports`] before the first paint.
    crash_viewer: CrashViewer,
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
    vis.widgets.active.fg_stroke = egui::Stroke::new(1.0, t::text_on_accent());
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

/// Convert the stable key name stored in [`crust_core::KeyBinding`] to an
/// [`egui::Key`]. Case-insensitive; returns `None` for unknown keys (those
/// bindings are treated as inert until the user rebinds them).
pub fn egui_key_from_name(name: &str) -> Option<egui::Key> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    // Match the exact `egui::Key` variant names first (what
    // `egui_key_name` below emits). This is the hot path for bindings
    // persisted by our own settings round-trip.
    use egui::Key::*;
    Some(match n {
        "A" | "a" => A, "B" | "b" => B, "C" | "c" => C, "D" | "d" => D,
        "E" | "e" => E, "F" | "f" => F, "G" | "g" => G, "H" | "h" => H,
        "I" | "i" => I, "J" | "j" => J, "K" | "k" => K, "L" | "l" => L,
        "M" | "m" => M, "N" | "n" => N, "O" | "o" => O, "P" | "p" => P,
        "Q" | "q" => Q, "R" | "r" => R, "S" | "s" => S, "T" | "t" => T,
        "U" | "u" => U, "V" | "v" => V, "W" | "w" => W, "X" | "x" => X,
        "Y" | "y" => Y, "Z" | "z" => Z,
        "Num0" | "0" => Num0, "Num1" | "1" => Num1, "Num2" | "2" => Num2,
        "Num3" | "3" => Num3, "Num4" | "4" => Num4, "Num5" | "5" => Num5,
        "Num6" | "6" => Num6, "Num7" | "7" => Num7, "Num8" | "8" => Num8,
        "Num9" | "9" => Num9,
        "F1" => F1, "F2" => F2, "F3" => F3, "F4" => F4, "F5" => F5,
        "F6" => F6, "F7" => F7, "F8" => F8, "F9" => F9, "F10" => F10,
        "F11" => F11, "F12" => F12,
        "ArrowUp" | "Up" => ArrowUp,
        "ArrowDown" | "Down" => ArrowDown,
        "ArrowLeft" | "Left" => ArrowLeft,
        "ArrowRight" | "Right" => ArrowRight,
        "PageUp" => PageUp,
        "PageDown" => PageDown,
        "Home" => Home,
        "End" => End,
        "Tab" => Tab,
        "Enter" | "Return" => Enter,
        "Escape" | "Esc" => Escape,
        "Backspace" => Backspace,
        "Insert" | "Ins" => Insert,
        "Delete" | "Del" => Delete,
        "Space" => Space,
        "Minus" | "-" => Minus,
        "Plus" | "+" => Plus,
        "Equals" | "=" => Equals,
        "Comma" | "," => Comma,
        "Period" | "." => Period,
        "Semicolon" | ";" => Semicolon,
        "Slash" | "/" => Slash,
        "Backslash" | "\\" => Backslash,
        "OpenBracket" | "[" => OpenBracket,
        "CloseBracket" | "]" => CloseBracket,
        "Backtick" | "`" => Backtick,
        "Quote" | "'" => Quote,
        _ => return None,
    })
}

/// Stable reverse mapping used by the "capture hotkey" UI so we persist
/// exactly the variant names `egui_key_from_name` expects back.
pub fn egui_key_name(key: egui::Key) -> &'static str {
    use egui::Key::*;
    match key {
        A => "A", B => "B", C => "C", D => "D", E => "E", F => "F",
        G => "G", H => "H", I => "I", J => "J", K => "K", L => "L",
        M => "M", N => "N", O => "O", P => "P", Q => "Q", R => "R",
        S => "S", T => "T", U => "U", V => "V", W => "W", X => "X",
        Y => "Y", Z => "Z",
        Num0 => "Num0", Num1 => "Num1", Num2 => "Num2", Num3 => "Num3",
        Num4 => "Num4", Num5 => "Num5", Num6 => "Num6", Num7 => "Num7",
        Num8 => "Num8", Num9 => "Num9",
        F1 => "F1", F2 => "F2", F3 => "F3", F4 => "F4", F5 => "F5",
        F6 => "F6", F7 => "F7", F8 => "F8", F9 => "F9", F10 => "F10",
        F11 => "F11", F12 => "F12",
        ArrowUp => "ArrowUp", ArrowDown => "ArrowDown",
        ArrowLeft => "ArrowLeft", ArrowRight => "ArrowRight",
        PageUp => "PageUp", PageDown => "PageDown",
        Home => "Home", End => "End",
        Tab => "Tab", Enter => "Enter", Escape => "Escape",
        Backspace => "Backspace", Insert => "Insert", Delete => "Delete",
        Space => "Space",
        Minus => "Minus", Plus => "Plus", Equals => "Equals",
        Comma => "Comma", Period => "Period", Semicolon => "Semicolon",
        Slash => "Slash", Backslash => "Backslash",
        OpenBracket => "OpenBracket", CloseBracket => "CloseBracket",
        Backtick => "Backtick", Quote => "Quote",
        // Any other `egui::Key` variant we don't currently expose falls
        // back to a stable string so persistence round-trips; next load
        // `egui_key_from_name` will return None and the binding becomes
        // inert until the user rebinds it.
        _ => "",
    }
}

/// Compose an [`egui::Modifiers`] mask from a [`crust_core::KeyBinding`].
fn binding_modifiers(binding: &crust_core::KeyBinding) -> egui::Modifiers {
    let mut m = egui::Modifiers::NONE;
    if binding.ctrl {
        m |= egui::Modifiers::CTRL;
    }
    if binding.shift {
        m |= egui::Modifiers::SHIFT;
    }
    if binding.alt {
        m |= egui::Modifiers::ALT;
    }
    if binding.command {
        m |= egui::Modifiers::COMMAND;
    }
    m
}

/// Try to consume a hotkey press matching `binding` from the egui input
/// queue. Returns false for unbound or unknown-key bindings so callers can
/// short-circuit without extra checks.
fn consume_binding(input: &mut egui::InputState, binding: &crust_core::KeyBinding) -> bool {
    if binding.is_unbound() {
        return false;
    }
    let Some(key) = egui_key_from_name(&binding.key) else {
        return false;
    };
    input.consume_key(binding_modifiers(binding), key)
}

impl CrustApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd_tx: mpsc::Sender<AppCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // Visuals
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
            quick_switch: ChannelQuickSwitch::default(),
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
            whispers_visible: false,
            whisper_threads: HashMap::new(),
            whisper_display_names: HashMap::new(),
            whisper_order: Vec::new(),
            whisper_unread: HashMap::new(),
            whisper_unread_mentions: HashMap::new(),
            active_whisper_login: None,
            whisper_pending_images: HashSet::new(),
            live_feed_pending_thumbnails: HashSet::new(),
            loading_screen: LoadingScreen::default(),
            stream_statuses: HashMap::new(),
            stream_status_fetched: HashMap::new(),
            stream_status_fetch_inflight: HashMap::new(),
            last_stream_refresh_scan: std::time::Instant::now(),
            last_active_stream_refresh: std::time::Instant::now(),
            live_map_cache: HashMap::new(),
            stream_tracker: StreamStatusTracker::default(),
            sound_controller: crate::sound::SoundController::new(),
            settings_sounds: crust_core::sound::SoundSettings::with_defaults(),
            event_toasts: Vec::new(),
            event_toast_queue: VecDeque::new(),
            last_event_toast_emit: None,
            suppress_stream_toasts_until: std::time::Instant::now()
                + STREAM_NOTIFICATION_STARTUP_GRACE,
            settings_open: false,
            settings_section: SettingsSection::default(),
            plugin_ui_session: PluginUiSessionState::default(),
            kick_beta_enabled: false,
            irc_beta_enabled: false,
            irc_nickserv_user: String::new(),
            irc_nickserv_pass: String::new(),
            always_on_top: false,
            chat_font_size: t::chat_font_size(),
            ui_font_size: t::ui_font_size(),
            topbar_font_size: t::topbar_font_size_raw(),
            tabs_font_size: t::tabs_font_size_raw(),
            timestamps_font_size: t::timestamps_font_size_raw(),
            pills_font_size: t::pills_font_size_raw(),
            popups_font_size: t::popups_font_size_raw(),
            chips_font_size: t::chips_font_size_raw(),
            usercard_font_size: t::usercard_font_size_raw(),
            dialog_font_size: t::dialog_font_size_raw(),
            applied_pixels_per_point: 0.0,
            pending_restore_channel: None,
            last_saved_active_channel: None,
            pending_restore_channel_order: Vec::new(),
            pending_restore_split_panes: Vec::new(),
            pending_restore_split_focused: 0,
            last_saved_channel_order: Vec::new(),
            last_saved_split_panes: Vec::new(),
            last_saved_split_focused: 0,
            last_saved_window_pos: None,
            last_saved_window_size: None,
            last_saved_window_max: false,
            window_geom_dirty_since: None,
            last_saved_whispers_visible: false,
            last_saved_whisper_login: String::new(),
            pending_active_snap: std::collections::HashSet::new(),
            prev_frame_visible_channels: std::collections::HashSet::new(),
            egui_ctx: None,
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
            settings_filter_editor_modal: Default::default(),
            mod_action_presets: Vec::new(),
            settings_mod_action_presets: Vec::new(),
            nicknames: Vec::new(),
            settings_nicknames: Vec::new(),
            ignored_users: Vec::new(),
            settings_ignored_users: Vec::new(),
            compiled_ignored_users: crust_core::ignores::CompiledIgnoredUsers::new(&[]),
            ignored_phrases: Vec::new(),
            settings_ignored_phrases: Vec::new(),
            command_aliases: Vec::new(),
            settings_command_aliases: Vec::new(),
            hotkey_bindings: crust_core::HotkeyBindings::defaults(),
            settings_hotkey_bindings: crust_core::HotkeyBindings::defaults(),
            hotkey_capture_target: None,
            show_pronouns_in_usercard: false,
            auto_claim_bonus_points: false,
            twitch_webview_logged_in: None,
            channel_points: std::collections::HashMap::new(),
            desktop_notifications_enabled: false,
            update_checks_enabled: true,
            updater_last_checked_at: None,
            updater_skipped_version: String::new(),
            updater_available_version: None,
            updater_available_asset: None,
            updater_available_release_url: None,
            updater_install_inflight: false,
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
            global_search: GlobalSearchState::default(),
            pending_scroll_to_message: HashMap::new(),
            sorted_chatters: HashMap::new(),
            chatters_dirty: HashSet::new(),
            chatters_last_rebuild: None,
            emote_picker_prefs_last_saved: None,
            spellcheck_enabled: true,
            spell_custom_dict: Vec::new(),
            spell_custom_dict_add_buf: String::new(),
            mod_tools_open: false,
            mod_tools_active_tab: ModToolsTab::AutoMod,
            mod_tools_focused_index: 0,
            mod_tools_filter: String::new(),
            automod_queue: HashMap::new(),
            unban_requests: HashMap::new(),
            unban_resolution_drafts: HashMap::new(),
            auth_refresh_inflight: false,
            last_auth_refresh_attempt: None,
            streamer_mode_setting: "off".to_owned(),
            streamer_mode_active: false,
            streamer_hide_link_previews: true,
            streamer_hide_viewer_counts: true,
            streamer_suppress_sounds: true,
            settings_streamer_mode: "off".to_owned(),
            settings_streamer_hide_link_previews: true,
            settings_streamer_hide_viewer_counts: true,
            settings_streamer_suppress_sounds: true,
            external_streamlink_path: String::new(),
            external_streamlink_quality: "best".to_owned(),
            external_streamlink_extra_args: String::new(),
            external_player_template: "{streamlink} --player {mpv} twitch.tv/{channel} {quality}"
                .to_owned(),
            external_mpv_path: String::new(),
            external_streamlink_session_token: String::new(),
            crash_viewer: CrashViewer::default(),
        }
    }

    /// Install any crash reports that were recovered from the last
    /// session's panic hook. Safe to call with an empty `Vec` - the
    /// viewer widget only auto-opens when at least one report is
    /// present.
    pub fn set_pending_crash_reports(&mut self, reports: Vec<CrashReportMeta>) {
        self.crash_viewer.set_pending_reports(reports);
    }

    /// Register a cleanup closure that runs before the crash viewer's
    /// "Restart Crust" button calls `std::process::exit`. The app
    /// crate uses this to defuse its active session sentinel so the
    /// relaunched process does not treat the current run as an
    /// abnormal shutdown.
    pub fn set_crash_pre_exit_hook<F>(&mut self, f: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.crash_viewer.set_pre_exit_hook(f);
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

    /// Toggle sidebar visibility and persist the new value.  Use this from
    /// every toolbar/menu site that flips the flag - direct mutation of
    /// `self.sidebar_visible` skips the SetAppearanceSettings dispatch and
    /// the change is lost on next launch.
    fn toggle_sidebar_visible(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.send_appearance_settings();
    }

    fn toggle_analytics_visible(&mut self) {
        self.analytics_visible = !self.analytics_visible;
        self.send_appearance_settings();
    }

    fn toggle_irc_status_visible(&mut self) {
        self.irc_status_visible = !self.irc_status_visible;
        self.send_appearance_settings();
    }

    /// Switch between sidebar / top-tabs layouts and persist the new value.
    /// Mirrors the menu logic that auto-shows the sidebar when entering
    /// Sidebar mode.
    fn set_channel_layout(&mut self, layout: ChannelLayout) {
        if self.channel_layout == layout {
            return;
        }
        self.channel_layout = layout;
        if layout == ChannelLayout::Sidebar {
            self.sidebar_visible = true;
        }
        self.send_appearance_settings();
    }

    fn request_stream_status_refresh(&mut self, login: &str) {
        let login = login.trim().to_ascii_lowercase();
        if !is_valid_twitch_login(&login) {
            return;
        }
        if let Some(started_at) = self.stream_status_fetch_inflight.get(&login) {
            if started_at.elapsed() < STREAM_REFRESH_INFLIGHT_TIMEOUT {
                return;
            }
        }

        self.stream_status_fetch_inflight
            .insert(login.clone(), std::time::Instant::now());
        self.send_cmd(AppCommand::FetchStreamStatus { login });
    }

    fn stream_refresh_interval_for(
        &self,
        login: &str,
        is_active_channel: bool,
    ) -> std::time::Duration {
        let live_state = self.stream_statuses.get(login).map(|status| status.is_live);

        match (is_active_channel, live_state) {
            (true, Some(true)) => STREAM_REFRESH_ACTIVE_LIVE_INTERVAL,
            (true, Some(false)) => STREAM_REFRESH_ACTIVE_OFFLINE_INTERVAL,
            (true, None) => STREAM_REFRESH_ACTIVE_UNKNOWN_INTERVAL,
            (false, Some(true)) => STREAM_REFRESH_BACKGROUND_LIVE_INTERVAL,
            (false, Some(false)) => STREAM_REFRESH_BACKGROUND_OFFLINE_INTERVAL,
            (false, None) => STREAM_REFRESH_BACKGROUND_UNKNOWN_INTERVAL,
        }
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

    /// Recompute whether the audio backend should stay muted based on the
    /// current streamer-mode state. Called whenever either
    /// `streamer_mode_active` or `streamer_suppress_sounds` changes.
    fn sync_sound_suppression(&self) {
        self.sound_controller
            .set_suppressed(self.streamer_mode_active && self.streamer_suppress_sounds);
    }

    fn dispatch_desktop_notification(&self, title: &str, body: &str, with_sound: bool) {
        let with_sound =
            with_sound && !(self.streamer_mode_active && self.streamer_suppress_sounds);
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
                            last_error = Some(format!(
                                "{} (exit code {:?})",
                                shell.display(),
                                status.code()
                            ));
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

    fn trigger_test_gifted_sub_alert(&mut self, ctx: &Context) {
        self.push_event_toast(
            "🎉🎊  You received a gifted Tier 1 sub!".to_owned(),
            t::raid_cyan(),
            true,
        );

        self.sound_controller
            .play_event(crust_core::sound::SoundEvent::Subscribe);

        if self.desktop_notifications_enabled {
            self.request_user_attention(ctx, egui::UserAttentionType::Informational);
            self.dispatch_desktop_notification(
                "Crust Gifted Sub Test",
                "Test alert: You received a gifted Tier 1 sub!",
                true,
            );
        }
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
                self.push_event_toast(
                    format!("{} went offline", payload.channel_name),
                    t::text_muted(),
                    false,
                );
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

    /// Drain at most one moderation hotkey trigger per frame. Gated to the
    /// mod-tools window being open and no text-edit currently focused so
    /// single-letter binds (A/D/J/K) do not steal global keypresses.
    fn consume_mod_hotkey_input(&self, ctx: &Context) -> ModHotkey {
        use crust_core::HotkeyAction;
        if !self.mod_tools_open {
            return ModHotkey::None;
        }
        if ctx.memory(|m| m.focused().is_some()) {
            // A widget (filter input, unban draft) has focus -- let it consume keys.
            return ModHotkey::None;
        }
        let candidates = [
            (HotkeyAction::ModApproveFocused, ModHotkey::ApproveFocused),
            (HotkeyAction::ModDenyFocused, ModHotkey::DenyFocused),
            (HotkeyAction::ModFocusNext, ModHotkey::FocusNext),
            (HotkeyAction::ModFocusPrev, ModHotkey::FocusPrev),
            (HotkeyAction::ModBulkApprove, ModHotkey::BulkApprove),
            (HotkeyAction::ModBulkDeny, ModHotkey::BulkDeny),
            (HotkeyAction::ModNextTab, ModHotkey::NextTab),
            (HotkeyAction::ModPrevTab, ModHotkey::PrevTab),
        ];
        ctx.input_mut(|i| {
            for (action, kind) in candidates {
                let binding = self.hotkey_bindings.get(action);
                if consume_binding(i, &binding) {
                    return kind;
                }
            }
            ModHotkey::None
        })
    }

    /// Number of items in the active moderation tab for the given channel.
    fn mod_tab_len(&self, tab: ModToolsTab, channel: &ChannelId) -> usize {
        match tab {
            ModToolsTab::AutoMod => self
                .automod_queue
                .get(channel)
                .map(|q| q.len())
                .unwrap_or(0),
            ModToolsTab::LowTrust => self
                .state
                .channels
                .get(channel)
                .map(|c| c.low_trust_users.len())
                .unwrap_or(0),
            ModToolsTab::UnbanRequests => self
                .unban_requests
                .get(channel)
                .map(|q| q.len())
                .unwrap_or(0),
        }
    }

    /// Render the tabbed moderation tools window.
    fn render_mod_tools_window(
        &mut self,
        ctx: &Context,
        moderation_channel: Option<ChannelId>,
    ) {
        let mut window_open = self.mod_tools_open;
        let mut refresh_channel: Option<ChannelId> = None;
        let mut automod_actions: Vec<(String, String, String)> = Vec::new();
        let mut automod_bulk_action: Option<String> = None;
        let mut unban_actions: Vec<(String, bool, Option<String>)> = Vec::new();
        let mut unban_bulk_action: Option<bool> = None;
        let mut low_trust_actions: Vec<LowTrustAction> = Vec::new();

        let hotkey = self.consume_mod_hotkey_input(ctx);

        // Apply tab-switching / focus-nav hotkeys before rendering so the tab
        // strip and focus highlight reflect the new state this frame.
        if let Some(channel) = moderation_channel.as_ref() {
            match hotkey {
                ModHotkey::NextTab => {
                    self.mod_tools_active_tab = self.mod_tools_active_tab.next();
                    self.mod_tools_focused_index = 0;
                }
                ModHotkey::PrevTab => {
                    self.mod_tools_active_tab = self.mod_tools_active_tab.prev();
                    self.mod_tools_focused_index = 0;
                }
                ModHotkey::FocusNext => {
                    let len = self.mod_tab_len(self.mod_tools_active_tab, channel);
                    if len > 0 {
                        self.mod_tools_focused_index = (self.mod_tools_focused_index + 1) % len;
                    }
                }
                ModHotkey::FocusPrev => {
                    let len = self.mod_tab_len(self.mod_tools_active_tab, channel);
                    if len > 0 {
                        self.mod_tools_focused_index = if self.mod_tools_focused_index == 0 {
                            len - 1
                        } else {
                            self.mod_tools_focused_index - 1
                        };
                    }
                }
                _ => {}
            }
        }

        egui::Window::new("Moderation Tools")
            .open(&mut window_open)
            .default_size(egui::vec2(580.0, 540.0))
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
                });
                ui.label(
                    RichText::new(
                        "Hotkeys: J/K focus  A allow  D deny  Shift+A/D bulk  Tab/Shift+Tab switch tab",
                    )
                    .font(t::tiny())
                    .color(t::text_muted()),
                );

                // Tab strip with item-count badges.
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for tab in [
                        ModToolsTab::AutoMod,
                        ModToolsTab::LowTrust,
                        ModToolsTab::UnbanRequests,
                    ] {
                        let count = self.mod_tab_len(tab, &channel);
                        let label = if count == 0 {
                            tab.label().to_owned()
                        } else {
                            format!("{}  {}", tab.label(), count)
                        };
                        let active = self.mod_tools_active_tab == tab;
                        if ui
                            .selectable_label(active, RichText::new(label).font(t::small()))
                            .clicked()
                        {
                            self.mod_tools_active_tab = tab;
                            self.mod_tools_focused_index = 0;
                        }
                    }
                });

                // Filter strip.
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Filter:").font(t::small()));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.mod_tools_filter)
                            .hint_text("login or message text")
                            .desired_width(220.0),
                    );
                    if resp.changed() {
                        self.mod_tools_focused_index = 0;
                    }
                    if !self.mod_tools_filter.is_empty()
                        && ui.button(RichText::new("Clear").font(t::small())).clicked()
                    {
                        self.mod_tools_filter.clear();
                        self.mod_tools_focused_index = 0;
                    }
                });

                ui.separator();

                match self.mod_tools_active_tab {
                    ModToolsTab::AutoMod => {
                        Self::render_automod_tab(
                            ui,
                            &channel,
                            &self.automod_queue,
                            &self.mod_tools_filter,
                            self.mod_tools_focused_index,
                            &mut automod_actions,
                            &mut automod_bulk_action,
                        );
                    }
                    ModToolsTab::LowTrust => {
                        let entries: Vec<(String, crust_core::model::LowTrustEntry)> = self
                            .state
                            .channels
                            .get(&channel)
                            .map(|c| {
                                let mut v: Vec<(String, crust_core::model::LowTrustEntry)> = c
                                    .low_trust_users
                                    .iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect();
                                v.sort_by(|a, b| a.0.cmp(&b.0));
                                v
                            })
                            .unwrap_or_default();
                        Self::render_low_trust_tab(
                            ui,
                            &entries,
                            &self.mod_tools_filter,
                            self.mod_tools_focused_index,
                            &mut low_trust_actions,
                        );
                    }
                    ModToolsTab::UnbanRequests => {
                        ui.horizontal(|ui| {
                            if ui
                                .button(
                                    RichText::new("Refresh unban requests").font(t::small()),
                                )
                                .clicked()
                            {
                                refresh_channel = Some(channel.clone());
                            }
                        });
                        ui.add_space(4.0);
                        let requests = self
                            .unban_requests
                            .get(&channel)
                            .cloned()
                            .unwrap_or_default();
                        Self::render_unban_tab(
                            ui,
                            &channel,
                            &requests,
                            &self.mod_tools_filter,
                            self.mod_tools_focused_index,
                            &mut self.unban_resolution_drafts,
                            &mut unban_actions,
                            &mut unban_bulk_action,
                        );
                    }
                }
            });

        self.mod_tools_open = window_open;

        // Apply approve/deny/bulk hotkeys after the render loop sees the
        // current focus index for this frame.
        if let Some(channel) = moderation_channel.as_ref() {
            match hotkey {
                ModHotkey::ApproveFocused => self.queue_focused_mod_action(
                    channel,
                    true,
                    &mut automod_actions,
                    &mut unban_actions,
                    &mut low_trust_actions,
                ),
                ModHotkey::DenyFocused => self.queue_focused_mod_action(
                    channel,
                    false,
                    &mut automod_actions,
                    &mut unban_actions,
                    &mut low_trust_actions,
                ),
                ModHotkey::BulkApprove => match self.mod_tools_active_tab {
                    ModToolsTab::AutoMod => automod_bulk_action = Some("ALLOW".to_owned()),
                    ModToolsTab::UnbanRequests => unban_bulk_action = Some(true),
                    ModToolsTab::LowTrust => {}
                },
                ModHotkey::BulkDeny => match self.mod_tools_active_tab {
                    ModToolsTab::AutoMod => automod_bulk_action = Some("DENY".to_owned()),
                    ModToolsTab::UnbanRequests => unban_bulk_action = Some(false),
                    ModToolsTab::LowTrust => {}
                },
                _ => {}
            }
        }

        if let Some(channel) = refresh_channel {
            self.send_cmd(AppCommand::FetchUnbanRequests { channel });
        }
        if let Some(channel) = moderation_channel {
            if let Some(action) = automod_bulk_action {
                for item in self
                    .automod_queue
                    .get(&channel)
                    .cloned()
                    .unwrap_or_default()
                {
                    automod_actions.push((
                        item.message_id,
                        item.sender_user_id,
                        action.clone(),
                    ));
                }
            }
            if let Some(approve) = unban_bulk_action {
                for request in self
                    .unban_requests
                    .get(&channel)
                    .cloned()
                    .unwrap_or_default()
                {
                    let key = Self::unban_draft_key(&channel, &request.request_id);
                    let draft = self.unban_resolution_drafts.entry(key).or_default();
                    let resolution = draft.trim();
                    unban_actions.push((
                        request.request_id,
                        approve,
                        if resolution.is_empty() {
                            None
                        } else {
                            Some(resolution.to_owned())
                        },
                    ));
                }
            }
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
            for action in low_trust_actions {
                match action {
                    LowTrustAction::Set {
                        login,
                        user_id,
                        restricted,
                    } => {
                        self.send_cmd(AppCommand::SetSuspiciousUser {
                            channel: channel.clone(),
                            login,
                            user_id,
                            restricted,
                        });
                    }
                    LowTrustAction::Clear { login, user_id } => {
                        self.send_cmd(AppCommand::ClearSuspiciousUser {
                            channel: channel.clone(),
                            login,
                            user_id,
                        });
                    }
                }
            }
        }
    }

    /// Translate the "approve/deny focused entry" hotkey for the active tab
    /// into a queued action.
    fn queue_focused_mod_action(
        &mut self,
        channel: &ChannelId,
        approve: bool,
        automod_actions: &mut Vec<(String, String, String)>,
        unban_actions: &mut Vec<(String, bool, Option<String>)>,
        low_trust_actions: &mut Vec<LowTrustAction>,
    ) {
        let filter = self.mod_tools_filter.trim().to_ascii_lowercase();
        match self.mod_tools_active_tab {
            ModToolsTab::AutoMod => {
                let items = self
                    .automod_queue
                    .get(channel)
                    .cloned()
                    .unwrap_or_default();
                let filtered: Vec<&AutoModQueueItem> = items
                    .iter()
                    .filter(|i| Self::automod_matches_filter(i, &filter))
                    .collect();
                if let Some(item) = filtered.get(self.mod_tools_focused_index) {
                    automod_actions.push((
                        item.message_id.clone(),
                        item.sender_user_id.clone(),
                        if approve { "ALLOW".to_owned() } else { "DENY".to_owned() },
                    ));
                }
            }
            ModToolsTab::UnbanRequests => {
                let requests = self
                    .unban_requests
                    .get(channel)
                    .cloned()
                    .unwrap_or_default();
                let filtered: Vec<&UnbanRequestItem> = requests
                    .iter()
                    .filter(|r| Self::unban_matches_filter(r, &filter))
                    .collect();
                if let Some(req) = filtered.get(self.mod_tools_focused_index) {
                    let key = Self::unban_draft_key(channel, &req.request_id);
                    let resolution = self
                        .unban_resolution_drafts
                        .get(&key)
                        .map(|s| s.trim().to_owned())
                        .unwrap_or_default();
                    unban_actions.push((
                        req.request_id.clone(),
                        approve,
                        if resolution.is_empty() {
                            None
                        } else {
                            Some(resolution)
                        },
                    ));
                }
            }
            ModToolsTab::LowTrust => {
                // "Approve" = clear treatment; "Deny" = elevate to Restricted.
                let entries: Vec<(String, crust_core::model::LowTrustEntry)> = self
                    .state
                    .channels
                    .get(channel)
                    .map(|c| {
                        let mut v: Vec<(String, crust_core::model::LowTrustEntry)> = c
                            .low_trust_users
                            .iter()
                            .filter(|(k, e)| Self::low_trust_matches_filter(k, e, &filter))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        v.sort_by(|a, b| a.0.cmp(&b.0));
                        v
                    })
                    .unwrap_or_default();
                if let Some((login, entry)) = entries.get(self.mod_tools_focused_index) {
                    if entry.user_id.is_empty() {
                        return;
                    }
                    if approve {
                        low_trust_actions.push(LowTrustAction::Clear {
                            login: login.clone(),
                            user_id: entry.user_id.clone(),
                        });
                    } else {
                        low_trust_actions.push(LowTrustAction::Set {
                            login: login.clone(),
                            user_id: entry.user_id.clone(),
                            restricted: true,
                        });
                    }
                }
            }
        }
    }

    fn automod_matches_filter(item: &AutoModQueueItem, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        item.sender_login.to_ascii_lowercase().contains(filter)
            || item.text.to_ascii_lowercase().contains(filter)
    }

    fn unban_matches_filter(req: &UnbanRequestItem, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        req.user_login.to_ascii_lowercase().contains(filter)
            || req
                .text
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains(filter))
                .unwrap_or(false)
    }

    fn low_trust_matches_filter(
        login: &str,
        entry: &crust_core::model::LowTrustEntry,
        filter: &str,
    ) -> bool {
        if filter.is_empty() {
            return true;
        }
        login.contains(filter)
            || entry.display_name.to_ascii_lowercase().contains(filter)
    }

    fn render_automod_tab(
        ui: &mut egui::Ui,
        _channel: &ChannelId,
        queue: &HashMap<ChannelId, Vec<AutoModQueueItem>>,
        filter: &str,
        focused: usize,
        actions: &mut Vec<(String, String, String)>,
        bulk: &mut Option<String>,
    ) {
        let filter_lc = filter.trim().to_ascii_lowercase();
        let items: Vec<AutoModQueueItem> = queue
            .get(_channel)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| Self::automod_matches_filter(i, &filter_lc))
            .collect();
        if !items.is_empty() {
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("Allow all").font(t::small()).color(t::green()))
                    .clicked()
                {
                    *bulk = Some("ALLOW".to_owned());
                }
                if ui
                    .button(RichText::new("Deny all").font(t::small()).color(t::red()))
                    .clicked()
                {
                    *bulk = Some("DENY".to_owned());
                }
            });
            ui.add_space(4.0);
        }
        if items.is_empty() {
            ui.label(
                RichText::new(if filter.is_empty() {
                    "No held AutoMod messages."
                } else {
                    "No AutoMod entries match the filter."
                })
                .color(t::text_muted())
                .font(t::small()),
            );
            return;
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for (idx, item) in items.iter().enumerate() {
                    let mut frame = chrome::card_frame();
                    if idx == focused {
                        frame = frame.stroke(egui::Stroke::new(1.5, t::accent()));
                    }
                    frame.show(ui, |ui| {
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
                        if let Some(reason) = item
                            .reason
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                        {
                            ui.label(
                                RichText::new(format!(" AutoMod: {reason} "))
                                    .font(t::small())
                                    .color(t::text_on_accent())
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
                                actions.push((
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
                                actions.push((
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

    fn render_low_trust_tab(
        ui: &mut egui::Ui,
        entries: &[(String, crust_core::model::LowTrustEntry)],
        filter: &str,
        focused: usize,
        actions: &mut Vec<LowTrustAction>,
    ) {
        let filter_lc = filter.trim().to_ascii_lowercase();
        let filtered: Vec<&(String, crust_core::model::LowTrustEntry)> = entries
            .iter()
            .filter(|(login, entry)| Self::low_trust_matches_filter(login, entry, &filter_lc))
            .collect();
        if filtered.is_empty() {
            ui.label(
                RichText::new(if filter.is_empty() {
                    "No suspicious users currently tracked. Entries appear automatically when EventSub reports activity."
                } else {
                    "No low-trust entries match the filter."
                })
                .color(t::text_muted())
                .font(t::small()),
            );
            return;
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for (idx, (login, entry)) in filtered.iter().enumerate() {
                    let mut frame = chrome::card_frame();
                    if idx == focused {
                        frame = frame.stroke(egui::Stroke::new(1.5, t::accent()));
                    }
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("@{}", entry.display_name))
                                    .font(t::small())
                                    .strong(),
                            );
                            ui.separator();
                            let (badge_text, badge_color) = match entry.status {
                                crust_core::model::LowTrustStatus::Restricted => {
                                    ("RESTRICTED", t::red())
                                }
                                crust_core::model::LowTrustStatus::Monitored => {
                                    ("MONITORED", t::gold())
                                }
                            };
                            ui.label(
                                RichText::new(format!(" {badge_text} "))
                                    .font(t::tiny())
                                    .color(t::text_on_accent())
                                    .background_color(badge_color),
                            );
                            ui.label(
                                RichText::new(format!("login: {login}"))
                                    .font(t::tiny())
                                    .color(t::text_muted()),
                            );
                        });
                        ui.horizontal(|ui| {
                            let id_known = !entry.user_id.is_empty();
                            ui.add_enabled_ui(id_known, |ui| {
                                if ui
                                    .button(
                                        RichText::new("Monitor").font(t::small()),
                                    )
                                    .clicked()
                                {
                                    actions.push(LowTrustAction::Set {
                                        login: login.clone(),
                                        user_id: entry.user_id.clone(),
                                        restricted: false,
                                    });
                                }
                                if ui
                                    .button(
                                        RichText::new("Restrict")
                                            .font(t::small())
                                            .color(t::red()),
                                    )
                                    .clicked()
                                {
                                    actions.push(LowTrustAction::Set {
                                        login: login.clone(),
                                        user_id: entry.user_id.clone(),
                                        restricted: true,
                                    });
                                }
                                if ui
                                    .button(
                                        RichText::new("Clear")
                                            .font(t::small())
                                            .color(t::green()),
                                    )
                                    .clicked()
                                {
                                    actions.push(LowTrustAction::Clear {
                                        login: login.clone(),
                                        user_id: entry.user_id.clone(),
                                    });
                                }
                            });
                            if !id_known {
                                ui.label(
                                    RichText::new("(user-id unknown - reload after next event)")
                                        .font(t::tiny())
                                        .color(t::text_muted()),
                                );
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
            });
    }

    fn render_unban_tab(
        ui: &mut egui::Ui,
        channel: &ChannelId,
        requests: &[UnbanRequestItem],
        filter: &str,
        focused: usize,
        drafts: &mut HashMap<String, String>,
        actions: &mut Vec<(String, bool, Option<String>)>,
        bulk: &mut Option<bool>,
    ) {
        let filter_lc = filter.trim().to_ascii_lowercase();
        let filtered: Vec<&UnbanRequestItem> = requests
            .iter()
            .filter(|r| Self::unban_matches_filter(r, &filter_lc))
            .collect();
        if !filtered.is_empty() {
            ui.horizontal(|ui| {
                if ui
                    .button(
                        RichText::new("Approve all").font(t::small()).color(t::green()),
                    )
                    .clicked()
                {
                    *bulk = Some(true);
                }
                if ui
                    .button(RichText::new("Deny all").font(t::small()).color(t::red()))
                    .clicked()
                {
                    *bulk = Some(false);
                }
            });
            ui.add_space(4.0);
        }
        if filtered.is_empty() {
            ui.label(
                RichText::new(if filter.is_empty() {
                    "No pending unban requests loaded."
                } else {
                    "No unban requests match the filter."
                })
                .color(t::text_muted())
                .font(t::small()),
            );
            return;
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for (idx, request) in filtered.iter().enumerate() {
                    let key = Self::unban_draft_key(channel, &request.request_id);
                    let draft = drafts.entry(key).or_default();
                    let mut frame = chrome::card_frame();
                    if idx == focused {
                        frame = frame.stroke(egui::Stroke::new(1.5, t::accent()));
                    }
                    frame.show(ui, |ui| {
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
                        if let Some(text) = request
                            .text
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
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
                                actions.push((
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
                                actions.push((
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

    fn normalize_whisper_login(login: &str) -> Option<String> {
        let normalized = login
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('@')
            .to_ascii_lowercase();
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    fn tokenize_whisper_text(&self, text: &str, twitch_emotes: &[TwitchEmotePos]) -> Vec<Span> {
        let emote_map = build_emote_lookup(&self.emote_catalog);
        tokenize_whisper_text(text, twitch_emotes, &emote_map)
    }

    fn queue_whisper_span_images(&mut self, spans: &[Span]) {
        for span in spans {
            let url = match span {
                Span::Emote { url, .. } | Span::Emoji { url, .. } => Some(url.as_str()),
                _ => None,
            };
            let Some(url) = url else {
                continue;
            };
            if self.emote_bytes.contains_key(url) {
                continue;
            }
            if self.whisper_pending_images.insert(url.to_owned()) {
                self.send_cmd(AppCommand::FetchImage {
                    url: url.to_owned(),
                });
            }
        }
    }

    fn render_whisper_line_body(
        &mut self,
        ui: &mut egui::Ui,
        line: &WhisperLine,
        text_color: Color32,
    ) {
        if line.text.trim().is_empty() {
            ui.label(
                RichText::new("(empty whisper)")
                    .font(t::small())
                    .color(t::text_muted()),
            );
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for span in &line.spans {
                match span {
                    Span::Text { text, .. } => {
                        if !text.is_empty() {
                            ui.label(RichText::new(text).font(t::small()).color(text_color));
                        }
                    }
                    Span::Emote { code, url, .. } => {
                        if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                            let size = whisper_fit_size(w, h, WHISPER_EMOTE_SIZE);
                            let uri = bytes_uri(url, raw.as_ref());
                            ui.add(
                                egui::Image::from_bytes(
                                    uri,
                                    egui::load::Bytes::Shared(raw.clone()),
                                )
                                .fit_to_exact_size(size),
                            );
                        } else {
                            ui.label(
                                RichText::new(code)
                                    .font(t::small())
                                    .italics()
                                    .color(t::text_secondary()),
                            );
                            if self.whisper_pending_images.insert(url.clone()) {
                                self.send_cmd(AppCommand::FetchImage { url: url.clone() });
                            }
                        }
                    }
                    Span::Emoji { text, url } => {
                        if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                            let size = whisper_fit_size(w, h, WHISPER_EMOTE_SIZE);
                            let uri = bytes_uri(url, raw.as_ref());
                            ui.add(
                                egui::Image::from_bytes(
                                    uri,
                                    egui::load::Bytes::Shared(raw.clone()),
                                )
                                .fit_to_exact_size(size),
                            );
                        } else {
                            ui.label(RichText::new(text).font(t::small()).color(text_color));
                            if self.whisper_pending_images.insert(url.clone()) {
                                self.send_cmd(AppCommand::FetchImage { url: url.clone() });
                            }
                        }
                    }
                    Span::Mention { login } => {
                        ui.label(
                            RichText::new(format!("@{login}"))
                                .font(t::small())
                                .strong()
                                .color(t::mention()),
                        );
                    }
                    Span::Url { text, .. } => {
                        ui.label(
                            RichText::new(text)
                                .font(t::small())
                                .color(t::link())
                                .underline(),
                        );
                    }
                    Span::Badge { name, version } => {
                        ui.label(
                            RichText::new(format!("[{name}/{version}]"))
                                .font(t::small())
                                .color(t::text_muted()),
                        );
                    }
                }
            }
        });
    }

    fn whisper_unread_total(&self) -> u32 {
        self.whisper_unread.values().copied().sum()
    }

    fn touch_whisper_thread_order(&mut self, login: &str) {
        if let Some(idx) = self.whisper_order.iter().position(|entry| entry == login) {
            self.whisper_order.remove(idx);
        }
        self.whisper_order.insert(0, login.to_owned());
    }

    fn mark_whisper_thread_read(&mut self, login: &str) {
        self.whisper_unread.remove(login);
        self.whisper_unread_mentions.remove(login);
    }

    /// Re-apply the current nickname alias list to every cached message, so a
    /// newly added/removed alias takes effect without needing a reconnect.
    fn reapply_nicknames_to_cached_messages(&mut self) {
        // Fast path: no aliases configured (including the empty event at
        // startup), so there's nothing to rewrite and we skip the full
        // cross-channel message walk.
        if self.nicknames.is_empty() {
            return;
        }
        let nicknames = &self.nicknames;
        for (channel, ch_state) in self.state.channels.iter_mut() {
            let channel_login = channel.display_name().to_owned();
            for msg in ch_state.messages.iter_mut() {
                crust_core::model::apply_nickname(
                    nicknames,
                    &msg.sender.login,
                    &channel_login,
                    &mut msg.sender.display_name,
                );
            }
        }
    }

    fn whisper_message_mentions_current_user(&self, spans: &[Span], text: &str) -> bool {
        let Some(username) = self
            .state
            .auth
            .username
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return false;
        };

        if spans.iter().any(
            |span| matches!(span, Span::Mention { login } if login.eq_ignore_ascii_case(username)),
        ) {
            return true;
        }

        let needle = format!("@{}", username.to_ascii_lowercase());
        text.to_ascii_lowercase().contains(&needle)
    }

    fn activate_whisper_thread(&mut self, login: &str) {
        self.whispers_visible = true;
        self.active_whisper_login = Some(login.to_owned());
        self.mark_whisper_thread_read(login);
    }

    fn open_whisper_compose(&mut self, partner_login: &str) {
        if let Some(channel) = self
            .state
            .active_channel
            .as_ref()
            .filter(|channel| channel.is_twitch())
            .cloned()
            .or_else(|| {
                self.state
                    .channel_order
                    .iter()
                    .find(|channel| channel.is_twitch())
                    .cloned()
            })
        {
            self.activate_channel(channel);
            self.chat_input_buf = format!("/w {partner_login} ");
        }
    }

    fn render_whisper_thread_list(
        &mut self,
        ui: &mut egui::Ui,
        thread_order: &[String],
        current_thread: &mut String,
        select_thread: &mut Option<String>,
        compact_layout: bool,
    ) {
        ui.label(
            RichText::new("Threads")
                .font(t::small())
                .strong()
                .color(t::text_secondary()),
        );
        ui.add_space(4.0);

        let pane_width = ui.available_width().max(120.0);
        let title_chars = ((pane_width / 9.0) as usize).clamp(12, 26);
        let preview_chars = ((pane_width / 6.0) as usize).clamp(20, 60);
        let row_height = if compact_layout { 56.0 } else { 64.0 };
        let selected_fill = t::whisper_selected_bg();
        let idle_fill = if t::is_light() {
            t::bg_dialog()
        } else {
            t::bg_surface()
        };

        egui::ScrollArea::vertical()
            .id_salt(("whisper_threads", compact_layout))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for login in thread_order {
                    let is_selected = login == current_thread;
                    let unread = self.whisper_unread.get(login).copied().unwrap_or(0);
                    let unread_mentions = self
                        .whisper_unread_mentions
                        .get(login)
                        .copied()
                        .unwrap_or(0);
                    let last_line = self
                        .whisper_threads
                        .get(login)
                        .and_then(|thread| thread.back())
                        .cloned();
                    let display_name = self
                        .whisper_display_names
                        .get(login)
                        .filter(|name| !name.trim().is_empty())
                        .cloned()
                        .unwrap_or_else(|| login.clone());
                    let preview = last_line
                        .as_ref()
                        .map(|line| {
                            if line.text.trim().is_empty() {
                                "(empty whisper)".to_owned()
                            } else {
                                truncate_with_ellipsis(line.text.trim(), preview_chars)
                            }
                        })
                        .unwrap_or_default();
                    let ts = last_line
                        .as_ref()
                        .map(|line| self.format_whisper_timestamp(line.timestamp))
                        .unwrap_or_default();

                    let row = egui::Frame::new()
                        .fill(if is_selected {
                            selected_fill
                        } else {
                            idle_fill
                        })
                        .stroke(if is_selected {
                            egui::Stroke::new(1.0, t::accent())
                        } else {
                            t::stroke_subtle()
                        })
                        .corner_radius(t::RADIUS)
                        .inner_margin(Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.set_min_height(row_height);
                            ui.horizontal(|ui| {
                                let title = truncate_with_ellipsis(
                                    &format!("@{display_name}"),
                                    title_chars,
                                );
                                ui.label(RichText::new(title).font(t::small()).strong().color(
                                    if is_selected {
                                        t::text_primary()
                                    } else {
                                        t::text_primary()
                                    },
                                ));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !ts.is_empty() {
                                            ui.label(
                                                RichText::new(ts)
                                                    .font(t::tiny())
                                                    .color(t::text_secondary()),
                                            );
                                        }
                                    },
                                );
                            });
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                if !preview.is_empty() {
                                    ui.label(
                                        RichText::new(preview)
                                            .font(t::tiny())
                                            .color(t::text_secondary()),
                                    );
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if unread_mentions > 0 {
                                            chrome::pill(
                                                ui,
                                                compact_badge_count(unread_mentions).to_string(),
                                                t::text_primary(),
                                                t::mention_pill_bg(),
                                            );
                                        } else if unread > 0 {
                                            chrome::pill(
                                                ui,
                                                compact_badge_count(unread).to_string(),
                                                t::gold(),
                                                t::warning_soft_bg(),
                                            );
                                        }
                                    },
                                );
                            });
                        });

                    let row_response = ui.interact(
                        row.response.rect,
                        ui.id().with(("whisper_thread_row", login)),
                        egui::Sense::click(),
                    );
                    if row_response.clicked() {
                        *current_thread = login.clone();
                        *select_thread = Some(login.clone());
                    }
                    ui.add_space(4.0);
                }
            });
    }

    fn render_whisper_conversation_panel(
        &mut self,
        ui: &mut egui::Ui,
        current_thread: &str,
        mark_read_thread: &mut Option<String>,
        compose_thread: &mut Option<String>,
        compact_layout: bool,
    ) {
        let display_name = self
            .whisper_display_names
            .get(current_thread)
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| current_thread.to_owned());
        let lines = self
            .whisper_threads
            .get(current_thread)
            .cloned()
            .unwrap_or_default();
        let self_fill = t::whisper_self_fill();
        let self_stroke = t::whisper_self_stroke();
        let self_text = t::whisper_self_text();
        let self_meta = t::whisper_self_meta();

        let other_fill = t::whisper_other_fill();
        let other_stroke = t::whisper_other_stroke();
        let other_text = t::text_primary();
        let other_meta = t::text_secondary();

        egui::Frame::new()
            .fill(t::bg_surface())
            .stroke(t::stroke_subtle())
            .corner_radius(t::RADIUS)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("@{display_name}"))
                            .font(t::small())
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!("{} messages", lines.len()))
                            .font(t::tiny())
                            .color(t::text_secondary()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(RichText::new("Reply").font(t::small())).clicked() {
                            *compose_thread = Some(current_thread.to_owned());
                        }
                        if ui
                            .button(RichText::new("Mark read").font(t::small()))
                            .clicked()
                        {
                            *mark_read_thread = Some(current_thread.to_owned());
                        }
                    });
                });
            });

        ui.add_space(6.0);

        let messages_height = ui.available_height();
        if messages_height <= 0.0 {
            return;
        }

        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), messages_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::new()
                    .fill(t::bg_base())
                    .stroke(t::stroke_subtle())
                    .corner_radius(t::RADIUS)
                    .inner_margin(Margin::symmetric(8, 8))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt(("whisper_messages", compact_layout))
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                if lines.is_empty() {
                                    ui.label(
                                        RichText::new("No messages in this thread yet.")
                                            .font(t::small())
                                            .color(t::text_muted()),
                                    );
                                    return;
                                }

                                for line in lines {
                                    let bubble_max_width = if compact_layout {
                                        (ui.available_width() * 0.9).max(140.0)
                                    } else {
                                        (ui.available_width() * 0.76).max(220.0)
                                    };
                                    if line.is_self {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::TOP),
                                            |ui| {
                                                ui.allocate_ui_with_layout(
                                                    egui::vec2(bubble_max_width, 0.0),
                                                    egui::Layout::top_down(egui::Align::Min),
                                                    |ui| {
                                                        egui::Frame::new()
                                                            .fill(self_fill)
                                                            .stroke(egui::Stroke::new(1.0, self_stroke))
                                                            .corner_radius(t::RADIUS)
                                                            .inner_margin(Margin::symmetric(10, 8))
                                                            .show(ui, |ui| {
                                                                self.render_whisper_line_body(
                                                                    ui, &line, self_text,
                                                                );
                                                                ui.with_layout(
                                                                    egui::Layout::right_to_left(
                                                                        egui::Align::Center,
                                                                    ),
                                                                    |ui| {
                                                                        ui.label(
                                                                            RichText::new(
                                                                                self.format_whisper_timestamp(line.timestamp),
                                                                            )
                                                                            .font(t::tiny())
                                                                            .color(self_meta),
                                                                        );
                                                                    },
                                                                );
                                                            });
                                                    },
                                                );
                                            },
                                        );
                                    } else {
                                        ui.with_layout(
                                            egui::Layout::left_to_right(egui::Align::TOP),
                                            |ui| {
                                                ui.allocate_ui_with_layout(
                                                    egui::vec2(bubble_max_width, 0.0),
                                                    egui::Layout::top_down(egui::Align::Min),
                                                    |ui| {
                                                        egui::Frame::new()
                                                            .fill(other_fill)
                                                            .stroke(egui::Stroke::new(1.0, other_stroke))
                                                            .corner_radius(t::RADIUS)
                                                            .inner_margin(Margin::symmetric(10, 8))
                                                            .show(ui, |ui| {
                                                                let sender_label = if line
                                                                    .from_display_name
                                                                    .trim()
                                                                    .is_empty()
                                                                {
                                                                    line.from_login.clone()
                                                                } else {
                                                                    line.from_display_name.clone()
                                                                };
                                                                if !sender_label.trim().is_empty() {
                                                                    ui.label(
                                                                        RichText::new(sender_label)
                                                                        .font(t::tiny())
                                                                        .strong()
                                                                        .color(other_meta),
                                                                    );
                                                                    ui.add_space(2.0);
                                                                }
                                                                self.render_whisper_line_body(
                                                                    ui, &line, other_text,
                                                                );
                                                                ui.with_layout(
                                                                    egui::Layout::right_to_left(
                                                                        egui::Align::Center,
                                                                    ),
                                                                    |ui| {
                                                                        ui.label(
                                                                            RichText::new(
                                                                                self.format_whisper_timestamp(line.timestamp),
                                                                            )
                                                                            .font(t::tiny())
                                                                            .color(other_meta),
                                                                        );
                                                                    },
                                                                );
                                                            });
                                                    },
                                                );
                                            },
                                        );
                                    }
                                    ui.add_space(4.0);
                                }
                            });
                    });
            },
        );
    }

    fn show_whispers_window(&mut self, ctx: &Context) {
        if !self.whispers_visible {
            return;
        }

        let mut window_open = self.whispers_visible;
        let thread_order = self.whisper_order.clone();
        let mut select_thread: Option<String> = None;
        let mut mark_read_thread: Option<String> = None;
        let mut compose_thread: Option<String> = None;

        egui::Window::new("Whispers")
            .open(&mut window_open)
            .default_size(egui::vec2(760.0, 500.0))
            .min_width(340.0)
            .show(ctx, |ui| {
                if thread_order.is_empty() {
                    ui.label(
                        RichText::new(
                            "No whispers yet. Incoming whispers will appear here for quick reply and tracking.",
                        )
                        .font(t::small())
                        .color(t::text_muted()),
                    );
                    return;
                }

                let mut active_thread = self
                    .active_whisper_login
                    .clone()
                    .or_else(|| thread_order.first().cloned());
                if active_thread
                    .as_ref()
                    .map(|current| !thread_order.iter().any(|entry| entry == current))
                    .unwrap_or(false)
                {
                    active_thread = thread_order.first().cloned();
                }
                let active_thread = active_thread.unwrap_or_else(|| thread_order[0].clone());
                let mut current_thread = active_thread.clone();

                let compact_layout = ui.available_width() < 680.0;
                if compact_layout {
                    let thread_area_height = (ui.available_height() * 0.35).clamp(84.0, 180.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), thread_area_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            chrome::card_frame().show(ui, |ui| {
                                self.render_whisper_thread_list(
                                    ui,
                                    &thread_order,
                                    &mut current_thread,
                                    &mut select_thread,
                                    true,
                                );
                            });
                        },
                    );
                    ui.add_space(8.0);
                    self.render_whisper_conversation_panel(
                        ui,
                        &current_thread,
                        &mut mark_read_thread,
                        &mut compose_thread,
                        true,
                    );
                } else {
                    ui.horizontal(|ui| {
                        let pane_height = ui.available_height();
                        let thread_pane_width = (ui.available_width() * 0.30).clamp(160.0, 280.0);

                        ui.allocate_ui_with_layout(
                            egui::vec2(thread_pane_width, pane_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                self.render_whisper_thread_list(
                                    ui,
                                    &thread_order,
                                    &mut current_thread,
                                    &mut select_thread,
                                    false,
                                );
                            },
                        );

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.allocate_ui_with_layout(
                            ui.available_size(),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                self.render_whisper_conversation_panel(
                                    ui,
                                    &current_thread,
                                    &mut mark_read_thread,
                                    &mut compose_thread,
                                    false,
                                );
                            },
                        );
                    });
                }
            });

        self.whispers_visible = window_open;
        if let Some(thread) = select_thread {
            self.active_whisper_login = Some(thread.clone());
            self.mark_whisper_thread_read(&thread);
        }
        if let Some(thread) = mark_read_thread {
            self.mark_whisper_thread_read(&thread);
        }
        if let Some(thread) = compose_thread {
            self.mark_whisper_thread_read(&thread);
            self.open_whisper_compose(&thread);
        }
        if self.whispers_visible
            && self.active_whisper_login.is_none()
            && !self.whisper_order.is_empty()
        {
            if let Some(active) = self.whisper_order.first().cloned() {
                self.active_whisper_login = Some(active.clone());
                self.mark_whisper_thread_read(&active);
            }
        }
    }

    fn format_whisper_timestamp(&self, timestamp: DateTime<Utc>) -> String {
        let local = timestamp.with_timezone(&Local);
        if self.use_24h_timestamps {
            if self.show_timestamp_seconds {
                local.format("%H:%M:%S").to_string()
            } else {
                local.format("%H:%M").to_string()
            }
        } else if self.show_timestamp_seconds {
            local.format("%I:%M:%S %p").to_string()
        } else {
            local.format("%I:%M %p").to_string()
        }
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
            AppEvent::GeneralSettingsUpdated { auto_join, .. } => {
                self.loading_screen
                    .on_event(LoadEvent::StartupChannelsConfigured {
                        channels: auto_join.clone(),
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
                if let Some(host) = plugin_host() {
                    host.update_channel_snapshot(
                        channel.clone(),
                        PluginChannelSnapshot {
                            is_joined: true,
                            ..Default::default()
                        },
                    );
                }
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
                    self.stream_tracker
                        .unwatch_channel(&login, crust_core::notifications::Platform::Twitch);
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
                if let Some(host) = plugin_host() {
                    host.update_channel_snapshot(
                        channel.clone(),
                        PluginChannelSnapshot {
                            is_joined: false,
                            ..Default::default()
                        },
                    );
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
                        let st = status.as_deref().map(str::trim).filter(|s| !s.is_empty());
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

                let channel_live = self
                    .live_map_cache
                    .get(channel.display_name())
                    .copied();
                let is_watching_channel =
                    self.state.active_channel.as_ref() == Some(&channel);
                let highlight_rule = crust_core::highlight::first_match_context_rule_message(
                    &self.highlight_rules,
                    &message,
                    channel.display_name(),
                    channel_live,
                    is_watching_channel,
                )
                .cloned();
                if highlight_rule.is_some() {
                    message.flags.is_highlighted = true;
                }
                let (highlight_mentions, _highlight_alert, highlight_sound, highlight_sound_url) =
                    highlight_rule
                        .as_ref()
                        .map(|rule| {
                            (
                                rule.show_in_mentions,
                                rule.has_alert,
                                rule.has_sound,
                                rule.sound_url.clone().filter(|s| !s.trim().is_empty()),
                            )
                        })
                        .unwrap_or((false, false, false, None));

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
                            ..
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

                let mut chatters_became_dirty = false;
                let mut request_attention: Option<egui::UserAttentionType> = None;
                let mut desktop_notification: Option<(String, String, bool)> = None;
                // Decide up-front whether this message also belongs in the
                // cross-channel Mentions pseudo-tab. Cloned here because
                // `message` gets moved into `ch.push_message` below. The
                // actual push into `state.mentions` happens after the
                // per-channel borrow is released (tracked via
                // `mention_bump_unread`).
                let mentions_capture: Option<ChatMessage> = if (message.flags.is_highlighted
                    || message.flags.is_mention
                    || message.flags.is_first_msg
                    || message.flags.is_pinned
                    || highlight_mentions)
                    && !message.flags.suppress_notification
                {
                    Some(message.clone())
                } else {
                    None
                };
                let mut mention_pushable = mentions_capture.is_some();
                let mentions_tab_active = self
                    .state
                    .active_channel
                    .as_ref()
                    .map(|c| c.is_mentions())
                    .unwrap_or(false);
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
                                    // Defer the sort.  On busy channels with
                                    // thousands of chatters, rebuilding the
                                    // sorted vec on every unique new chatter
                                    // (~O(n log n) per insert) is the main
                                    // freeze source during history load.
                                    chatters_became_dirty = true;
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
                    if absorbed {
                        // Echo landed on an existing local copy; the mention
                        // (if any) was already recorded when that local copy
                        // was first pushed. Skip the mirror to avoid
                        // double-counting in the Mentions tab.
                        mention_pushable = false;
                    }
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

                        if !message.flags.is_history && !message.flags.suppress_notification {
                            // Audio pings are routed through the
                            // cross-platform SoundController; streamer
                            // mode suppression and per-event volume/path
                            // lookup happen inside the controller.  The
                            // rate limiter inside the controller prevents
                            // overlapping plays when a single message
                            // matches several conditions (eg. a mention
                            // that also fires a highlight rule).
                            if message.flags.is_mention {
                                self.sound_controller
                                    .play_event(crust_core::sound::SoundEvent::Mention);
                            } else if highlight_sound {
                                self.sound_controller
                                    .play_highlight_override(highlight_sound_url.as_deref());
                            }
                            match &message.msg_kind {
                                MsgKind::Sub { .. } => {
                                    self.sound_controller
                                        .play_event(crust_core::sound::SoundEvent::Subscribe);
                                }
                                MsgKind::Raid { .. } => {
                                    self.sound_controller
                                        .play_event(crust_core::sound::SoundEvent::Raid);
                                }
                                _ => {}
                            }
                        }

                        if self.desktop_notifications_enabled
                            && !message.flags.is_history
                            && !message.flags.suppress_notification
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
                // Mirror the message into the cross-channel Mentions buffer
                // if it qualifies. Done after the per-channel borrow is
                // released so we can mutably touch `self.state` again. Only
                // bump the Mentions-tab unread counter when the user is not
                // already looking at that tab and the message is live
                // (history replays get `is_history = true` upstream).
                if mention_pushable {
                    if let Some(mut m) = mentions_capture {
                        // Preserve per-channel attribution on the clone even
                        // if upstream code tweaked the live copy's `channel`
                        // (defensive: state model guarantees it already).
                        m.channel = channel.clone();
                        let is_history = m.flags.is_history;
                        let bump = !mentions_tab_active && !is_history;
                        self.state.push_mention(m, bump);
                    }
                }
                if let Some(attention) = request_attention {
                    self.request_user_attention(ctx, attention);
                }
                if let Some((title, body, with_sound)) = desktop_notification {
                    self.dispatch_desktop_notification(&title, &body, with_sound);
                }
                if chatters_became_dirty {
                    self.chatters_dirty.insert(channel);
                }
            }
            AppEvent::WhisperReceived {
                from_login,
                from_display_name,
                target_login,
                text,
                twitch_emotes,
                is_self,
                timestamp,
                is_history,
            } => {
                let from_login_norm = Self::normalize_whisper_login(&from_login);
                let target_login_norm = Self::normalize_whisper_login(&target_login);
                let partner_login = if is_self {
                    target_login_norm.or(from_login_norm)
                } else {
                    from_login_norm.or(target_login_norm)
                };
                let Some(partner_login) = partner_login else {
                    return;
                };

                let display_name = if is_self {
                    if target_login.trim().is_empty() {
                        partner_login.clone()
                    } else {
                        target_login.trim().to_owned()
                    }
                } else if from_display_name.trim().is_empty() {
                    partner_login.clone()
                } else {
                    from_display_name.trim().to_owned()
                };
                self.whisper_display_names
                    .insert(partner_login.clone(), display_name.clone());

                let text = text.to_owned();
                let spans = self.tokenize_whisper_text(&text, &twitch_emotes);
                let mentions_current_user =
                    self.whisper_message_mentions_current_user(&spans, &text);
                self.queue_whisper_span_images(&spans);
                let notification_body_text = text.trim().to_owned();

                let line = WhisperLine {
                    from_login: from_login.trim().to_owned(),
                    from_display_name: from_display_name.trim().to_owned(),
                    text,
                    twitch_emotes,
                    spans,
                    timestamp,
                    is_self,
                };

                let thread = self
                    .whisper_threads
                    .entry(partner_login.clone())
                    .or_default();
                thread.push_back(line);
                while thread.len() > MAX_WHISPERS_PER_THREAD {
                    thread.pop_front();
                }

                self.touch_whisper_thread_order(&partner_login);

                if self.active_whisper_login.is_none() {
                    self.active_whisper_login = Some(partner_login.clone());
                }

                let thread_is_focused = self.whispers_visible
                    && self
                        .active_whisper_login
                        .as_deref()
                        .map(|v| v.eq_ignore_ascii_case(&partner_login))
                        .unwrap_or(false);
                if thread_is_focused {
                    self.mark_whisper_thread_read(&partner_login);
                } else if !is_history {
                    *self
                        .whisper_unread
                        .entry(partner_login.clone())
                        .or_insert(0) += 1;
                    if !is_self && mentions_current_user {
                        *self
                            .whisper_unread_mentions
                            .entry(partner_login.clone())
                            .or_insert(0) += 1;
                    }
                    if !is_self {
                        // Audio ping for incoming whispers (self-echoes
                        // from our own outgoing whispers don't deserve a
                        // notification sound).
                        self.sound_controller
                            .play_event(crust_core::sound::SoundEvent::Whisper);
                    }
                    if self.desktop_notifications_enabled {
                        let title = Self::truncate_notification_text(
                            &format!("Whisper from {display_name}"),
                            80,
                        );
                        let body = if notification_body_text.trim().is_empty() {
                            "(empty whisper)".to_owned()
                        } else {
                            Self::truncate_notification_text(notification_body_text.trim(), 220)
                        };
                        self.dispatch_desktop_notification(&title, &body, true);
                        self.request_user_attention(ctx, egui::UserAttentionType::Informational);
                    }
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
                self.whisper_pending_images.remove(&uri);
                self.live_feed_pending_thumbnails.remove(&uri);
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
                // `sort_by_cached_key` evaluates the key once per element (O(n))
                // instead of ~O(n log n) lowercase allocations in `sort_by`.
                emotes.sort_by_cached_key(|e| e.code.to_ascii_lowercase());
                self.emote_catalog = emotes;

                // Re-tokenize existing messages across ALL channels so that
                // emotes that loaded after the messages arrived (e.g. global
                // BTTV/FFZ/7TV emotes like LUL) get resolved.
                let emote_map = build_emote_lookup(&self.emote_catalog);
                if !emote_map.is_empty() {
                    let mut whisper_urls_to_fetch: HashSet<String> = HashSet::new();

                    // Re-tokenize only the tail window per channel so a
                    // mid-session emote-catalog update (BTTV/FFZ/7TV arriving
                    // after messages did) doesn't stall the UI thread for
                    // thousands of older off-screen rows.
                    const RETOKENIZE_TAIL: usize = 400;
                    for ch in self.state.channels.values_mut() {
                        let len = ch.messages.len();
                        let start = len.saturating_sub(RETOKENIZE_TAIL);
                        for msg in ch.messages.iter_mut().skip(start) {
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

                    for thread in self.whisper_threads.values_mut() {
                        for line in thread.iter_mut() {
                            let new_spans =
                                tokenize_whisper_text(&line.text, &line.twitch_emotes, &emote_map);
                            for span in &new_spans {
                                if let Span::Emote { url, .. } | Span::Emoji { url, .. } = span {
                                    if !self.emote_bytes.contains_key(url.as_str()) {
                                        whisper_urls_to_fetch.insert(url.clone());
                                    }
                                }
                            }
                            line.spans = new_spans;
                        }
                    }

                    for url in whisper_urls_to_fetch {
                        if self.whisper_pending_images.insert(url.clone()) {
                            self.send_cmd(AppCommand::FetchImage { url });
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
                if let Some(host) = plugin_host() {
                    host.update_auth_snapshot(PluginAuthSnapshot {
                        logged_in: true,
                        username: self.state.auth.username.clone(),
                        user_id: self.state.auth.user_id.clone(),
                        display_name: self.state.auth.username.clone(),
                    });
                }
            }
            AppEvent::LoggedOut => {
                self.auth_refresh_inflight = false;
                self.state.auth.logged_in = false;
                self.state.auth.username = None;
                self.state.auth.user_id = None;
                self.state.auth.avatar_url = None;
                self.whispers_visible = false;
                self.whisper_threads.clear();
                self.whisper_display_names.clear();
                self.whisper_order.clear();
                self.whisper_unread.clear();
                self.whisper_unread_mentions.clear();
                self.active_whisper_login = None;
                self.whisper_pending_images.clear();
                if let Some(host) = plugin_host() {
                    host.update_auth_snapshot(PluginAuthSnapshot::default());
                }
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
            AppEvent::UpdaterSettingsUpdated {
                update_checks_enabled,
                last_checked_at,
                skipped_version,
            } => {
                self.update_checks_enabled = update_checks_enabled;
                self.updater_last_checked_at = last_checked_at;
                self.updater_skipped_version = skipped_version;
            }
            AppEvent::UpdateAvailable {
                version,
                release_url,
                asset_name,
            } => {
                self.updater_available_version = Some(version.clone());
                self.updater_available_asset = Some(asset_name.clone());
                self.updater_available_release_url = Some(release_url.clone());
                self.updater_install_inflight = false;
                self.push_event_toast(
                    format!("Update available: v{} ({})", version, asset_name),
                    Color32::from_rgb(92, 180, 255),
                    false,
                );

                if let Some(ch_id) = self.state.active_channel.clone() {
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel: ch_id,
                        text: format!(
                            "A new Crust release is available (v{}). Open: {}",
                            version, release_url
                        ),
                    });
                }
            }
            AppEvent::UpdateCheckUpToDate { version } => {
                self.updater_available_version = None;
                self.updater_available_asset = None;
                self.updater_available_release_url = None;
                self.push_event_toast(
                    format!("Crust is up to date (v{})", version),
                    Color32::from_rgb(116, 193, 129),
                    false,
                );
            }
            AppEvent::UpdateCheckFailed { message, manual } => {
                tracing::warn!("Update check failed: {message}");
                if manual {
                    self.push_event_toast(
                        format!("Update check failed: {}", message),
                        Color32::from_rgb(219, 116, 116),
                        false,
                    );
                }
            }
            AppEvent::UpdateInstallStarted { version } => {
                self.updater_install_inflight = true;
                self.push_event_toast(
                    format!("Preparing update v{}...", version),
                    Color32::from_rgb(92, 180, 255),
                    false,
                );
            }
            AppEvent::UpdateInstallScheduled {
                version,
                restart_now,
            } => {
                self.updater_install_inflight = false;
                self.updater_available_version = None;
                self.updater_available_asset = None;
                self.updater_available_release_url = None;
                let msg = if restart_now {
                    format!("Update v{} staged. Restarting to apply...", version)
                } else {
                    format!("Update v{} staged. Restart Crust to apply.", version)
                };
                self.push_event_toast(msg, Color32::from_rgb(116, 193, 129), false);
            }
            AppEvent::UpdateInstallFailed { version, message } => {
                self.updater_install_inflight = false;
                let text = if version.trim().is_empty() {
                    format!("Update install failed: {}", message)
                } else {
                    format!("Update v{} failed: {}", version, message)
                };
                self.push_event_toast(text, Color32::from_rgb(219, 116, 116), false);
            }
            AppEvent::MentionsLoaded { messages } => {
                // Backfill the cross-channel Mentions pseudo-tab from the
                // SQLite log on startup. Merge by timestamp so live mentions
                // already accumulated during startup interleave correctly.
                self.state.prepend_mentions_history(messages);
            }
            AppEvent::HistoryLoaded { channel, messages } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    // Scroll to the seam between history and live chat so the
                    // user sees context instead of waking up at the bottom.
                    // Only scroll when there is a real seami.e. live chat
                    // has already started accumulating. When the channel has
                    // no live messages yet, scrolling to the "seam" would
                    // force the newest history row to the top of the viewport
                    // and leave the rest of the history above the foldwhich
                    // makes the user's most recent sent message look like it's
                    // pinned at the top. Fall through to normal stick-to-bottom
                    // in that case.
                    let live_count_before = ch.messages.len();
                    let seam_id = if live_count_before > 0 && live_count_before < 100 {
                        ch.messages.front().and_then(|m| m.server_id.clone())
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
            AppEvent::UserProfileLoaded { mut profile } => {
                // Apply nickname alias so the user card shows the custom name.
                let channel_scope = self
                    .user_profile_popup
                    .channel
                    .as_ref()
                    .map(|c| c.display_name().to_owned())
                    .unwrap_or_default();
                crust_core::model::apply_nickname(
                    &self.nicknames,
                    &profile.login,
                    &channel_scope,
                    &mut profile.display_name,
                );
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
                    // Bound the scan to the most-recent N messages so a cold
                    // popup on a 10k-message buffer doesn't stall the UI thread.
                    const USERCARD_SCAN_LIMIT: usize = 2_000;
                    let ch = self.user_profile_popup.channel.clone();
                    let login_ref = profile.login.as_str();
                    let logs: Vec<_> = ch
                        .as_ref()
                        .and_then(|c| self.state.channels.get(c))
                        .map(|s| {
                            s.messages
                                .iter()
                                .rev()
                                .take(USERCARD_SCAN_LIMIT)
                                .filter(|m| {
                                    m.sender.login.eq_ignore_ascii_case(login_ref)
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
                                .take(USERCARD_SCAN_LIMIT)
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
                    let mut shared_channels: Vec<String> =
                        self.state
                            .channels
                            .iter()
                            .filter_map(|(cid, state)| {
                                let seen_in_chatters = state
                                    .chatters
                                    .iter()
                                    .any(|name| name.eq_ignore_ascii_case(&profile.login));
                                let seen_in_messages =
                                    state.messages.iter().rev().take(400).any(|m| {
                                        m.sender.login.eq_ignore_ascii_case(&profile.login)
                                    });
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
                    let popup_login = profile.login.clone();
                    let popup_channel = self.user_profile_popup.channel.clone();
                    self.user_profile_popup.set_profile(profile);

                    // Silently fetch follow age via Helix when this is a Twitch
                    // tab so the usercard can show "Follow age" or "Not
                    // following" without the user running /follow-age.
                    if let Some(ch) = popup_channel {
                        if ch.is_twitch() && !popup_login.is_empty() {
                            self.send_cmd(AppCommand::FetchUserCardFollowAge {
                                channel: ch,
                                login: popup_login,
                            });
                        }
                    }
                }
            }
            AppEvent::UserCardFollowAgeLoaded {
                channel,
                login,
                followed_at,
            } => {
                self.user_profile_popup
                    .set_follow_age_resolved(&login, &channel, followed_at);
            }
            AppEvent::AutoClaimBonusPointsUpdated { enabled } => {
                self.auto_claim_bonus_points = enabled;
            }
            AppEvent::ChannelPointsBalanceUpdated { channel, balance } => {
                self.channel_points.insert(channel, balance);
            }
            AppEvent::ChannelPointsClaimed {
                channel,
                points,
                balance,
            } => {
                self.channel_points.insert(channel.clone(), balance);
                self.analytics_panel
                    .record_bonus_claim(&channel, points, balance);
            }
            AppEvent::StreamStatusUpdated {
                login,
                is_live,
                title,
                game,
                viewers,
            } => {
                let login = login.to_ascii_lowercase();
                let (title, game, viewers) = {
                    let entry =
                        self.stream_statuses
                            .entry(login.clone())
                            .or_insert(StreamStatusInfo {
                                is_live,
                                title: None,
                                game: None,
                                viewers: None,
                            });
                    entry.is_live = is_live;
                    if !is_live {
                        entry.title = None;
                        entry.game = None;
                        entry.viewers = None;
                    } else {
                        if title.is_some() {
                            entry.title = title.clone();
                        }
                        if game.is_some() {
                            entry.game = game.clone();
                        }
                        if viewers.is_some() {
                            entry.viewers = viewers;
                        }
                    }
                    (entry.title.clone(), entry.game.clone(), entry.viewers)
                };

                self.live_map_cache.insert(login.clone(), is_live);
                self.stream_status_fetch_inflight.remove(&login);
                self.stream_status_fetched
                    .insert(login.clone(), std::time::Instant::now());

                self.handle_stream_status_transition(ctx, &login, is_live, title, game, viewers);
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
            AppEvent::LowTrustStatusUpdated {
                channel,
                login,
                user_id,
                display_name,
                status,
            } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    match status {
                        Some(s) => ch.set_low_trust(&login, &user_id, &display_name, s),
                        None => ch.clear_low_trust(&login),
                    }
                }
            }
            AppEvent::ClearUserMessagesLocally { channel, login } => {
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
                if let Some(host) = plugin_host() {
                    let mut snapshot = PluginChannelSnapshot::default();
                    snapshot.is_mod = is_mod;
                    snapshot.is_broadcaster = self
                        .state
                        .auth
                        .username
                        .as_deref()
                        .map(|u| u.eq_ignore_ascii_case(channel.display_name()))
                        .unwrap_or(false);
                    host.update_channel_snapshot(channel.clone(), snapshot);
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
            AppEvent::SharedChatSessionUpdated { channel, session } => {
                match session {
                    Some(state) => {
                        self.state.shared_chat_sessions.insert(channel, state);
                    }
                    None => {
                        self.state.shared_chat_sessions.remove(&channel);
                    }
                }
            }
            AppEvent::SharedChannelResolved {
                room_id,
                login,
                display_name,
                profile_url,
            } => {
                // Cache the profile so future renders can resolve the chip
                // without re-fetching and so new mirrored messages pick up
                // the metadata on arrival.
                self.state.shared_channel_profiles.insert(
                    room_id.clone(),
                    crust_core::state::SharedChannelProfile {
                        login: login.clone(),
                        display_name: display_name.clone(),
                        profile_url: profile_url.clone(),
                    },
                );
                // Back-fill every buffered ChatMessage with the same
                // `source-room-id` so already-rendered rows switch from the
                // text-only chip to the proper source-channel badge once the
                // profile resolves.
                for ch in self.state.channels.values_mut() {
                    for msg in ch.messages.iter_mut() {
                        if let Some(shared) = msg.shared.as_mut() {
                            if shared.room_id == room_id {
                                if shared.login.is_none() {
                                    shared.login = Some(login.clone());
                                }
                                if shared.display_name.is_none() {
                                    shared.display_name = Some(display_name.clone());
                                }
                                if shared.profile_url.is_none() {
                                    shared.profile_url = profile_url.clone();
                                }
                            }
                        }
                    }
                }
                for msg in self.state.mentions.iter_mut() {
                    if let Some(shared) = msg.shared.as_mut() {
                        if shared.room_id == room_id {
                            if shared.login.is_none() {
                                shared.login = Some(login.clone());
                            }
                            if shared.display_name.is_none() {
                                shared.display_name = Some(display_name.clone());
                            }
                            if shared.profile_url.is_none() {
                                shared.profile_url = profile_url.clone();
                            }
                        }
                    }
                }
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
            AppEvent::SpellDictionaryUpdated { enabled, words } => {
                self.spellcheck_enabled = enabled;
                self.spell_custom_dict = words.clone();
                crate::spellcheck::set_enabled(enabled);
                crate::spellcheck::set_user_dict(words);
            }
            AppEvent::RestoreLastActiveChannel {
                channel,
                channel_order,
                split_panes,
                split_panes_focused,
                whispers_visible,
                last_whisper_login,
            } => {
                if !channel.is_empty() {
                    self.pending_restore_channel = Some(channel.clone());
                    self.last_saved_active_channel = Some(channel);
                }
                if !channel_order.is_empty() {
                    self.pending_restore_channel_order = channel_order.clone();
                    self.last_saved_channel_order = channel_order;
                }
                if !split_panes.is_empty() {
                    self.pending_restore_split_panes = split_panes.clone();
                    self.pending_restore_split_focused = split_panes_focused;
                    self.last_saved_split_panes = split_panes;
                    self.last_saved_split_focused = split_panes_focused;
                }
                self.whispers_visible = whispers_visible;
                self.last_saved_whispers_visible = whispers_visible;
                if !last_whisper_login.is_empty() {
                    self.active_whisper_login = Some(last_whisper_login.clone());
                }
                self.last_saved_whisper_login = last_whisper_login;
            }
            AppEvent::FontSettingsUpdated {
                chat_font_size,
                ui_font_size,
                topbar_font_size,
                tabs_font_size,
                timestamps_font_size,
                pills_font_size,
                popups_font_size,
                chips_font_size,
                usercard_font_size,
                dialog_font_size,
            } => {
                self.chat_font_size = t::set_chat_font_size(chat_font_size);
                self.ui_font_size = t::set_ui_font_size(ui_font_size);
                self.topbar_font_size = t::set_topbar_font_size(topbar_font_size);
                self.tabs_font_size = t::set_tabs_font_size(tabs_font_size);
                self.timestamps_font_size = t::set_timestamps_font_size(timestamps_font_size);
                self.pills_font_size = t::set_pills_font_size(pills_font_size);
                self.popups_font_size = t::set_popups_font_size(popups_font_size);
                self.chips_font_size = t::set_chips_font_size(chips_font_size);
                self.usercard_font_size = t::set_usercard_font_size(usercard_font_size);
                self.dialog_font_size = t::set_dialog_font_size(dialog_font_size);
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
                self.settings_highlight_rule_bufs =
                    rules.iter().map(|r| r.pattern.clone()).collect();
            }
            AppEvent::FilterRecordsUpdated { records } => {
                self.filter_records = crust_core::model::filters::compile_filters(&records);
                self.settings_filter_records = records.clone();
                self.settings_filter_record_bufs =
                    records.iter().map(|r| r.pattern.clone()).collect();
            }
            AppEvent::ModActionPresetsUpdated { presets } => {
                self.mod_action_presets = presets.clone();
                self.settings_mod_action_presets = presets;
            }
            AppEvent::NicknamesUpdated { nicknames } => {
                self.nicknames = nicknames.clone();
                self.settings_nicknames = nicknames;
                // Retro-apply to messages already in memory so the chat view
                // reflects the new aliases without needing a reconnect.
                self.reapply_nicknames_to_cached_messages();
            }
            AppEvent::IgnoredUsersUpdated { users } => {
                self.ignored_users = users.clone();
                self.settings_ignored_users = users;
                self.compiled_ignored_users =
                    crust_core::ignores::CompiledIgnoredUsers::new(&self.ignored_users);
            }
            AppEvent::IgnoredPhrasesUpdated { phrases } => {
                self.ignored_phrases = phrases.clone();
                self.settings_ignored_phrases = phrases;
            }
            AppEvent::CommandAliasesUpdated { aliases } => {
                self.command_aliases = aliases.clone();
                self.settings_command_aliases = aliases;
            }
            AppEvent::HotkeyBindingsUpdated { bindings } => {
                // Runtime always feeds us the defaults-merged set so we
                // don't have to remember which actions were missing.
                let merged = crust_core::HotkeyBindings::from_pairs(bindings);
                self.hotkey_bindings = merged.clone();
                self.settings_hotkey_bindings = merged;
            }
            AppEvent::UserPronounsLoaded { login, pronouns } => {
                tracing::info!(
                    "UI: UserPronounsLoaded login={login} pronouns={:?}",
                    pronouns
                );
                self.user_profile_popup.set_pronouns(&login, pronouns);
            }
            AppEvent::UsercardSettingsUpdated { show_pronouns } => {
                self.show_pronouns_in_usercard = show_pronouns;
            }
            AppEvent::StreamerModeSettingsUpdated {
                mode,
                hide_link_previews,
                hide_viewer_counts,
                suppress_sounds,
            } => {
                self.streamer_mode_setting = mode;
                self.streamer_hide_link_previews = hide_link_previews;
                self.streamer_hide_viewer_counts = hide_viewer_counts;
                self.streamer_suppress_sounds = suppress_sounds;
                self.settings_streamer_mode = self.streamer_mode_setting.clone();
                self.settings_streamer_hide_link_previews = hide_link_previews;
                self.settings_streamer_hide_viewer_counts = hide_viewer_counts;
                self.settings_streamer_suppress_sounds = suppress_sounds;
                self.sync_sound_suppression();
            }
            AppEvent::StreamerModeActiveChanged { active } => {
                self.streamer_mode_active = active;
                self.sync_sound_suppression();
            }
            AppEvent::SoundSettingsUpdated { events } => {
                let settings = crust_core::sound::SoundSettings::from_pairs(events);
                self.settings_sounds = settings.clone();
                self.sound_controller.apply_settings(settings);
            }
            AppEvent::ExternalToolsSettingsUpdated {
                streamlink_path,
                streamlink_quality,
                streamlink_extra_args,
                player_template,
                mpv_path,
                streamlink_session_token,
            } => {
                self.external_streamlink_path = streamlink_path;
                self.external_streamlink_quality = streamlink_quality;
                self.external_streamlink_extra_args = streamlink_extra_args;
                self.external_player_template = player_template;
                self.external_mpv_path = mpv_path;
                self.external_streamlink_session_token = streamlink_session_token;
            }
            AppEvent::TabVisibilityRulesUpdated { rules } => {
                let map: HashMap<ChannelId, TabVisibilityRule> = rules.into_iter().collect();
                self.state.replace_tab_visibility_rules(map);
            }
            AppEvent::UploadStarted { channel: _ } => {
                // Status already shown as a SystemNotice inline message.
            }
            AppEvent::UploadFinished { channel, result } => match result {
                Ok(url) => {
                    let buf = self.input_buf_for_channel_mut(&channel);
                    if !buf.is_empty() && !buf.ends_with(' ') {
                        buf.push(' ');
                    }
                    buf.push_str(&url);
                    buf.push(' ');
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel,
                        text: format!("Upload complete: {url}"),
                    });
                }
                Err(err) => {
                    self.send_cmd(AppCommand::InjectLocalMessage {
                        channel,
                        text: format!("Upload failed: {err}"),
                    });
                }
            },
            AppEvent::AuthExpired => {
                warn!("Auth expired - checking refresh path");
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
                        text: "\u{26a0}\u{fe0f} Twitch auth check failed. Trying token refresh..."
                            .into(),
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
                let keep_ids: HashSet<String> = requests
                    .iter()
                    .map(|item| item.request_id.clone())
                    .collect();
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

                // Only the most-recent messages per channel are visible /
                // about-to-be-visible after virtualized scrolling.  Walking
                // every cached message across every channel for every new
                // sender cosmetic event is O(N * M) and is a major freeze
                // source on busy channels (many users resolve 7TV styles at
                // once).  Bound to the tail window.
                const COSMETICS_BACKFILL_TAIL: usize = 400;
                let mut _updated = 0u32;
                for ch in self.state.channels.values_mut() {
                    let len = ch.messages.len();
                    let start = len.saturating_sub(COSMETICS_BACKFILL_TAIL);
                    for msg in ch.messages.iter_mut().skip(start) {
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
            AppEvent::PluginUiAction { .. }
            | AppEvent::PluginUiChange { .. }
            | AppEvent::PluginUiSubmit { .. }
            | AppEvent::PluginUiWindowClosed { .. } => {
                // Plugin UI interaction events are routed directly back into the
                // plugin host; the main app state does not reduce them.
            }
            AppEvent::HypeTrainUpdated {
                channel,
                phase,
                level,
                progress,
                goal,
                total,
                top_contributor_login,
                top_contributor_type,
                top_contributor_total,
                ends_at,
            } => {
                self.state.apply_hype_train_update(
                    channel,
                    crust_core::state::HypeTrainState {
                        phase,
                        level,
                        progress,
                        goal,
                        total,
                        top_contributor_login,
                        top_contributor_type,
                        top_contributor_total,
                        ends_at,
                        updated_at: std::time::Instant::now(),
                    },
                );
            }
            AppEvent::RaidBannerShown {
                channel,
                display_name,
                viewer_count,
                source_login,
            } => {
                self.state.show_raid_banner(
                    channel,
                    crust_core::state::RaidBannerState {
                        display_name,
                        viewer_count,
                        source_login,
                        shown_at: std::time::Instant::now(),
                        dismissed: false,
                    },
                );
            }
            AppEvent::LiveFeedUpdated { channels } => {
                self.request_live_feed_thumbnails(&channels);
                self.state.apply_live_snapshot(channels);
            }
            AppEvent::LiveFeedError { message } => {
                self.state.apply_live_error(message);
            }
            AppEvent::LiveFeedPartialUpdate { channels, error } => {
                self.request_live_feed_thumbnails(&channels);
                self.state.apply_live_partial(channels, error);
            }
            AppEvent::TwitchWebviewLoginState { logged_in } => {
                self.twitch_webview_logged_in = logged_in;
            }
            AppEvent::TwitchWebviewBonusClaimed { channel } => {
                self.analytics_panel.record_bonus_claim(&channel, 0, 0);
            }
        }
    }

    /// Resolve the input-buffer that should receive text for `channel`.
    /// Prefers a matching split pane; falls back to the classic single-channel
    /// input buffer.
    fn input_buf_for_channel_mut(&mut self, channel: &ChannelId) -> &mut String {
        if let Some(pane) = self
            .split_panes
            .panes
            .iter_mut()
            .find(|p| &p.channel == channel)
        {
            return &mut pane.input_buf;
        }
        &mut self.chat_input_buf
    }

    fn send_cmd(&self, cmd: AppCommand) {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => {}
            Err(TrySendError::Full(cmd)) => {
                // User actions (send message/slash commands) should not be
                // silently dropped under transient command bursts.
                if self.cmd_tx.blocking_send(cmd).is_err() {
                    warn!("Command channel closed");
                }
            }
            Err(TrySendError::Closed(_)) => {
                warn!("Command channel closed");
            }
        }
    }

    /// Apply the user's custom command aliases to `text` before it hits the
    /// slash parser or chat backend. Returns `Ok(None)` when the text was
    /// unchanged (no alias matched), `Ok(Some(expanded))` when an alias
    /// expanded successfully, or `Err(cmd)` with an `InjectLocalMessage`
    /// error the caller should dispatch instead of the original text (e.g.
    /// when the alias chain cycled).
    ///
    /// Keep this side-effect free: the caller is responsible for injecting
    /// the error and for routing the expanded text through the normal
    /// dispatch path.
    fn expand_outgoing_aliases(
        &self,
        text: &str,
        channel: &ChannelId,
    ) -> Result<Option<String>, AppCommand> {
        if self.command_aliases.is_empty() {
            return Ok(None);
        }
        let channel_login = channel.display_name();
        let user_login = self.state.auth.username.clone().unwrap_or_default();
        match crust_core::commands::expand_command_aliases(
            text,
            &self.command_aliases,
            channel_login,
            &user_login,
        ) {
            Ok(crust_core::commands::AliasExpansion::Unchanged) => Ok(None),
            Ok(crust_core::commands::AliasExpansion::Expanded { text, .. }) => {
                Ok(Some(text))
            }
            Err(err) => Err(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text: err.user_message(),
            }),
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

    fn apply_global_search_output(&mut self, output: GlobalSearchOutput) {
        for channel in output.load_older_requests {
            let oldest_ts = self
                .state
                .channels
                .get(&channel)
                .and_then(|s| s.messages.front())
                .map(|m| m.timestamp.timestamp_millis())
                .unwrap_or(i64::MAX);
            self.request_older_local_history(&channel, oldest_ts);
        }
        if let Some((channel, msg_id)) = output.jump_to {
            self.jump_to_message(channel, msg_id);
        }
    }

    fn jump_to_message(&mut self, channel: ChannelId, msg_id: MessageId) {
        let still_present = self
            .state
            .channels
            .get(&channel)
            .map(|s| s.messages.iter().any(|m| m.id == msg_id))
            .unwrap_or(false);
        if !still_present {
            self.push_event_toast("Message no longer in buffer".to_string(), t::red(), false);
            tracing::warn!(
                "global search jump target evicted from buffer: {:?}",
                channel
            );
            return;
        }
        self.state.active_channel = Some(channel.clone());
        self.pending_scroll_to_message.insert(channel, msg_id);
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
                let btn_h = t::bar_h();
                let name_galley = ui.painter().layout_no_wrap(
                    display_name.clone(),
                    t::topbar_font(),
                    t::text_primary(),
                );
                let pill_w_max = (230.0 * t::font_scale()).max(btn_h + name_galley.size().x + 40.0);
                let pill_w =
                    (btn_h + 6.0 + name_galley.size().x + 10.0).clamp(btn_h + 28.0, pill_w_max);
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(pill_w, btn_h), egui::Sense::click());
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
                        ui.painter()
                            .circle_filled(avatar_c, avatar_r, t::bg_raised());
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
                        ui.painter()
                            .circle_filled(avatar_c, avatar_r, t::accent_dim());
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
                        t::topbar_font(),
                        t::text_primary(),
                    );
                }

                if resp.clicked() {
                    self.login_dialog.toggle();
                }
            }
        } else {
            let login_label = if compact_account {
                ""
            } else if self.state.accounts.is_empty() {
                "Log in"
            } else {
                "Accounts"
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
            } else {
                let label_galley = ui.painter().layout_no_wrap(
                    login_label.to_owned(),
                    t::topbar_font(),
                    t::text_primary(),
                );
                let login_w = (label_galley.size().x + 24.0).max(68.0);
                if ui
                    .add_sized(
                        [login_w, t::bar_h()],
                        egui::Button::new(RichText::new(login_label).font(t::topbar_font())),
                    )
                    .on_hover_text("Log in with a Twitch OAuth token")
                    .clicked()
                {
                    self.login_dialog.toggle();
                }
            }
        }
    }

    /// Push the chat and UI font sizes into the active egui context.
    fn apply_font_scale(&mut self, ctx: &Context) {
        // Keep globals in sync with struct state (handles first frame + external updates).
        let chat = t::set_chat_font_size(self.chat_font_size);
        let ui = t::set_ui_font_size(self.ui_font_size);
        self.chat_font_size = chat;
        self.ui_font_size = ui;

        if (ui - self.applied_pixels_per_point).abs() > f32::EPSILON {
            ctx.set_pixels_per_point(ui);
            self.applied_pixels_per_point = ui;
        }
    }

    /// Persist the current font sizes to settings via the runtime.
    fn send_font_sizes(&self) {
        self.send_cmd(AppCommand::SetFontSizes {
            chat_font_size: self.chat_font_size,
            ui_font_size: self.ui_font_size,
            topbar_font_size: self.topbar_font_size,
            tabs_font_size: self.tabs_font_size,
            timestamps_font_size: self.timestamps_font_size,
            pills_font_size: self.pills_font_size,
            popups_font_size: self.popups_font_size,
            chips_font_size: self.chips_font_size,
            usercard_font_size: self.usercard_font_size,
            dialog_font_size: self.dialog_font_size,
        });
    }

    /// Handle Ctrl+= / Ctrl+- / Ctrl+0 and Ctrl+scroll for chat-font zoom.
    fn handle_font_zoom_shortcuts(&mut self, ctx: &Context) {
        let zoom_in = self.hotkey_bindings.get(crust_core::HotkeyAction::ZoomIn);
        // Also accept Ctrl+Plus as a synonym when the default Ctrl+= is in
        // effect - on many keyboard layouts Plus and Equals share a key
        // and egui routes them differently.
        let accept_plus_alias = zoom_in
            == crust_core::KeyBinding::new("Equals").with_ctrl();
        let zoom_out = self.hotkey_bindings.get(crust_core::HotkeyAction::ZoomOut);
        let zoom_reset = self.hotkey_bindings.get(crust_core::HotkeyAction::ZoomReset);
        let (grow, shrink, reset, zoom_delta) = ctx.input_mut(|i| {
            let mut grow = consume_binding(i, &zoom_in);
            if accept_plus_alias
                && i.consume_key(egui::Modifiers::CTRL, egui::Key::Plus)
            {
                grow = true;
            }
            let shrink = consume_binding(i, &zoom_out);
            let reset = consume_binding(i, &zoom_reset);
            // egui collapses Ctrl+wheel into Zoom events. Drain them here so
            // they adjust chat font instead of the (unused) ctx pixels_per_point
            // zoom pipeline.
            let mut zoom_delta = 1.0_f32;
            i.events.retain(|evt| match evt {
                egui::Event::Zoom(factor) => {
                    zoom_delta *= *factor;
                    false
                }
                egui::Event::MouseWheel {
                    modifiers, delta, ..
                } if modifiers.ctrl => {
                    // Some backends still emit MouseWheel under Ctrl; treat as
                    // zoom and drop so ScrollArea doesn't scroll too.
                    let step = delta.y.clamp(-3.0, 3.0);
                    zoom_delta *= (1.1_f32).powf(step);
                    false
                }
                _ => true,
            });
            (grow, shrink, reset, zoom_delta)
        });

        let mut next = self.chat_font_size;
        let mut changed = false;
        if grow {
            next += 1.0;
            changed = true;
        }
        if shrink {
            next -= 1.0;
            changed = true;
        }
        if reset {
            next = t::DEFAULT_CHAT_FONT_SIZE;
            changed = true;
        }
        if (zoom_delta - 1.0).abs() > 0.001 && zoom_delta.is_finite() && zoom_delta > 0.0 {
            // Each wheel notch gives ~1.1 / 0.909. Convert to pts per notch,
            // then cap per-frame delta so a fast wheel gesture can't blow
            // past the clamp range in one frame.
            let pts = (zoom_delta.ln() / (1.1_f32).ln()).clamp(-2.0, 2.0);
            if pts.abs() > 0.05 {
                next += pts;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let clamped = next.clamp(t::MIN_CHAT_FONT_SIZE, t::MAX_CHAT_FONT_SIZE);
        if (clamped - self.chat_font_size).abs() < 0.01 {
            return;
        }
        self.chat_font_size = clamped;
        t::set_chat_font_size(clamped);
        self.send_font_sizes();
        ctx.request_repaint();
    }

    fn handle_search_shortcuts(&mut self, ctx: &Context) {
        // IMPORTANT: check the global (Ctrl+Shift+F by default) BEFORE the
        // per-channel search. egui's `consume_key` uses
        // `matches_logically`, so a binding like Ctrl+Shift+F satisfies a
        // plain Ctrl+F check. Run the more specific shortcut first.
        let toggle_global = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::ToggleGlobalSearch);
        let open_msg_search = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::OpenMessageSearch);
        let toggle_global_pressed =
            ctx.input_mut(|i| consume_binding(i, &toggle_global));
        if toggle_global_pressed {
            if self.global_search.open {
                self.global_search.close();
            } else {
                self.global_search.request_open();
            }
        }

        let open_search = ctx.input_mut(|i| consume_binding(i, &open_msg_search));
        // Escape is a fixed overlay-close gesture: not remappable because
        // too many overlays rely on it.
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

    fn open_channel_quick_switch(&mut self) {
        self.quick_switch.open = true;
        self.quick_switch.query.clear();
        self.quick_switch.selected = 0;
        self.quick_switch.focus_query = true;
    }

    fn close_channel_quick_switch(&mut self) {
        self.quick_switch.open = false;
        self.quick_switch.focus_query = false;
    }

    fn quick_switch_candidates(&self) -> Vec<QuickSwitchCandidate> {
        let query = self.quick_switch.query.trim().to_ascii_lowercase();
        let mut out: Vec<QuickSwitchCandidate> = Vec::new();

        for channel in &self.state.channel_order {
            if !channel_matches_query(channel, &query) {
                continue;
            }

            let (unread_count, unread_mentions) = self
                .state
                .channels
                .get(channel)
                .map(|state| (state.unread_count, state.unread_mentions))
                .unwrap_or((0, 0));
            let prefix = if channel.is_kick() || channel.is_irc_server_tab() {
                ""
            } else {
                "#"
            };
            let subtitle = if channel.is_kick() {
                Some("Kick channel".to_owned())
            } else if channel.is_irc() {
                Some("IRC target".to_owned())
            } else {
                Some("Twitch channel".to_owned())
            };

            out.push(QuickSwitchCandidate {
                entry: QuickSwitchEntry::Channel(channel.clone()),
                label: format!("{prefix}{}", channel.display_name()),
                subtitle,
                unread_count,
                unread_mentions,
            });
        }

        for login in &self.whisper_order {
            let display_name = self
                .whisper_display_names
                .get(login)
                .filter(|name| !name.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| login.clone());
            if !whisper_thread_matches_query(login, &display_name, &query) {
                continue;
            }

            out.push(QuickSwitchCandidate {
                entry: QuickSwitchEntry::WhisperThread {
                    login: login.clone(),
                },
                label: format!("@{display_name}"),
                subtitle: Some("Whisper thread".to_owned()),
                unread_count: self.whisper_unread.get(login).copied().unwrap_or(0),
                unread_mentions: self
                    .whisper_unread_mentions
                    .get(login)
                    .copied()
                    .unwrap_or(0),
            });
        }

        // Prioritize mentions, then unread, then everything else.
        out.sort_by_key(|candidate| {
            quick_switch_priority_bucket(candidate.unread_mentions, candidate.unread_count)
        });
        out
    }

    fn activate_quick_switch_entry(&mut self, entry: QuickSwitchEntry) {
        match entry {
            QuickSwitchEntry::Channel(channel) => self.activate_channel(channel),
            QuickSwitchEntry::WhisperThread { login } => self.activate_whisper_thread(&login),
        }
    }

    /// Returns true when the quick-switch palette is open/consuming hotkeys.
    fn handle_quick_switch_shortcuts(&mut self, ctx: &Context) -> bool {
        let open_binding = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::OpenQuickSwitcher);
        let open_requested = ctx.input_mut(|i| consume_binding(i, &open_binding));
        if open_requested {
            self.open_channel_quick_switch();
            return true;
        }

        if !self.quick_switch.open {
            return false;
        }

        let (close, up, down, submit) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });

        if close {
            self.close_channel_quick_switch();
            return true;
        }

        let candidates = self.quick_switch_candidates();
        if candidates.is_empty() {
            if submit {
                self.close_channel_quick_switch();
            }
            return true;
        }

        self.quick_switch.selected = self.quick_switch.selected.min(candidates.len() - 1);
        if up {
            self.quick_switch.selected = if self.quick_switch.selected == 0 {
                candidates.len() - 1
            } else {
                self.quick_switch.selected - 1
            };
        }
        if down {
            self.quick_switch.selected = (self.quick_switch.selected + 1) % candidates.len();
        }
        if submit {
            let target = candidates[self.quick_switch.selected].entry.clone();
            self.activate_quick_switch_entry(target);
            self.close_channel_quick_switch();
        }

        true
    }

    fn show_channel_quick_switch(&mut self, ctx: &Context) {
        if !self.quick_switch.open {
            return;
        }

        let mut activate: Option<QuickSwitchEntry> = None;
        let mut open = self.quick_switch.open;
        egui::Window::new("Quick Switch")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -36.0])
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                ui.label(
                    RichText::new("Find and switch channels or whisper threads")
                        .font(t::small())
                        .color(t::text_secondary()),
                );

                let input_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.quick_switch.query)
                        .desired_width(f32::INFINITY)
                        .hint_text(
                            "Type channel/login (supports twitch:, kick:, irc://, whisper:)",
                        ),
                );
                if self.quick_switch.focus_query {
                    input_resp.request_focus();
                    self.quick_switch.focus_query = false;
                }

                if input_resp.changed() {
                    self.quick_switch.selected = 0;
                }

                ui.add_space(6.0);
                ui.label(
                    RichText::new("Ctrl+K opens this palette, ↑/↓ navigates, Enter switches")
                        .font(t::tiny())
                        .color(t::text_muted()),
                );
                ui.add_space(6.0);

                let candidates = self.quick_switch_candidates();
                if candidates.is_empty() {
                    ui.label(
                        RichText::new("No channels match this query")
                            .font(t::small())
                            .color(t::text_muted()),
                    );
                    return;
                }

                self.quick_switch.selected = self.quick_switch.selected.min(candidates.len() - 1);
                let visible_rows = candidates.len().min(QUICK_SWITCH_MAX_ROWS);

                egui::ScrollArea::vertical()
                    .max_height(visible_rows as f32 * 28.0 + 8.0)
                    .show(ui, |ui| {
                        for (idx, candidate) in candidates.iter().enumerate() {
                            let is_selected = idx == self.quick_switch.selected;
                            let unread_count = candidate.unread_count;
                            let unread_mentions = candidate.unread_mentions;

                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 24.0),
                                egui::Sense::click(),
                            );
                            let row_fill = if is_selected {
                                t::tab_selected_bg()
                            } else if row_resp.hovered() {
                                t::tab_hover_bg()
                            } else {
                                t::bg_surface()
                            };
                            ui.painter().rect(
                                row_rect,
                                t::RADIUS_SM,
                                row_fill,
                                egui::Stroke::new(1.0, t::border_subtle()),
                                egui::StrokeKind::Middle,
                            );

                            let mut row_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(row_rect.shrink2(egui::vec2(8.0, 3.0)))
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            );
                            row_ui.label(
                                RichText::new(&candidate.label)
                                    .font(t::small())
                                    .color(if is_selected {
                                        t::text_primary()
                                    } else {
                                        t::text_secondary()
                                    })
                                    .strong(),
                            );
                            if let Some(subtitle) = candidate.subtitle.as_deref() {
                                row_ui.add_space(6.0);
                                row_ui.label(
                                    RichText::new(subtitle)
                                        .font(t::tiny())
                                        .color(t::text_muted()),
                                );
                            }
                            row_ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if unread_mentions > 0 {
                                        channel_tab_badge(
                                            ui,
                                            compact_badge_count(unread_mentions),
                                            t::text_primary(),
                                            t::mention_pill_bg(),
                                        );
                                    } else if unread_count > 0 {
                                        channel_tab_badge(
                                            ui,
                                            compact_badge_count(unread_count),
                                            t::text_secondary(),
                                            t::bg_raised(),
                                        );
                                    }
                                },
                            );

                            if row_resp.hovered() {
                                self.quick_switch.selected = idx;
                            }
                            if row_resp.clicked() {
                                activate = Some(candidate.entry.clone());
                            }
                        }
                    });
            });

        if !open {
            self.close_channel_quick_switch();
            return;
        }

        if let Some(entry) = activate {
            self.activate_quick_switch_entry(entry);
            self.close_channel_quick_switch();
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

    /// Known live state for `channel`, used by tab-visibility rules.
    ///
    /// Returns `Some(true)` / `Some(false)` when a stream-status snapshot
    /// exists for a Twitch channel, `None` when the status is unknown
    /// (pre-first-fetch, or non-Twitch channels where we don't track
    /// live-state). Callers should treat `None` as "keep visible" so a
    /// `hide_when_offline` tab never silently disappears before the
    /// first status probe returns.
    fn channel_live_status(&self, channel: &ChannelId) -> Option<bool> {
        if !channel.is_twitch() || channel.is_virtual() {
            return None;
        }
        self.live_map_cache
            .get(&channel.display_name().to_ascii_lowercase())
            .copied()
    }

    /// True when the tab for `channel` should be hidden by its configured
    /// visibility rule given the current live state. The active channel
    /// and virtual pins (Live / Mentions) are never reported hidden.
    fn tab_is_hidden(&self, channel: &ChannelId) -> bool {
        if channel.is_virtual() {
            return false;
        }
        self.state
            .is_tab_hidden(channel, self.channel_live_status(channel))
    }

    fn activate_channel(&mut self, channel: ChannelId) {
        if let Some(state) = self.state.channels.get_mut(&channel) {
            state.mark_read();
        }
        // Activating the Mentions pseudo-tab clears its unread counter.
        // (Real channels already clear via `mark_read` above; the Mentions
        // buffer lives separately on AppState.)
        if channel.is_mentions() {
            self.state.clear_mentions_unread();
        }
        // Whenever the user explicitly activates a channel we want the
        // MessageList to snap to the bottom and clear any stale "paused"
        // state, so the channel never opens into dead space above a frozen
        // scroll offset (the "channel is black until I click Resume
        // scrolling" bug).  This is the authoritative user-intent signal,
        // complementing the cumulative_pass_nr heuristic inside MessageList.
        let channel_changed = self
            .state
            .active_channel
            .as_ref()
            .map(|cur| cur != &channel)
            .unwrap_or(true);
        if channel_changed {
            self.pending_active_snap.insert(channel.clone());
            // Also write the force-snap flag directly to egui temp storage
            // so the MessageList that renders later in THIS same frame
            // picks it up immediately.  Without this, activation only
            // takes effect on the following frame (because the top-of-
            // update flush already ran before the user click was
            // processed), leaving a visible "black chat" transient or -
            // worse - losing the snap entirely if the stale paused
            // offset anchors below the viewport.
            if let Some(ctx) = self.egui_ctx.as_ref() {
                let key = egui::Id::new("ml_force_snap").with(channel.as_str());
                ctx.data_mut(|d| d.insert_temp(key, true));
                ctx.request_repaint();
            }
        }
        if !self.split_panes.panes.is_empty() {
            let focused = self.split_panes.focused;
            if let Some(pane) = self.split_panes.panes.get_mut(focused) {
                pane.channel = channel.clone();
                pane.input_buf.clear();
            }
        }
        self.state.active_channel = Some(channel.clone());
        self.persist_active_channel(&channel);
    }

    /// Persist the currently-focused channel so it can be restored next launch.
    fn persist_active_channel(&mut self, channel: &ChannelId) {
        if channel.is_virtual() {
            return; // Pseudo-tabs (Live / Mentions) can never be restored by id.
        }
        let key = channel.0.clone();
        if self.last_saved_active_channel.as_deref() == Some(key.as_str()) {
            return;
        }
        self.last_saved_active_channel = Some(key.clone());
        self.send_cmd(AppCommand::SetLastActiveChannel { channel: key });
    }

    /// If a previous-session channel is queued for restoration and it is now
    /// present in state, activate it.
    fn try_restore_pending_channel(&mut self) {
        let Some(key) = self.pending_restore_channel.clone() else {
            return;
        };
        let target = ChannelId(key);
        if target.is_virtual() {
            // Pseudo-tab sentinels can never appear in state.channels; clear
            // immediately so they don't permanently suppress persistence.
            self.pending_restore_channel = None;
            return;
        }
        if self.state.channels.contains_key(&target) {
            self.activate_channel(target);
            self.pending_restore_channel = None;
        }
    }

    /// Detect active-channel changes made through direct assignment and persist
    /// them without adding an activate_channel call at every site.
    fn reconcile_last_active_channel(&mut self) {
        // Don't overwrite the persisted entry while we're still waiting for
        // the previous-session channel to come online; otherwise a transient
        // "first channel in list" activation clobbers the user's choice.
        if self.pending_restore_channel.is_some() {
            return;
        }
        // Never persist any pseudo-tab sentinel (Live / Mentions) as the
        // last-active channel; restore would never find them anyway.
        if self
            .state
            .active_channel
            .as_ref()
            .map(|c| c.is_virtual())
            .unwrap_or(false)
        {
            return;
        }
        let current = self.state.active_channel.as_ref().map(|c| c.0.clone());
        if current.as_deref() == self.last_saved_active_channel.as_deref() {
            return;
        }
        if let Some(key) = current {
            self.last_saved_active_channel = Some(key.clone());
            self.send_cmd(AppCommand::SetLastActiveChannel { channel: key });
        }
    }

    /// Sample the OS window geometry once per frame; if it differs from the
    /// last persisted snapshot, debounce briefly then send a save command.
    fn sample_window_geometry(&mut self, ctx: &Context) {
        let info = ctx.input(|i| i.viewport().clone());
        let pos = info
            .outer_rect
            .map(|r| [r.min.x, r.min.y])
            .or_else(|| info.inner_rect.map(|r| [r.min.x, r.min.y]));
        let size = info.inner_rect.map(|r| [r.width(), r.height()]);
        let max = info.maximized.unwrap_or(false);

        let changed = pos != self.last_saved_window_pos
            || size != self.last_saved_window_size
            || max != self.last_saved_window_max;

        if changed {
            self.last_saved_window_pos = pos;
            self.last_saved_window_size = size;
            self.last_saved_window_max = max;
            self.window_geom_dirty_since = Some(std::time::Instant::now());
        }

        // Debounce: only flush after geometry has been stable for 750 ms to
        // avoid pounding settings.toml during a window drag/resize.
        if let Some(at) = self.window_geom_dirty_since {
            if at.elapsed() >= std::time::Duration::from_millis(750) {
                self.window_geom_dirty_since = None;
                self.send_cmd(AppCommand::SetWindowGeometry {
                    pos: self.last_saved_window_pos,
                    size: self.last_saved_window_size,
                    maximized: self.last_saved_window_max,
                });
            }
        }
    }

    /// Detect changes to channel order, split panes, and whispers panel state
    /// that bypassed direct setters; persist diffs without spamming the disk.
    fn reconcile_persistent_ui_state(&mut self) {
        // Channel order (skip while restoration is still pending).
        if self.pending_restore_channel_order.is_empty() {
            let order: Vec<String> = self
                .state
                .channel_order
                .iter()
                .filter(|c| !c.is_virtual())
                .map(|c| c.0.clone())
                .collect();
            if order != self.last_saved_channel_order {
                self.last_saved_channel_order = order.clone();
                self.send_cmd(AppCommand::SetChannelOrder { order });
            }
        }

        // Split panes (skip while restoration is still pending).
        if self.pending_restore_split_panes.is_empty() {
            let panes: Vec<(String, f32)> = self
                .split_panes
                .panes
                .iter()
                .filter(|p| !p.channel.is_virtual())
                .map(|p| (p.channel.0.clone(), p.frac))
                .collect();
            let focused = self.split_panes.focused;
            let panes_changed = panes.len() != self.last_saved_split_panes.len()
                || panes
                    .iter()
                    .zip(self.last_saved_split_panes.iter())
                    .any(|(a, b)| a.0 != b.0 || (a.1 - b.1).abs() > 0.001);
            if panes_changed || focused != self.last_saved_split_focused {
                self.last_saved_split_panes = panes.clone();
                self.last_saved_split_focused = focused;
                self.send_cmd(AppCommand::SetSplitPanes { panes, focused });
            }
        }

        // Whispers panel.
        let active_login = self.active_whisper_login.clone().unwrap_or_default();
        if self.whispers_visible != self.last_saved_whispers_visible
            || active_login != self.last_saved_whisper_login
        {
            self.last_saved_whispers_visible = self.whispers_visible;
            self.last_saved_whisper_login = active_login.clone();
            self.send_cmd(AppCommand::SetWhispersPanel {
                visible: self.whispers_visible,
                active_login,
            });
        }
    }

    /// Apply the previous session's channel order and split-pane layout once
    /// the relevant channels have finished joining.
    fn try_restore_pending_layout(&mut self) {
        // Channel order: reorder once every persisted channel has joined.
        if !self.pending_restore_channel_order.is_empty() {
            let mut still_missing = false;
            for key in &self.pending_restore_channel_order {
                if !self.state.channels.contains_key(&ChannelId(key.clone())) {
                    still_missing = true;
                    break;
                }
            }
            if !still_missing {
                let mut new_order: Vec<ChannelId> = Vec::new();
                for key in &self.pending_restore_channel_order {
                    new_order.push(ChannelId(key.clone()));
                }
                // Append any extra channels (joined since the snapshot) at the end
                // in their current relative order.
                for ch in &self.state.channel_order {
                    if !new_order.contains(ch) {
                        new_order.push(ch.clone());
                    }
                }
                self.state.channel_order = new_order.clone();
                self.last_saved_channel_order = new_order
                    .iter()
                    .filter(|c| !c.is_virtual())
                    .map(|c| c.0.clone())
                    .collect();
                self.pending_restore_channel_order.clear();
            }
        }

        // Split panes: rebuild once every referenced channel has joined.
        if !self.pending_restore_split_panes.is_empty() {
            let mut all_present = true;
            for (key, _) in &self.pending_restore_split_panes {
                if !self.state.channels.contains_key(&ChannelId(key.clone())) {
                    all_present = false;
                    break;
                }
            }
            if all_present {
                self.split_panes.panes.clear();
                for (key, frac) in &self.pending_restore_split_panes {
                    let ch = ChannelId(key.clone());
                    self.split_panes.add_pane(ch, None);
                    if let Some(p) = self.split_panes.panes.last_mut() {
                        p.frac = *frac;
                    }
                }
                self.split_panes.normalize_fractions();
                self.split_panes.focused = self
                    .pending_restore_split_focused
                    .min(self.split_panes.panes.len().saturating_sub(1));
                self.last_saved_split_panes = self
                    .split_panes
                    .panes
                    .iter()
                    .map(|p| (p.channel.0.clone(), p.frac))
                    .collect();
                self.last_saved_split_focused = self.split_panes.focused;
                self.pending_restore_split_panes.clear();
            }
        }
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
        use crust_core::HotkeyAction;
        let next_b = self.hotkey_bindings.get(HotkeyAction::NextTab);
        let prev_b = self.hotkey_bindings.get(HotkeyAction::PrevTab);
        let move_left_b = self.hotkey_bindings.get(HotkeyAction::MoveTabLeft);
        let move_right_b = self.hotkey_bindings.get(HotkeyAction::MoveTabRight);
        let first_b = self.hotkey_bindings.get(HotkeyAction::FirstTab);
        let last_b = self.hotkey_bindings.get(HotkeyAction::LastTab);
        let split_prev_b = self.hotkey_bindings.get(HotkeyAction::SplitFocusPrev);
        let split_next_b = self.hotkey_bindings.get(HotkeyAction::SplitFocusNext);
        let split_move_left_b = self.hotkey_bindings.get(HotkeyAction::SplitMoveLeft);
        let split_move_right_b = self.hotkey_bindings.get(HotkeyAction::SplitMoveRight);
        let tab_slots = [
            self.hotkey_bindings.get(HotkeyAction::SelectTab1),
            self.hotkey_bindings.get(HotkeyAction::SelectTab2),
            self.hotkey_bindings.get(HotkeyAction::SelectTab3),
            self.hotkey_bindings.get(HotkeyAction::SelectTab4),
            self.hotkey_bindings.get(HotkeyAction::SelectTab5),
            self.hotkey_bindings.get(HotkeyAction::SelectTab6),
            self.hotkey_bindings.get(HotkeyAction::SelectTab7),
            self.hotkey_bindings.get(HotkeyAction::SelectTab8),
            self.hotkey_bindings.get(HotkeyAction::SelectTab9),
        ];

        // Legacy aliases: users who haven't rebound get the full Chatterino
        // Ctrl+Tab / Ctrl+PageDown / Alt+Right set for free.  Once they
        // customize the binding, only their chosen shortcut fires.
        let defaults = crust_core::HotkeyBindings::defaults();
        let next_legacy = next_b == defaults.get(HotkeyAction::NextTab);
        let prev_legacy = prev_b == defaults.get(HotkeyAction::PrevTab);

        let (
            next,
            prev,
            direct_idx,
            move_left,
            move_right,
            first,
            last,
            split_prev,
            split_next,
            split_move_left,
            split_move_right,
        ) = ctx.input_mut(|i| {
            let mut next = consume_binding(i, &next_b);
            if next_legacy {
                if i.consume_key(egui::Modifiers::CTRL, egui::Key::PageDown)
                    || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowRight)
                {
                    next = true;
                }
            }
            let mut prev = consume_binding(i, &prev_b);
            if prev_legacy {
                if i.consume_key(egui::Modifiers::CTRL, egui::Key::PageUp)
                    || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowLeft)
                {
                    prev = true;
                }
            }
            let move_left = consume_binding(i, &move_left_b);
            let move_right = consume_binding(i, &move_right_b);
            let first = consume_binding(i, &first_b);
            let last = consume_binding(i, &last_b);
            let split_prev = consume_binding(i, &split_prev_b);
            let split_next = consume_binding(i, &split_next_b);
            let split_move_left = consume_binding(i, &split_move_left_b);
            let split_move_right = consume_binding(i, &split_move_right_b);
            let direct_idx = tab_slots
                .iter()
                .position(|binding| consume_binding(i, binding));
            (
                next,
                prev,
                direct_idx,
                move_left,
                move_right,
                first,
                last,
                split_prev,
                split_next,
                split_move_left,
                split_move_right,
            )
        });

        if let Some(idx) = direct_idx {
            if let Some(target) = self.state.channel_order.get(idx).cloned() {
                self.activate_channel(target);
            }
            return;
        }

        let split_mode = self.split_panes.panes.len() > 1;

        if move_left {
            if split_mode {
                self.split_panes.move_focused(-1);
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            } else if let Some(active) = self.state.active_channel.clone() {
                if let Some(idx) = self.state.channel_order.iter().position(|ch| ch == &active) {
                    if idx > 0 {
                        self.state.channel_order.swap(idx, idx - 1);
                    }
                }
            }
        }
        if move_right {
            if split_mode {
                self.split_panes.move_focused(1);
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            } else if let Some(active) = self.state.active_channel.clone() {
                if let Some(idx) = self.state.channel_order.iter().position(|ch| ch == &active) {
                    if idx + 1 < self.state.channel_order.len() {
                        self.state.channel_order.swap(idx, idx + 1);
                    }
                }
            }
        }
        if first {
            if split_mode {
                self.split_panes.focus_first();
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            } else if let Some(target) = self.state.channel_order.first().cloned() {
                self.activate_channel(target);
            }
            return;
        }
        if last {
            if split_mode {
                self.split_panes.focus_last();
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            } else if let Some(target) = self.state.channel_order.last().cloned() {
                self.activate_channel(target);
            }
            return;
        }

        if split_prev {
            if split_mode {
                self.split_panes.focus_prev();
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            }
            return;
        }
        if split_next {
            if split_mode {
                self.split_panes.focus_next();
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            }
            return;
        }
        if split_move_left {
            if split_mode {
                self.split_panes.move_focused(-1);
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
            }
            return;
        }
        if split_move_right {
            if split_mode {
                self.split_panes.move_focused(1);
                if let Some(ch) = self.split_panes.focused_channel().cloned() {
                    self.state.active_channel = Some(ch);
                }
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

    /// Dispatch `AppCommand::FetchImage` for every new thumbnail URL in a
    /// live-feed snapshot, deduplicating via `live_feed_pending_thumbnails`
    /// and the existing `emote_bytes` cache.
    fn request_live_feed_thumbnails(
        &mut self,
        channels: &[crust_core::model::LiveChannelSnapshot],
    ) {
        for snap in channels {
            if snap.thumbnail_url.is_empty() {
                continue;
            }
            if self.emote_bytes.contains_key(&snap.thumbnail_url) {
                continue;
            }
            if self
                .live_feed_pending_thumbnails
                .insert(snap.thumbnail_url.clone())
            {
                let _ = self
                    .cmd_tx
                    .try_send(crust_core::events::AppCommand::FetchImage {
                        url: snap.thumbnail_url.clone(),
                    });
            }
        }
    }

    /// Apply a `LiveFeedAction` produced by the LiveFeed widget.
    fn handle_live_feed_action(&mut self, action: crate::widgets::live_feed::LiveFeedAction) {
        use crate::widgets::live_feed::LiveFeedAction;
        match action {
            LiveFeedAction::OpenChannel(login) => {
                let id = crust_core::ChannelId::new(&login);
                if self.state.channel_order.contains(&id) {
                    self.state.active_channel = Some(id);
                } else {
                    // Navigate to the new tab immediately; the JoinChannel
                    // command's ChannelJoined event arrives later but won't
                    // change `active_channel` since join_channel only sets it
                    // when active_channel is None.
                    self.state.active_channel = Some(id.clone());
                    let _ = self
                        .cmd_tx
                        .try_send(crust_core::events::AppCommand::JoinChannel { channel: id });
                }
            }
            LiveFeedAction::Refresh => {
                let _ = self
                    .cmd_tx
                    .try_send(crust_core::events::AppCommand::LiveFeedRefresh);
            }
            LiveFeedAction::OpenStreamlink(login) => {
                let _ = self
                    .cmd_tx
                    .try_send(crust_core::events::AppCommand::OpenStreamlink { channel: login });
            }
            LiveFeedAction::OpenInPlayer(login) => {
                let _ = self
                    .cmd_tx
                    .try_send(crust_core::events::AppCommand::OpenPlayer { channel: login });
            }
        }
    }

    /// Handle a Mentions-tab row / pill click: switch to the source channel
    /// and schedule a scroll-to-message for the next frame. If the source
    /// channel is no longer joined, we silently no-op - the mention row
    /// itself already contains the full message text so the user has not
    /// lost information.
    fn jump_to_mention(&mut self, target: crate::widgets::mentions::MentionJumpTarget) {
        if !self.state.channels.contains_key(&target.channel) {
            return;
        }
        // Queue the scroll-to BEFORE activating - activate_channel goes
        // through the normal focus pipeline and next frame the chat renderer
        // will consume `pending_scroll_to_message`.
        self.pending_scroll_to_message
            .insert(target.channel.clone(), target.message);
        self.activate_channel(target.channel);
    }
}

// eframe::App implementation

impl eframe::App for CrustApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Slow-frame tracing: start a timer for every update() call and warn
        // loudly when a single frame takes longer than the threshold so we
        // can pinpoint hitches when users report them.  Set
        // `CRUST_SLOW_FRAME_MS` to an integer (default 100ms) to tune, or a
        // value over 10000 to disable.
        let frame_start = std::time::Instant::now();

        // Keep a cloned ctx handle so non-UI code paths (e.g. activate_channel
        // when triggered by events) can write to egui temp storage and
        // request repaints directly.  egui::Context is internally Arc-based
        // so cloning is cheap.
        if self.egui_ctx.is_none() {
            self.egui_ctx = Some(ctx.clone());
        }

        if self.auth_refresh_inflight
            && self
                .last_auth_refresh_attempt
                .map(|t| t.elapsed() >= AUTH_REFRESH_INFLIGHT_TIMEOUT)
                .unwrap_or(false)
        {
            self.auth_refresh_inflight = false;
        }

        // Rebuild any chatter-lists that were marked dirty since the last
        // paint (one sort per dirty channel, regardless of how many new
        // chatters arrived).  Throttled to at most once every 500 ms so a
        // continuously-busy channel doesn't eat CPU on every paint.
        const CHATTER_REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        if !self.chatters_dirty.is_empty()
            && self
                .chatters_last_rebuild
                .map(|t| t.elapsed() >= CHATTER_REBUILD_INTERVAL)
                .unwrap_or(true)
        {
            let dirty: Vec<ChannelId> = self.chatters_dirty.drain().collect();
            for channel in dirty {
                if let Some(ch) = self.state.channels.get(&channel) {
                    let sorted = sorted_chatters_vec(&ch.chatters);
                    self.sorted_chatters.insert(channel, sorted);
                }
            }
            self.chatters_last_rebuild = Some(std::time::Instant::now());
        }

        self.apply_font_scale(ctx);
        self.try_restore_pending_channel();
        self.try_restore_pending_layout();
        self.reconcile_last_active_channel();
        self.sample_window_geometry(ctx);
        self.reconcile_persistent_ui_state();

        // Detect channel activation that bypassed `activate_channel`
        // (keyboard shortcuts, pane focus changes, split-pane swaps, etc.)
        // by diffing the set of currently-visible channels against the
        // previous frame's snapshot.  Any channel that wasn't visible
        // last frame but is visible this frame counts as a fresh
        // activation and is queued for snap-to-bottom.
        let mut current_visible: std::collections::HashSet<ChannelId> =
            std::collections::HashSet::new();
        if let Some(ref ch) = self.state.active_channel {
            current_visible.insert(ch.clone());
        }
        for pane in &self.split_panes.panes {
            current_visible.insert(pane.channel.clone());
        }
        for ch in &current_visible {
            if !self.prev_frame_visible_channels.contains(ch) {
                self.pending_active_snap.insert(ch.clone());
            }
        }
        self.prev_frame_visible_channels = current_visible;

        // Surface any queued "user just activated this channel" signals to
        // egui temp storage so MessageList::show can consume them this
        // frame and force a snap-to-bottom regardless of stale paused
        // state.  Drained on read inside MessageList.
        if !self.pending_active_snap.is_empty() {
            let queued: Vec<ChannelId> = self.pending_active_snap.drain().collect();
            ctx.data_mut(|d| {
                for ch in queued {
                    let key = egui::Id::new("ml_force_snap").with(ch.as_str());
                    d.insert_temp(key, true);
                }
            });
        }

        let quick_switch_consumed = self.handle_quick_switch_shortcuts(ctx);
        if !quick_switch_consumed {
            self.handle_channel_shortcuts(ctx);
            self.handle_search_shortcuts(ctx);
            self.handle_font_zoom_shortcuts(ctx);
        }

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

        // Evict hype-train / raid banners whose phase has ended so the UI
        // stops drawing them once the cooldown elapses.  Cheap: both sweeps
        // iterate per-channel HashMaps which are tiny in practice.
        let now_instant = std::time::Instant::now();
        self.state.expire_stale_hype_trains(
            now_instant,
            std::time::Duration::from_secs(60),
        );
        self.state.expire_stale_raid_banners(
            now_instant,
            std::time::Duration::from_secs(60),
        );

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
        if self.last_active_stream_refresh.elapsed() >= STREAM_REFRESH_SCAN_INTERVAL {
            self.last_active_stream_refresh = std::time::Instant::now();
            let active_login = self
                .state
                .active_channel
                .as_ref()
                .filter(|ch| ch.is_twitch())
                .map(|ch| ch.display_name().to_ascii_lowercase());
            if let Some(login) = active_login {
                let refresh_interval = self.stream_refresh_interval_for(&login, true);
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= refresh_interval)
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
                let refresh_interval = self.stream_refresh_interval_for(&login, false);
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= refresh_interval)
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
                let refresh_interval = self.stream_refresh_interval_for(&login, false);
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= refresh_interval)
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
        for action in self.user_profile_popup.show(
            ctx,
            &self.emote_bytes,
            &self.stv_avatars,
            &self.mod_action_presets,
        ) {
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
                PopupAction::Warn {
                    channel,
                    login,
                    user_id,
                    reason,
                } => {
                    self.send_cmd(AppCommand::WarnUser {
                        channel,
                        login,
                        user_id,
                        reason,
                    });
                }
                PopupAction::Monitor {
                    channel,
                    login,
                    user_id,
                } => {
                    self.send_cmd(AppCommand::SetSuspiciousUser {
                        channel,
                        login,
                        user_id,
                        restricted: false,
                    });
                }
                PopupAction::Restrict {
                    channel,
                    login,
                    user_id,
                } => {
                    self.send_cmd(AppCommand::SetSuspiciousUser {
                        channel,
                        login,
                        user_id,
                        restricted: true,
                    });
                }
                PopupAction::Unmonitor {
                    channel,
                    login,
                    user_id,
                } => {
                    self.send_cmd(AppCommand::ClearSuspiciousUser {
                        channel,
                        login,
                        user_id,
                    });
                }
                PopupAction::Unrestrict {
                    channel,
                    login,
                    user_id,
                } => {
                    self.send_cmd(AppCommand::ClearSuspiciousUser {
                        channel,
                        login,
                        user_id,
                    });
                }
                PopupAction::ClearUserMessagesLocally { channel, login } => {
                    self.send_cmd(AppCommand::ClearUserMessagesLocally { channel, login });
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

        // Dialogs
        self.show_channel_quick_switch(ctx);

        // Surface recovered crash reports (if any) from the previous run.
        // The viewer auto-opens when reports are installed and closes
        // once the user dismisses or deletes them all.
        self.crash_viewer.show(ctx);

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

        let plugin_ui_snapshot = plugin_host()
            .map(|host| {
                host.set_current_channel(self.active_search_target());
                host.plugin_ui_snapshot()
            })
            .unwrap_or_default();
        self.plugin_ui_session
            .prune_missing_surfaces(&plugin_ui_snapshot);

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
                chat_font_size: self.chat_font_size,
                ui_font_size: self.ui_font_size,
                topbar_font_size: self.topbar_font_size,
                tabs_font_size: self.tabs_font_size,
                timestamps_font_size: self.timestamps_font_size,
                pills_font_size: self.pills_font_size,
                popups_font_size: self.popups_font_size,
                chips_font_size: self.chips_font_size,
                usercard_font_size: self.usercard_font_size,
                dialog_font_size: self.dialog_font_size,
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
                update_checks_enabled: self.update_checks_enabled,
                updater_last_checked_at: self.updater_last_checked_at.clone(),
                updater_skipped_version: self.updater_skipped_version.clone(),
                updater_available_version: self.updater_available_version.clone(),
                updater_available_asset: self.updater_available_asset.clone(),
                updater_available_release_url: self.updater_available_release_url.clone(),
                updater_install_inflight: self.updater_install_inflight,
                request_update_check_now: false,
                request_update_install_now: false,
                request_skip_available_update: false,
                request_open_available_release: false,
                request_test_gifted_sub_alert: false,
                highlight_rules: self.settings_highlight_rules.clone(),
                highlight_rule_bufs: self.settings_highlight_rule_bufs.clone(),
                filter_records: self.settings_filter_records.clone(),
                filter_record_bufs: self.settings_filter_record_bufs.clone(),
                mod_action_presets: self.settings_mod_action_presets.clone(),
                nicknames: self.settings_nicknames.clone(),
                ignored_users: self.settings_ignored_users.clone(),
                ignored_phrases: self.settings_ignored_phrases.clone(),
                command_aliases: self.settings_command_aliases.clone(),
                hotkey_bindings: self.settings_hotkey_bindings.clone(),
                hotkey_capture_target: self.hotkey_capture_target,
                show_pronouns_in_usercard: self.show_pronouns_in_usercard,
                auto_claim_bonus_points: self.auto_claim_bonus_points,
                twitch_webview_logged_in: self.twitch_webview_logged_in,
                twitch_sign_in_requested: false,
                plugin_ui: plugin_ui_snapshot.clone(),
                plugin_statuses: plugin_host()
                    .map(|host| host.plugin_statuses())
                    .unwrap_or_default(),
                plugin_reload_requested: false,
                streamer_mode: self.settings_streamer_mode.clone(),
                streamer_hide_link_previews: self.settings_streamer_hide_link_previews,
                streamer_hide_viewer_counts: self.settings_streamer_hide_viewer_counts,
                streamer_suppress_sounds: self.settings_streamer_suppress_sounds,
                streamer_mode_active: self.streamer_mode_active,
                external_streamlink_path: self.external_streamlink_path.clone(),
                external_streamlink_quality: self.external_streamlink_quality.clone(),
                external_streamlink_extra_args: self.external_streamlink_extra_args.clone(),
                external_player_template: self.external_player_template.clone(),
                external_mpv_path: self.external_mpv_path.clone(),
                external_streamlink_session_token: self.external_streamlink_session_token.clone(),
                spellcheck_enabled: self.spellcheck_enabled,
                spell_custom_dict: self.spell_custom_dict.clone(),
                spell_custom_dict_add_buf: self.spell_custom_dict_add_buf.clone(),
                sound_events: self.settings_sounds.clone(),
                sound_preview_request: None,
                filter_editor_modal: self.settings_filter_editor_modal.clone(),
            };
            let appearance_before = self.appearance_snapshot();
            let streamer_before = (
                self.settings_streamer_mode.clone(),
                self.settings_streamer_hide_link_previews,
                self.settings_streamer_hide_viewer_counts,
                self.settings_streamer_suppress_sounds,
            );
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
                &mut self.plugin_ui_session,
                stats,
            );

            if state.plugin_reload_requested {
                self.send_cmd(AppCommand::ReloadPlugins);
            }

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
            let font_changed = (state.chat_font_size - self.chat_font_size).abs() > 0.01
                || (state.ui_font_size - self.ui_font_size).abs() > 0.001
                || (state.topbar_font_size - self.topbar_font_size).abs() > 0.01
                || (state.tabs_font_size - self.tabs_font_size).abs() > 0.01
                || (state.timestamps_font_size - self.timestamps_font_size).abs() > 0.01
                || (state.pills_font_size - self.pills_font_size).abs() > 0.01
                || (state.popups_font_size - self.popups_font_size).abs() > 0.01
                || (state.chips_font_size - self.chips_font_size).abs() > 0.01
                || (state.usercard_font_size - self.usercard_font_size).abs() > 0.01
                || (state.dialog_font_size - self.dialog_font_size).abs() > 0.01;
            if font_changed {
                self.chat_font_size = t::set_chat_font_size(state.chat_font_size);
                self.ui_font_size = t::set_ui_font_size(state.ui_font_size);
                self.topbar_font_size = t::set_topbar_font_size(state.topbar_font_size);
                self.tabs_font_size = t::set_tabs_font_size(state.tabs_font_size);
                self.timestamps_font_size = t::set_timestamps_font_size(state.timestamps_font_size);
                self.pills_font_size = t::set_pills_font_size(state.pills_font_size);
                self.popups_font_size = t::set_popups_font_size(state.popups_font_size);
                self.chips_font_size = t::set_chips_font_size(state.chips_font_size);
                self.usercard_font_size = t::set_usercard_font_size(state.usercard_font_size);
                self.dialog_font_size = t::set_dialog_font_size(state.dialog_font_size);
                self.send_font_sizes();
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
            self.settings_filter_editor_modal = state.filter_editor_modal.clone();
            if state.mod_action_presets != self.settings_mod_action_presets {
                self.send_cmd(crust_core::events::AppCommand::SetModActionPresets {
                    presets: state.mod_action_presets.clone(),
                });
            }
            if state.nicknames != self.settings_nicknames {
                self.send_cmd(crust_core::events::AppCommand::SetNicknames {
                    nicknames: state.nicknames.clone(),
                });
            }
            if state.ignored_users != self.settings_ignored_users {
                self.send_cmd(crust_core::events::AppCommand::SetIgnoredUsers {
                    users: state.ignored_users.clone(),
                });
            }
            if state.ignored_phrases != self.settings_ignored_phrases {
                self.send_cmd(crust_core::events::AppCommand::SetIgnoredPhrases {
                    phrases: state.ignored_phrases.clone(),
                });
            }
            if state.command_aliases != self.settings_command_aliases {
                self.send_cmd(crust_core::events::AppCommand::SetCommandAliases {
                    aliases: state.command_aliases.clone(),
                });
            }
            self.hotkey_capture_target = state.hotkey_capture_target;
            if state.hotkey_bindings != self.settings_hotkey_bindings {
                self.settings_hotkey_bindings = state.hotkey_bindings.clone();
                // Update the live registry immediately so the change takes
                // effect without waiting for the runtime's round-trip.
                self.hotkey_bindings = state.hotkey_bindings.clone();
                self.send_cmd(crust_core::events::AppCommand::SetHotkeyBindings {
                    bindings: state.hotkey_bindings.to_pairs(),
                });
            }
            if state.show_pronouns_in_usercard != self.show_pronouns_in_usercard {
                self.show_pronouns_in_usercard = state.show_pronouns_in_usercard;
                self.send_cmd(crust_core::events::AppCommand::SetShowPronounsInUsercard {
                    enabled: state.show_pronouns_in_usercard,
                });
            }
            if state.auto_claim_bonus_points != self.auto_claim_bonus_points {
                self.auto_claim_bonus_points = state.auto_claim_bonus_points;
                self.send_cmd(crust_core::events::AppCommand::SetAutoClaimBonusPoints {
                    enabled: state.auto_claim_bonus_points,
                });
            }
            if state.twitch_sign_in_requested {
                self.send_cmd(AppCommand::OpenTwitchSignIn);
            }
            if state.desktop_notifications_enabled != self.desktop_notifications_enabled {
                self.desktop_notifications_enabled = state.desktop_notifications_enabled;
                self.send_cmd(AppCommand::SetNotificationSettings {
                    desktop_notifications_enabled: self.desktop_notifications_enabled,
                });
            }
            if let Some(event) = state.sound_preview_request {
                self.sound_controller.preview_event(event);
            }
            if state.sound_events != self.settings_sounds {
                let new_sounds = state.sound_events.clone().normalised();
                self.settings_sounds = new_sounds.clone();
                // Apply immediately so previews during the same
                // settings-page session use the latest values even
                // before the runtime round-trips a fresh snapshot.
                self.sound_controller.apply_settings(new_sounds.clone());
                self.send_cmd(AppCommand::SetSoundSettings {
                    events: new_sounds.to_pairs(),
                });
            }
            if state.spellcheck_enabled != self.spellcheck_enabled {
                self.spellcheck_enabled = state.spellcheck_enabled;
                crate::spellcheck::set_enabled(self.spellcheck_enabled);
                self.send_cmd(AppCommand::SetSpellcheckEnabled {
                    enabled: self.spellcheck_enabled,
                });
            }
            self.spell_custom_dict_add_buf = state.spell_custom_dict_add_buf.clone();
            if state.spell_custom_dict != self.spell_custom_dict {
                // Optimistically mirror locally so the UI reflects the edit
                // immediately. The runtime will echo back a sanitised list
                // via `AppEvent::SpellDictionaryUpdated`.
                self.spell_custom_dict = state.spell_custom_dict.clone();
                crate::spellcheck::set_user_dict(self.spell_custom_dict.iter().cloned());
                self.send_cmd(AppCommand::SetCustomSpellDictionary {
                    words: state.spell_custom_dict.clone(),
                });
            }
            let streamer_after = (
                state.streamer_mode.clone(),
                state.streamer_hide_link_previews,
                state.streamer_hide_viewer_counts,
                state.streamer_suppress_sounds,
            );
            if streamer_after != streamer_before {
                self.settings_streamer_mode = state.streamer_mode.clone();
                self.settings_streamer_hide_link_previews = state.streamer_hide_link_previews;
                self.settings_streamer_hide_viewer_counts = state.streamer_hide_viewer_counts;
                self.settings_streamer_suppress_sounds = state.streamer_suppress_sounds;
                self.send_cmd(AppCommand::SetStreamerModeSettings {
                    mode: state.streamer_mode.clone(),
                    hide_link_previews: state.streamer_hide_link_previews,
                    hide_viewer_counts: state.streamer_hide_viewer_counts,
                    suppress_sounds: state.streamer_suppress_sounds,
                });
            }
            if state.external_streamlink_path != self.external_streamlink_path
                || state.external_streamlink_quality != self.external_streamlink_quality
                || state.external_streamlink_extra_args != self.external_streamlink_extra_args
                || state.external_player_template != self.external_player_template
                || state.external_mpv_path != self.external_mpv_path
                || state.external_streamlink_session_token != self.external_streamlink_session_token
            {
                self.external_streamlink_path = state.external_streamlink_path.clone();
                self.external_streamlink_quality = state.external_streamlink_quality.clone();
                self.external_streamlink_extra_args = state.external_streamlink_extra_args.clone();
                self.external_player_template = state.external_player_template.clone();
                self.external_mpv_path = state.external_mpv_path.clone();
                self.external_streamlink_session_token =
                    state.external_streamlink_session_token.clone();
                self.send_cmd(AppCommand::SetExternalToolsSettings {
                    streamlink_path: state.external_streamlink_path.clone(),
                    streamlink_quality: state.external_streamlink_quality.clone(),
                    streamlink_extra_args: state.external_streamlink_extra_args.clone(),
                    player_template: state.external_player_template.clone(),
                    mpv_path: state.external_mpv_path.clone(),
                    streamlink_session_token: state.external_streamlink_session_token.clone(),
                });
            }
            if state.update_checks_enabled != self.update_checks_enabled {
                self.update_checks_enabled = state.update_checks_enabled;
                self.send_cmd(AppCommand::SetUpdateChecksEnabled {
                    enabled: self.update_checks_enabled,
                });
            }
            if state.request_update_check_now {
                self.send_cmd(AppCommand::CheckForUpdates { manual: true });
            }
            if state.request_update_install_now {
                self.send_cmd(AppCommand::InstallAvailableUpdate { restart_now: true });
            }
            if state.request_skip_available_update {
                if let Some(version) = self.updater_available_version.clone() {
                    self.send_cmd(AppCommand::SkipUpdateVersion {
                        version: version.clone(),
                    });
                    self.updater_skipped_version = version;
                    self.updater_available_version = None;
                    self.updater_available_asset = None;
                    self.updater_available_release_url = None;
                }
            }
            if state.request_open_available_release {
                if let Some(url) = self.updater_available_release_url.clone() {
                    self.send_cmd(AppCommand::OpenUrl { url });
                }
            }
            if state.request_test_gifted_sub_alert {
                self.trigger_test_gifted_sub_alert(ctx);
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
            self.render_mod_tools_window(ctx, moderation_channel);
        }

        show_plugin_windows(ctx, &plugin_ui_snapshot, &mut self.plugin_ui_session);

        self.show_whispers_window(ctx);

        // Top bar
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
        let whisper_unread_total = self.whisper_unread_total();

        TopBottomPanel::top("status_bar")
            .exact_height(responsive.status_bar_height)
            .frame(
                Frame::new()
                    .fill(t::bg_surface())
                    .inner_margin(Margin {
                        left: 10,
                        right: 10,
                        top: 4,
                        bottom: 1,
                    })
                    .stroke(egui::Stroke::new(1.0, t::border_subtle())),
            )
            .show(ctx, |ui| {
                let bar_width = ui.available_width();
                let visibility =
                    toolbar_visibility(bar_width, self.irc_beta_enabled, moderation_available);
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
                        // Scale button padding so ⋯ menu + wrapped menu items
                        // don't get overlapped by neighbours at large topbar fonts.
                        let tb_pad_x = (t::topbar_font_size() * 0.55).max(8.0);
                        let tb_pad_y = (t::topbar_font_size() * 0.3).max(4.0);
                        ui.spacing_mut().button_padding = egui::vec2(tb_pad_x, tb_pad_y);
                        ui.spacing_mut().interact_size.y = t::bar_h();

                        if visibility.show_logo {
                            let logo_font = egui::FontId::proportional(15.0 * t::font_scale());
                            ui.label(
                                RichText::new("crust")
                                    .font(logo_font)
                                    .strong()
                                    .color(t::accent()),
                            );
                        }

                        chrome::toolbar_group_frame().show(ui, |ui| {
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
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
                                                .font(t::topbar_font())
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
                                                .font(t::topbar_font())
                                                .color(t::text_secondary()),
                                        );
                                    }
                                },
                            );
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
                                                    self.set_channel_layout(ChannelLayout::Sidebar);
                                                }
                                                ChannelLayout::Sidebar => {
                                                    self.toggle_sidebar_visible();
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
                                                selected: effective_channel_layout
                                                    == ChannelLayout::TopTabs,
                                                compact: visibility.compact_controls,
                                                ..Default::default()
                                            },
                                        )
                                        .clicked()
                                        {
                                            if self.channel_layout == ChannelLayout::Sidebar {
                                                self.set_channel_layout(ChannelLayout::TopTabs);
                                            } else {
                                                self.set_channel_layout(ChannelLayout::Sidebar);
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
                                let sep_gap = (t::topbar_font_size() * 0.4).max(6.0);
                                ui.add_space(sep_gap);
                                ui.separator();
                                ui.add_space(sep_gap);
                            }

                            if visibility.show_overflow_menu {
                                ui.menu_button(RichText::new("⋯").font(t::topbar_font()), |ui| {
                                    if visibility.show_join_in_overflow
                                        && ui
                                            .button(
                                                RichText::new("Join channel")
                                                    .font(t::topbar_font()),
                                            )
                                            .clicked()
                                    {
                                        self.join_dialog.toggle();
                                        ui.close_menu();
                                    }

                                    if ui
                                        .button(
                                            RichText::new("Quick switch channel (Ctrl+K)")
                                                .font(t::topbar_font()),
                                        )
                                        .clicked()
                                    {
                                        self.open_channel_quick_switch();
                                        ui.close_menu();
                                    }

                                    if ui
                                        .button(RichText::new("Settings").font(t::topbar_font()))
                                        .clicked()
                                    {
                                        self.settings_open = true;
                                        ui.close_menu();
                                    }

                                    if visibility.show_mod_in_overflow
                                        && moderation_available
                                        && ui
                                            .button(
                                                RichText::new("Moderation tools")
                                                    .font(t::topbar_font()),
                                            )
                                            .clicked()
                                    {
                                        self.mod_tools_open = true;
                                        if let Some(channel) = moderation_channel_toolbar.clone() {
                                            self.send_cmd(AppCommand::FetchUnbanRequests {
                                                channel,
                                            });
                                        }
                                        ui.close_menu();
                                    }

                                    ui.separator();

                                    let sidebar_open = self.channel_layout
                                        == ChannelLayout::Sidebar
                                        && self.sidebar_visible;
                                    let sidebar_label = if sidebar_open {
                                        "Hide sidebar"
                                    } else {
                                        "Show sidebar"
                                    };
                                    if ui
                                        .button(RichText::new(sidebar_label).font(t::topbar_font()))
                                        .clicked()
                                    {
                                        match self.channel_layout {
                                            ChannelLayout::TopTabs => {
                                                self.set_channel_layout(ChannelLayout::Sidebar);
                                            }
                                            ChannelLayout::Sidebar => {
                                                self.toggle_sidebar_visible();
                                            }
                                        }
                                        ui.close_menu();
                                    }

                                    let mode_label =
                                        if self.channel_layout == ChannelLayout::Sidebar {
                                            "Use top tabs"
                                        } else {
                                            "Use sidebar"
                                        };
                                    if ui
                                        .button(RichText::new(mode_label).font(t::topbar_font()))
                                        .clicked()
                                    {
                                        if self.channel_layout == ChannelLayout::Sidebar {
                                            self.set_channel_layout(ChannelLayout::TopTabs);
                                        } else {
                                            self.set_channel_layout(ChannelLayout::Sidebar);
                                        }
                                        ui.close_menu();
                                    }

                                    ui.separator();

                                    if visibility.show_perf_in_overflow
                                        && ui
                                            .button(
                                                RichText::new("Perf overlay")
                                                    .font(t::topbar_font()),
                                            )
                                            .clicked()
                                    {
                                        self.perf.visible = !self.perf.visible;
                                        ui.close_menu();
                                    }

                                    if visibility.show_stats_in_overflow
                                        && ui
                                            .button(
                                                RichText::new("Analytics").font(t::topbar_font()),
                                            )
                                            .clicked()
                                    {
                                        self.toggle_analytics_visible();
                                        ui.close_menu();
                                    }

                                    if visibility.show_whispers_in_overflow {
                                        let whispers_label = if whisper_unread_total > 0 {
                                            format!("Whispers ({whisper_unread_total})")
                                        } else {
                                            "Whispers".to_owned()
                                        };
                                        if ui
                                            .button(
                                                RichText::new(whispers_label)
                                                    .font(t::topbar_font()),
                                            )
                                            .clicked()
                                        {
                                            self.whispers_visible = !self.whispers_visible;
                                            if self.whispers_visible {
                                                if let Some(active) = self
                                                    .active_whisper_login
                                                    .clone()
                                                    .or_else(|| self.whisper_order.first().cloned())
                                                {
                                                    self.active_whisper_login =
                                                        Some(active.clone());
                                                    self.mark_whisper_thread_read(&active);
                                                }
                                            }
                                            ui.close_menu();
                                        }
                                    }

                                    if visibility.show_irc_in_overflow
                                        && ui
                                            .button(
                                                RichText::new("IRC status").font(t::topbar_font()),
                                            )
                                            .clicked()
                                    {
                                        self.toggle_irc_status_visible();
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
                                        .font(t::topbar_font())
                                        .color(t::text_muted()),
                                );
                                ui.separator();
                            }

                            chrome::toolbar_group_frame().show(ui, |ui| {
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);
                                        if visibility.show_mod_button {
                                            let mod_button = ui.add_enabled(
                                                moderation_available,
                                                egui::Button::new(
                                                    RichText::new("Mod").font(t::tiny()),
                                                ),
                                            );
                                            if mod_button.clicked() {
                                                self.mod_tools_open = !self.mod_tools_open;
                                                if self.mod_tools_open {
                                                    if let Some(channel) =
                                                        moderation_channel_toolbar.clone()
                                                    {
                                                        self.send_cmd(
                                                            AppCommand::FetchUnbanRequests {
                                                                channel,
                                                            },
                                                        );
                                                    }
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
                                            self.toggle_analytics_visible();
                                        }
                                        if visibility.show_whispers_toggle {
                                            let tooltip = if whisper_unread_total > 0 {
                                                format!(
                                                    "Open whispers ({whisper_unread_total} unread)"
                                                )
                                            } else {
                                                "Open whispers".to_owned()
                                            };
                                            if chrome::icon_button(
                                                ui,
                                                ChromeIcon::Whisper,
                                                &tooltip,
                                                IconButtonState {
                                                    selected: self.whispers_visible,
                                                    compact: visibility.compact_controls,
                                                    ..Default::default()
                                                },
                                            )
                                            .clicked()
                                            {
                                                self.whispers_visible = !self.whispers_visible;
                                                if self.whispers_visible {
                                                    if let Some(active) =
                                                        self.active_whisper_login.clone().or_else(
                                                            || self.whisper_order.first().cloned(),
                                                        )
                                                    {
                                                        self.active_whisper_login =
                                                            Some(active.clone());
                                                        self.mark_whisper_thread_read(&active);
                                                    }
                                                }
                                            }
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
                                            self.toggle_irc_status_visible();
                                        }
                                    },
                                );
                            });
                        });
                    },
                );
            });

        show_channel_info_bars(
            ctx,
            &self.state,
            &self.stream_statuses,
            &self.channel_points,
            &plugin_ui_snapshot,
            &mut self.plugin_ui_session,
        );

        // Channel list: left sidebar OR top tab strip
        // Accumulate actions outside the panel closure so we can call &mut self
        // methods after the panel is done drawing.
        let mut ch_selected: Option<ChannelId> = None;
        let mut ch_closed: Option<ChannelId> = None;
        let mut ch_reordered: Option<Vec<ChannelId>> = None;
        let mut ch_drag_split: Option<ChannelId> = None;
        let mut show_split_drop_zone = false;
        let mut ch_open_streamlink: Option<ChannelId> = None;
        let mut ch_open_player: Option<ChannelId> = None;
        let mut ch_visibility_change: Option<(ChannelId, TabVisibilityRule)> = None;
        let mut ch_unhide_bulk: Vec<ChannelId> = Vec::new();

        match effective_channel_layout {
            // Top-tab strip
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
                                    // Pinned "Live" chip at the start of the
                                    // strip when logged-in. Mirrors the pinned
                                    // sentinel row in the sidebar layout.
                                    if self.state.auth.logged_in {
                                        let live_id = crust_core::ChannelId::live_feed();
                                        let is_active =
                                            self.state.active_channel.as_ref() == Some(&live_id);
                                        let (fg, bg) = if is_active {
                                            (t::text_primary(), t::tab_selected_bg())
                                        } else {
                                            (t::text_primary(), t::bg_surface())
                                        };
                                        let stroke = if is_active {
                                            egui::Stroke::new(1.0, t::border_accent())
                                        } else {
                                            egui::Stroke::new(1.0, t::border_subtle())
                                        };
                                        let count = self.state.live_channels.len();
                                        let tab_frame = egui::Frame::new()
                                            .fill(bg)
                                            .stroke(stroke)
                                            .corner_radius(t::RADIUS_SM)
                                            .inner_margin(egui::Margin::symmetric(
                                                tab_metrics.chip_pad_x,
                                                tab_metrics.chip_pad_y,
                                            ))
                                            .show(ui, |ui| {
                                                ui.set_height(tab_metrics.chip_height);
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 4.0;
                                                    ui.label(
                                                        RichText::new("●")
                                                            .font(t::tabs_font())
                                                            .color(t::red()),
                                                    );
                                                    ui.label(
                                                        RichText::new("Live")
                                                            .font(t::tabs_font())
                                                            .color(fg)
                                                            .strong(),
                                                    );
                                                    if count > 0 {
                                                        ui.label(
                                                            RichText::new(format!("({count})"))
                                                                .font(t::tabs_font())
                                                                .color(t::text_muted()),
                                                        );
                                                    }
                                                });
                                            });
                                        // Raw hit-test over the painted frame's
                                        // rect - this does NOT apply egui widget
                                        // visuals (no hover/active overlay on
                                        // top of the custom frame fill).
                                        let resp = ui.interact(
                                            tab_frame.response.rect,
                                            egui::Id::new("live_feed_tab_chip"),
                                            egui::Sense::click(),
                                        );
                                        if resp.clicked() {
                                            ch_selected = Some(live_id);
                                        }
                                    }
                                    // Pinned "Mentions" chip next to the Live
                                    // chip when logged in. Unread count takes
                                    // priority over total count (matches the
                                    // Live chip's viewer-count display rules).
                                    if self.state.auth.logged_in {
                                        let mentions_id = crust_core::ChannelId::mentions();
                                        let is_active =
                                            self.state.active_channel.as_ref() == Some(&mentions_id);
                                        let (fg, bg) = if is_active {
                                            (t::text_primary(), t::tab_selected_bg())
                                        } else {
                                            (t::text_primary(), t::bg_surface())
                                        };
                                        let stroke = if is_active {
                                            egui::Stroke::new(1.0, t::border_accent())
                                        } else {
                                            egui::Stroke::new(1.0, t::border_subtle())
                                        };
                                        let total = self.state.mentions.len();
                                        let unread = self.state.mentions_unread;
                                        let tab_frame = egui::Frame::new()
                                            .fill(bg)
                                            .stroke(stroke)
                                            .corner_radius(t::RADIUS_SM)
                                            .inner_margin(egui::Margin::symmetric(
                                                tab_metrics.chip_pad_x,
                                                tab_metrics.chip_pad_y,
                                            ))
                                            .show(ui, |ui| {
                                                ui.set_height(tab_metrics.chip_height);
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 4.0;
                                                    ui.label(
                                                        RichText::new("@")
                                                            .font(t::tabs_font())
                                                            .color(t::accent())
                                                            .strong(),
                                                    );
                                                    ui.label(
                                                        RichText::new("Mentions")
                                                            .font(t::tabs_font())
                                                            .color(fg)
                                                            .strong(),
                                                    );
                                                    if unread > 0 {
                                                        let label = if unread > 99 {
                                                            "99+".to_owned()
                                                        } else {
                                                            format!("{unread}")
                                                        };
                                                        egui::Frame::new()
                                                            .fill(t::mention_pill_bg())
                                                            .corner_radius(t::RADIUS_SM)
                                                            .inner_margin(egui::Margin::symmetric(
                                                                5, 0,
                                                            ))
                                                            .show(ui, |ui| {
                                                                ui.label(
                                                                    RichText::new(label)
                                                                        .font(t::tiny())
                                                                        .strong()
                                                                        .color(t::text_primary()),
                                                                );
                                                            });
                                                    } else if total > 0 {
                                                        ui.label(
                                                            RichText::new(format!("({total})"))
                                                                .font(t::tabs_font())
                                                                .color(t::text_muted()),
                                                        );
                                                    }
                                                });
                                            });
                                        let resp = ui.interact(
                                            tab_frame.response.rect,
                                            egui::Id::new("mentions_tab_chip"),
                                            egui::Sense::click(),
                                        );
                                        if resp.clicked() {
                                            ch_selected = Some(mentions_id);
                                        }
                                    }
                                    // Collect hidden channels up-front so we
                                    // can both skip them in the render loop
                                    // and surface them via the "N hidden"
                                    // escape-hatch chip at the end of the
                                    // strip. Without that chip an offline
                                    // channel with `hide_when_offline` has
                                    // no reachable right-click target -
                                    // quick-switch (Ctrl+K) still works, but
                                    // isn't discoverable enough.
                                    let hidden_channels: Vec<ChannelId> = self
                                        .state
                                        .channel_order
                                        .iter()
                                        .filter(|ch| self.tab_is_hidden(ch))
                                        .cloned()
                                        .collect();
                                    for ch in self.state.channel_order.iter() {
                                        if self.tab_is_hidden(ch) {
                                            continue;
                                        }
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
                                                                .font(t::tabs_font())
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
                                                                .font(t::tabs_font())
                                                                .color(fg),
                                                        )
                                                        .truncate(),
                                                    );

                                                    if mentions > 0 {
                                                        channel_tab_badge(
                                                            ui,
                                                            compact_badge_count(mentions),
                                                            t::text_primary(),
                                                            t::mention_pill_bg(),
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
                                                                        .font(t::tabs_font())
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

                                        // Drag tab downward -> split pane
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
                                                    t::split_success_bg()
                                                } else {
                                                    t::alpha(t::accent(), 200)
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
                                                    t::text_on_accent(),
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
                                                        t::split_success_text(),
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
                                            if ch.is_twitch() {
                                                ui.separator();
                                                if ui
                                                    .button(
                                                        RichText::new("Open in Streamlink")
                                                            .font(t::small()),
                                                    )
                                                    .clicked()
                                                {
                                                    ch_open_streamlink = Some(ch.clone());
                                                    ui.close_menu();
                                                }
                                                if ui
                                                    .button(
                                                        RichText::new("Open in player")
                                                            .font(t::small()),
                                                    )
                                                    .clicked()
                                                {
                                                    ch_open_player = Some(ch.clone());
                                                    ui.close_menu();
                                                }
                                                ui.separator();
                                                let mut hide_offline = self
                                                    .state
                                                    .tab_visibility_rule(ch)
                                                    == TabVisibilityRule::HideWhenOffline;
                                                if ui
                                                    .checkbox(
                                                        &mut hide_offline,
                                                        RichText::new("Hide when offline")
                                                            .font(t::small()),
                                                    )
                                                    .changed()
                                                {
                                                    let new_rule = if hide_offline {
                                                        TabVisibilityRule::HideWhenOffline
                                                    } else {
                                                        TabVisibilityRule::Always
                                                    };
                                                    ch_visibility_change =
                                                        Some((ch.clone(), new_rule));
                                                    ui.close_menu();
                                                }
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

                                    // "N hidden" escape-hatch chip. Lists
                                    // every tab currently suppressed by its
                                    // visibility rule and lets the user
                                    // jump to one (making the active-tab
                                    // exemption show it again) or clear
                                    // the rule outright.
                                    if !hidden_channels.is_empty() {
                                        ui.menu_button(
                                            RichText::new(format!(
                                                "⊘ {} hidden",
                                                hidden_channels.len()
                                            ))
                                            .font(t::tabs_font())
                                            .color(t::text_muted()),
                                            |ui| {
                                                ui.label(
                                                    RichText::new("Hidden tabs")
                                                        .font(t::small())
                                                        .strong()
                                                        .color(t::text_secondary()),
                                                );
                                                ui.separator();
                                                for ch in &hidden_channels {
                                                    let label = format!(
                                                        "#{}",
                                                        ch.display_name()
                                                    );
                                                    ui.horizontal(|ui| {
                                                        if ui
                                                            .button(
                                                                RichText::new(&label)
                                                                    .font(t::small()),
                                                            )
                                                            .on_hover_text(
                                                                "Switch to this channel",
                                                            )
                                                            .clicked()
                                                        {
                                                            ch_selected = Some(ch.clone());
                                                            ui.close_menu();
                                                        }
                                                        if ui
                                                            .button(
                                                                RichText::new("Unhide")
                                                                    .font(t::small()),
                                                            )
                                                            .on_hover_text(
                                                                "Clear the visibility rule for this channel",
                                                            )
                                                            .clicked()
                                                        {
                                                            ch_visibility_change = Some((
                                                                ch.clone(),
                                                                TabVisibilityRule::Always,
                                                            ));
                                                            ui.close_menu();
                                                        }
                                                    });
                                                }
                                                ui.separator();
                                                if ui
                                                    .button(
                                                        RichText::new("Unhide all")
                                                            .font(t::small()),
                                                    )
                                                    .clicked()
                                                {
                                                    // Accumulate for bulk
                                                    // application outside
                                                    // the panel closure -
                                                    // can't call `&mut self`
                                                    // methods here because
                                                    // the surrounding loop
                                                    // already borrows self.
                                                    ch_unhide_bulk =
                                                        hidden_channels.clone();
                                                    ui.close_menu();
                                                }
                                            },
                                        );
                                    }
                                });
                            });
                    });
            }

            // Left sidebar (default)
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
                        if has_host_panels_for_slot(
                            &plugin_ui_snapshot,
                            PluginUiHostSlot::SidebarTop,
                        ) {
                            render_host_panels_for_slot(
                                ui,
                                &plugin_ui_snapshot,
                                &mut self.plugin_ui_session,
                                PluginUiHostSlot::SidebarTop,
                            );
                            ui.add_space(8.0);
                        }
                        ui.label(
                            RichText::new("CHANNELS")
                                .font(t::heading())
                                .strong()
                                .color(t::text_muted()),
                        );
                        ui.add_space(4.0);
                        ui.add(egui::Separator::default().spacing(6.0));

                        // Respect per-tab visibility rules: channels whose
                        // rule + live-state combination says "hide" are
                        // omitted from the sidebar entirely (and from drag
                        // semantics). Hidden channels keep their slot in
                        // the underlying `channel_order` so they pop back
                        // in place the moment the state flips.
                        let mut visible_channels: Vec<ChannelId> =
                            Vec::with_capacity(self.state.channel_order.len());
                        let mut hidden_channels: Vec<ChannelId> = Vec::new();
                        for ch in self.state.channel_order.iter() {
                            if self.tab_is_hidden(ch) {
                                hidden_channels.push(ch.clone());
                            } else {
                                visible_channels.push(ch.clone());
                            }
                        }

                        // Bottom-docked "N hidden" footer. Rendered as a
                        // nested bottom panel BEFORE the channel list so
                        // egui reserves its strip up-front; the sidebar's
                        // ScrollArea uses `auto_shrink=[false; 2]` and
                        // would otherwise eat this space, pushing the
                        // footer off-screen.
                        if !hidden_channels.is_empty() {
                            egui::TopBottomPanel::bottom("sidebar_hidden_footer")
                                .resizable(false)
                                .frame(
                                    Frame::new()
                                        .fill(Color32::TRANSPARENT)
                                        .inner_margin(egui::Margin::symmetric(0, 4)),
                                )
                                .show_inside(ui, |ui| {
                                    // Full-width rail above the trigger that
                                    // matches the separator at the top of the
                                    // channel list (same `spacing(6.0)`) so
                                    // the two dividers frame the list as a
                                    // visual pair.
                                    ui.add(egui::Separator::default().spacing(6.0));
                                    // Render the trigger like a ghost channel
                                    // row so the `⊘` glyph lines up with the
                                    // `#` prefix in the rows above. Strip the
                                    // default rounded button chrome so the
                                    // trigger blends with the surrounding
                                    // rail/rows instead of floating in a pill
                                    // that clashes with the `CHANNELS`
                                    // section header.
                                    ui.scope(|ui| {
                                        {
                                            let style = ui.style_mut();
                                            style.spacing.button_padding =
                                                egui::vec2(8.0, 5.0);
                                            let widgets =
                                                &mut style.visuals.widgets;
                                            widgets.inactive.bg_fill =
                                                Color32::TRANSPARENT;
                                            widgets.inactive.weak_bg_fill =
                                                Color32::TRANSPARENT;
                                            widgets.inactive.bg_stroke =
                                                egui::Stroke::NONE;
                                            widgets.inactive.expansion = 0.0;
                                            widgets.hovered.bg_fill =
                                                t::hover_row_bg();
                                            widgets.hovered.weak_bg_fill =
                                                t::hover_row_bg();
                                            widgets.hovered.bg_stroke =
                                                egui::Stroke::NONE;
                                            widgets.hovered.expansion = 0.0;
                                            widgets.active.bg_fill =
                                                t::hover_row_bg();
                                            widgets.active.weak_bg_fill =
                                                t::hover_row_bg();
                                            widgets.active.bg_stroke =
                                                egui::Stroke::NONE;
                                            widgets.active.expansion = 0.0;
                                        }
                                    ui.menu_button(
                                        RichText::new(format!(
                                            "⊘ {} hidden channel{}",
                                            hidden_channels.len(),
                                            if hidden_channels.len() == 1 {
                                                ""
                                            } else {
                                                "s"
                                            }
                                        ))
                                        .font(t::small())
                                        .color(t::text_muted()),
                                        |ui| {
                                            ui.label(
                                                RichText::new("Hidden tabs")
                                                    .font(t::small())
                                                    .strong()
                                                    .color(t::text_secondary()),
                                            );
                                            ui.separator();
                                            for ch in &hidden_channels {
                                                ui.horizontal(|ui| {
                                                    if ui
                                                        .button(
                                                            RichText::new(format!(
                                                                "#{}",
                                                                ch.display_name()
                                                            ))
                                                            .font(t::small()),
                                                        )
                                                        .on_hover_text(
                                                            "Switch to this channel",
                                                        )
                                                        .clicked()
                                                    {
                                                        ch_selected = Some(ch.clone());
                                                        ui.close_menu();
                                                    }
                                                    if ui
                                                        .button(
                                                            RichText::new("Unhide")
                                                                .font(t::small()),
                                                        )
                                                        .on_hover_text(
                                                            "Clear the visibility rule for this channel",
                                                        )
                                                        .clicked()
                                                    {
                                                        ch_visibility_change = Some((
                                                            ch.clone(),
                                                            TabVisibilityRule::Always,
                                                        ));
                                                        ui.close_menu();
                                                    }
                                                });
                                            }
                                            ui.separator();
                                            if ui
                                                .button(
                                                    RichText::new("Unhide all")
                                                        .font(t::small()),
                                                )
                                                .clicked()
                                            {
                                                ch_unhide_bulk = hidden_channels.clone();
                                                ui.close_menu();
                                            }
                                        },
                                    );
                                    });
                                });
                        }
                        let mut list = ChannelList {
                            channels: &visible_channels,
                            active: self.state.active_channel.as_ref(),
                            channel_states: &self.state.channels,
                            live_channels: Some(&self.live_map_cache),
                            show_live_indicator: self.show_tab_live_indicators,
                            show_close_button: self.show_tab_close_buttons,
                            show_live_feed_pin: self.state.auth.logged_in,
                            live_feed_count: self.state.live_channels.len(),
                            // Mentions pin: always visible when logged in so
                            // users know the feature exists even before the
                            // buffer has any content (mirrors Live-pin UX).
                            show_mentions_pin: self.state.auth.logged_in,
                            mentions_total: self.state.mentions.len(),
                            mentions_unread: self.state.mentions_unread,
                            tab_visibility_rules: &self.state.tab_visibility_rules,
                        };
                        let res = list.show(ui);
                        ch_selected = res.selected;
                        ch_closed = res.closed;
                        // `res.reordered` is the reordered VISIBLE subset;
                        // splice hidden channels back into their original
                        // absolute positions so they don't get bumped to
                        // the end of the list just because they weren't
                        // in the rendered slice.
                        if let Some(new_visible) = res.reordered {
                            let mut visible_iter = new_visible.into_iter();
                            let mut merged: Vec<ChannelId> =
                                Vec::with_capacity(self.state.channel_order.len());
                            for ch in self.state.channel_order.iter() {
                                if self.tab_is_hidden(ch) {
                                    merged.push(ch.clone());
                                } else if let Some(next) = visible_iter.next() {
                                    merged.push(next);
                                }
                            }
                            ch_reordered = Some(merged);
                        }
                        ch_drag_split = res.drag_split;
                        show_split_drop_zone = res.dragging_outside;
                        ch_open_streamlink = res.open_streamlink;
                        ch_open_player = res.open_player;
                        if let Some(change) = res.visibility_change {
                            ch_visibility_change = Some(change);
                        }
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

        // Streamlink / custom player launches (Twitch channels only).
        if let Some(ch) = ch_open_streamlink {
            if ch.is_twitch() {
                self.send_cmd(AppCommand::OpenStreamlink {
                    channel: ch.display_name().to_owned(),
                });
            }
        }
        if let Some(ch) = ch_open_player {
            if ch.is_twitch() {
                self.send_cmd(AppCommand::OpenPlayer {
                    channel: ch.display_name().to_owned(),
                });
            }
        }

        // Per-tab visibility rule toggle ("Hide when offline"). Updates
        // the local mirror immediately so the tab strip reflows on the
        // next frame, then persists via the runtime which re-broadcasts
        // the full rule set for cross-pane consistency.
        if let Some((ch, rule)) = ch_visibility_change {
            self.state
                .set_tab_visibility_rule(ch.clone(), rule);
            self.send_cmd(AppCommand::SetTabVisibilityRule {
                channel: ch,
                rule,
            });
        }
        // "Unhide all" from the hidden-tabs escape hatch.
        for ch in ch_unhide_bulk.drain(..) {
            self.state
                .set_tab_visibility_rule(ch.clone(), TabVisibilityRule::Always);
            self.send_cmd(AppCommand::SetTabVisibilityRule {
                channel: ch,
                rule: TabVisibilityRule::Always,
            });
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

        // Analytics right panel
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

        // Central area: messages + input
        CentralPanel::default()
            .frame(Frame::new().fill(t::bg_base()).inner_margin(Margin::ZERO))
            .show(ctx, |ui| {
                // Split-pane mode
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

                    // Draggable separators
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
                                    t::alpha(ac, highlight_alpha),
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

                        if ch.is_live_feed() {
                            use crate::widgets::live_feed::LiveFeed;
                            use std::collections::HashSet;
                            let joined: HashSet<String> = self
                                .state
                                .channel_order
                                .iter()
                                .filter(|c| {
                                    c.platform() == crust_core::model::Platform::Twitch
                                        && !c.is_live_feed()
                                })
                                .map(|c| c.0.clone())
                                .collect();
                            let action = LiveFeed {
                                snapshots: &self.state.live_channels,
                                loaded: self.state.live_feed_loaded,
                                error: self.state.live_feed_error.as_deref(),
                                last_updated: self.state.live_feed_last_updated,
                                joined_logins: &joined,
                                thumbnail_bytes: &self.emote_bytes,
                            }
                            .show(&mut pane_ui);
                            if let Some(act) = action {
                                self.handle_live_feed_action(act);
                            }
                            continue; // skip the rest of this pane iteration
                        }

                        if ch.is_mentions() {
                            use crate::widgets::mentions::MentionsList;
                            let action = MentionsList {
                                mentions: &self.state.mentions,
                                show_timestamps: self.show_timestamps,
                                show_timestamp_seconds: self.show_timestamp_seconds,
                                use_24h_timestamps: self.use_24h_timestamps,
                            }
                            .show(&mut pane_ui);
                            if let Some(target) = action {
                                self.jump_to_mention(target);
                            }
                            continue;
                        }

                        // Pane header
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
                            let show_viewers = self.split_header_show_viewer_count
                                && !(self.streamer_mode_active
                                    && self.streamer_hide_viewer_counts);
                            split_header_meta_text(
                                self.stream_statuses.get(&login),
                                show_viewers,
                                self.split_header_show_game,
                                self.split_header_show_title,
                                self.channel_points.get(&ch).copied(),
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

                        // Pane chat input (bottom)
                        let input_h = t::bar_h()
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
                            for up in inp.uploads {
                                self.send_cmd(AppCommand::UploadImage {
                                    channel: ch.clone(),
                                    bytes: up.bytes,
                                    format: up.format,
                                    source_path: up.source_path.map(|p| p.display().to_string()),
                                });
                            }
                            if let Some(word) = inp.add_to_dictionary {
                                self.send_cmd(AppCommand::AddWordToDictionary { word });
                            }
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
                                let active_login =
                                    self.state.auth.username.clone();
                                let live_channels = collect_live_channel_entries(
                                    &self.stream_statuses,
                                );
                                // Apply user-defined command aliases before
                                // dispatch so they compose with built-in
                                // slash commands and the IRC fall-through.
                                let alias_result = self
                                    .expand_outgoing_aliases(&text, &ch);
                                let (text, alias_error) = match alias_result {
                                    Ok(Some(expanded)) => (expanded, None),
                                    Ok(None) => (text, None),
                                    Err(err_cmd) => (text, Some(err_cmd)),
                                };
                                let pcmd = if alias_error.is_some() {
                                    None
                                } else {
                                    parse_slash_command(
                                        &text,
                                        &ch,
                                        None,
                                        None,
                                        can_mod,
                                        cc,
                                        self.kick_beta_enabled,
                                        self.irc_beta_enabled,
                                        active_login.as_deref(),
                                        &live_channels,
                                    )
                                };
                                if let Some(err_cmd) = alias_error {
                                    self.send_cmd(err_cmd);
                                } else if let Some(cmd) = pcmd {
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
                                    self.send_cmd(cmd);
                                } else {
                                    if ch.is_irc() {
                                        self.irc_status_panel
                                            .note_outgoing(&ch, &text);
                                    }
                                    self.send_cmd(AppCommand::SendMessage {
                                        channel: ch.clone(),
                                        text,
                                        reply_to_msg_id: None,
                                        reply: None,
                                    });
                                }
                            }
                            if inp.toggle_emote_picker {
                                self.emote_picker.toggle();
                            }
                        }

                        // Message list (remaining space)
                        // Region between header bottom and input top.
                        let mut search_h = 0.0;
                        let content_top = pane_rect.top() + split_header_height() + pane_inner_pad;
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
                            let mut raid_banner_dismissed = false;
                            let hype_banner = self.state.hype_train_for(&ch).cloned();
                            let raid_banner = self.state.raid_banner_for(&ch).cloned();
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
                            let shared_session =
                                self.state.shared_chat_sessions.get(&ch).cloned();
                            if hype_banner.is_some()
                                || raid_banner.is_some()
                                || shared_session.is_some()
                            {
                                if let Some(s) = &shared_session {
                                    show_shared_chat_banner(
                                        &mut msg_ui,
                                        s,
                                        &self.emote_bytes,
                                    );
                                    msg_ui.add_space(4.0);
                                }
                                if let Some(h) = &hype_banner {
                                    show_hype_train_banner(&mut msg_ui, h);
                                    msg_ui.add_space(4.0);
                                }
                                if let Some(r) = &raid_banner {
                                    if show_raid_banner(&mut msg_ui, r) {
                                        raid_banner_dismissed = true;
                                    }
                                    msg_ui.add_space(4.0);
                                }
                            }
                            let scroll_to =
                                self.pending_scroll_to_message.remove(&ch);
                            let ch_live = self
                                .live_map_cache
                                .get(ch.display_name())
                                .copied();
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
                                &self.compiled_ignored_users,
                                &self.highlight_rules,
                                &self.filter_records,
                                &self.mod_action_presets,
                                &ch_state.low_trust_users,
                            )
                            .with_scroll_to(scroll_to)
                            .with_hide_link_previews(
                                self.streamer_mode_active && self.streamer_hide_link_previews,
                            )
                            .with_channel_status(ch_live, true)
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
                            if raid_banner_dismissed {
                                self.state.dismiss_raid_banner(&ch);
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

                    // Emote picker -> focused pane
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
                // Classic single-channel mode
                } else if let Some(active_ch) = self.state.active_channel.clone() {
                    if active_ch.is_live_feed() {
                        use crate::widgets::live_feed::LiveFeed;
                        use std::collections::HashSet;
                        let joined: HashSet<String> = self
                            .state
                            .channel_order
                            .iter()
                            .filter(|c| {
                                c.platform() == crust_core::model::Platform::Twitch
                                    && !c.is_live_feed()
                            })
                            .map(|c| c.0.clone())
                            .collect();
                        let action = LiveFeed {
                            snapshots: &self.state.live_channels,
                            loaded: self.state.live_feed_loaded,
                            error: self.state.live_feed_error.as_deref(),
                            last_updated: self.state.live_feed_last_updated,
                            joined_logins: &joined,
                            thumbnail_bytes: &self.emote_bytes,
                        }
                        .show(ui);
                        if let Some(act) = action {
                            self.handle_live_feed_action(act);
                        }
                        return;  // skip the chat-rendering path entirely
                    }

                    if active_ch.is_mentions() {
                        use crate::widgets::mentions::MentionsList;
                        let action = MentionsList {
                            mentions: &self.state.mentions,
                            show_timestamps: self.show_timestamps,
                            show_timestamp_seconds: self.show_timestamp_seconds,
                            use_24h_timestamps: self.use_24h_timestamps,
                        }
                        .show(ui);
                        if let Some(target) = action {
                            self.jump_to_mention(target);
                        }
                        return;
                    }
                    let active_reply = self
                        .pending_reply
                        .as_ref()
                        .filter(|r| r.channel == active_ch)
                        .map(|r| r.info.clone());

                    // Input tray pinned to bottom. Box grows with chat font.
                    let input_box_h = (t::chat_font_size() + 16.0).max(t::bar_h());
                    let input_panel_h = if active_reply.is_some() {
                        64.0_f32.max(input_box_h + 36.0)
                    } else {
                        input_box_h + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom) as f32
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
                            for up in result.uploads {
                                self.send_cmd(AppCommand::UploadImage {
                                    channel: active_ch.clone(),
                                    bytes: up.bytes,
                                    format: up.format,
                                    source_path: up.source_path.map(|p| p.display().to_string()),
                                });
                            }
                            if let Some(word) = result.add_to_dictionary {
                                self.send_cmd(AppCommand::AddWordToDictionary { word });
                            }
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

                                let active_login =
                                    self.state.auth.username.clone();
                                let live_channels = collect_live_channel_entries(
                                    &self.stream_statuses,
                                );
                                // Expand user command aliases first; if the
                                // chain cycles, emit the error and skip the
                                // normal slash/send path.
                                let alias_result = self
                                    .expand_outgoing_aliases(&text, &active_ch);
                                let (text, alias_error) = match alias_result {
                                    Ok(Some(expanded)) => (expanded, None),
                                    Ok(None) => (text, None),
                                    Err(err_cmd) => (text, Some(err_cmd)),
                                };
                                let parsed_cmd = if alias_error.is_some() {
                                    None
                                } else {
                                    parse_slash_command(
                                        &text,
                                        &active_ch,
                                        reply_to_msg_id.clone(),
                                        active_reply.clone(),
                                        can_moderate,
                                        chatters_count,
                                        self.kick_beta_enabled,
                                        self.irc_beta_enabled,
                                        active_login.as_deref(),
                                        &live_channels,
                                    )
                                };

                                if let Some(err_cmd) = alias_error {
                                    self.send_cmd(err_cmd);
                                } else if !self.state.auth.logged_in {
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
                    let hype_banner_single =
                        self.state.hype_train_for(&active_ch).cloned();
                    let raid_banner_single =
                        self.state.raid_banner_for(&active_ch).cloned();
                    let mut raid_banner_dismissed_single = false;
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
                        if let Some(s) = self.state.shared_chat_sessions.get(&active_ch) {
                            show_shared_chat_banner(&mut msg_ui, s, &self.emote_bytes);
                            msg_ui.add_space(4.0);
                        }
                        if let Some(h) = &hype_banner_single {
                            show_hype_train_banner(&mut msg_ui, h);
                            msg_ui.add_space(4.0);
                        }
                        if let Some(r) = &raid_banner_single {
                            if show_raid_banner(&mut msg_ui, r) {
                                raid_banner_dismissed_single = true;
                            }
                            msg_ui.add_space(4.0);
                        }
                        let scroll_to =
                            self.pending_scroll_to_message.remove(&active_ch);
                        let active_ch_live = self
                            .live_map_cache
                            .get(active_ch.display_name())
                            .copied();
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
                            &self.compiled_ignored_users,
                            &self.highlight_rules,
                            &self.filter_records,
                            &self.mod_action_presets,
                            &state.low_trust_users,
                        )
                        .with_scroll_to(scroll_to)
                        .with_hide_link_previews(
                            self.streamer_mode_active && self.streamer_hide_link_previews,
                        )
                        .with_channel_status(active_ch_live, true)
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
                    if raid_banner_dismissed_single {
                        self.state.dismiss_raid_banner(&active_ch);
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

        // Split drop-zone overlay
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
                        t::alpha(ac, alpha),
                        egui::Stroke::new(2.0, t::alpha(ac, border_alpha)),
                        egui::epaint::StrokeKind::Outside,
                    );
                    // Center label.
                    ui.painter().text(
                        zone_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drop to split",
                        t::heading(),
                        t::alpha(t::text_on_accent(), (120.0 + pulse * 100.0) as u8),
                    );
                });
            ctx.request_repaint();
        }

        // Event toast overlay
        // Expire toasts older than 5 s, then render remaining ones as stacked
        // floating banners anchored to the top-right of the screen.
        self.flush_event_toast_queue();
        self.event_toasts
            .retain(|t| t.born.elapsed().as_secs_f32() < EVENT_TOAST_TTL_SECS);
        let mut toast_y = 58.0;
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
                    egui::vec2(-14.0 - slide_x, toast_y),
                )
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    ui.set_max_width(360.0);
                    let time = ctx.input(|input| input.time) as f32;
                    let border_stroke = if toast.confetti {
                        egui::Stroke::new(
                            1.6,
                            rainbow_color(
                                (time * 0.16 + age * 0.35).fract(),
                                (200.0 * opacity) as u8,
                            ),
                        )
                    } else {
                        egui::Stroke::new(1.5, t::alpha(toast.hue, (160.0 * opacity) as u8))
                    };
                    let fill_col = {
                        let o = t::overlay_fill();
                        t::alpha(o, (225.0 * opacity) as u8)
                    };
                    let frame_resp = egui::Frame::new()
                        .fill(fill_col)
                        .stroke(border_stroke)
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(14, 8))
                        .show(ui, |ui| {
                            ui.set_opacity(opacity);
                            ui.label(
                                RichText::new(&toast.text)
                                    .font(t::body())
                                    .color(t::text_on_accent()),
                            );
                        });

                    if toast.confetti {
                        let rect = frame_resp.response.rect.expand(4.0);
                        let painter = ui.painter();
                        for n in 0..24 {
                            let seed = (n as f32) * 17.0 + (i as f32) * 5.0;
                            let base_x = rect.left() + ((seed * 0.37).fract() * rect.width());
                            let drop = ((seed * 0.11) + age * 0.85).fract();
                            let y = rect.top() - 3.0 + drop * (rect.height() + 10.0);
                            let drift = ((age * 5.2) + seed * 0.23).sin() * 3.2;
                            let x = (base_x + drift).clamp(rect.left(), rect.right());
                            let hue = ((n as f32 / 24.0) + time * 0.18 + age * 0.4).fract();
                            let col = rainbow_color(hue, (190.0 * opacity) as u8);
                            painter.circle_filled(
                                egui::pos2(x, y),
                                1.6 + (n % 3) as f32 * 0.45,
                                col,
                            );
                        }
                    }

                    toast_y += frame_resp.response.rect.height() + 8.0;
                });
        }
        // Keep animating while toasts are live.
        if !self.event_toasts.is_empty() || !self.event_toast_queue.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }

        // Global search popup
        if self.global_search.open {
            refresh_if_stale(&mut self.global_search, &self.state.channels);
            let output: GlobalSearchOutput = show_global_search_window(
                ctx,
                &self.state.channels,
                &mut self.global_search,
                self.always_on_top,
            );
            self.apply_global_search_output(output);
        }

        self.perf.set_chat_stats(frame_chat_stats);
        self.perf.show(ctx);

        // Slow-frame warning - emit once per slow frame with context so we
        // can diagnose UI-thread hitches from user reports.
        let frame_ms = frame_start.elapsed().as_millis() as u64;
        let slow_threshold_ms: u64 = std::env::var("CRUST_SLOW_FRAME_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        if frame_ms >= slow_threshold_ms {
            let active = self
                .state
                .active_channel
                .as_ref()
                .map(|c| c.display_name().to_owned())
                .unwrap_or_default();
            let msg_count = self
                .state
                .active_channel
                .as_ref()
                .and_then(|c| self.state.channels.get(c))
                .map(|s| s.messages.len())
                .unwrap_or(0);
            let chatter_count = self
                .state
                .active_channel
                .as_ref()
                .and_then(|c| self.state.channels.get(c))
                .map(|s| s.chatters.len())
                .unwrap_or(0);
            tracing::warn!(
                "slow frame: {frame_ms} ms (channel={active} msgs={msg_count} \
                 chatters={chatter_count} channels={})",
                self.state.channels.len()
            );
        }

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

fn rainbow_color(hue: f32, alpha: u8) -> Color32 {
    let hsva = egui::ecolor::Hsva::new(hue.rem_euclid(1.0), 0.82, 1.0, alpha as f32 / 255.0);
    Color32::from(hsva)
}

fn quick_switch_priority_bucket(unread_mentions: u32, unread_count: u32) -> u8 {
    if unread_mentions > 0 {
        0
    } else if unread_count > 0 {
        1
    } else {
        2
    }
}

#[cfg(test)]
fn filter_channels_for_query(channels: &[ChannelId], query: &str) -> Vec<ChannelId> {
    let query = query.trim().to_ascii_lowercase();
    channels
        .iter()
        .filter(|ch| channel_matches_query(ch, &query))
        .cloned()
        .collect()
}

fn channel_matches_query(channel: &ChannelId, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    let display = channel.display_name().to_ascii_lowercase();
    if display.contains(query) {
        return true;
    }

    if channel.as_str().to_ascii_lowercase().contains(query) {
        return true;
    }

    if channel.is_twitch() && format!("twitch:{display}").contains(query) {
        return true;
    }
    if channel.is_kick() && format!("kick:{display}").contains(query) {
        return true;
    }
    if channel.is_irc() {
        if let Some(target) = channel.irc_target() {
            let host = target.host.to_ascii_lowercase();
            if host.contains(query) {
                return true;
            }
            let scheme = if target.tls { "ircs" } else { "irc" };
            let url_like = format!(
                "{scheme}://{}:{}/{}",
                host,
                target.port,
                target.channel.to_ascii_lowercase()
            );
            if url_like.contains(query) {
                return true;
            }
        }
    }

    false
}

fn whisper_thread_matches_query(login: &str, display_name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    let login = login.trim().to_ascii_lowercase();
    let display_name = display_name.trim().to_ascii_lowercase();
    login.contains(query)
        || display_name.contains(query)
        || format!("whisper:{login}").contains(query)
        || format!("w:{login}").contains(query)
        || format!("@{display_name}").contains(query)
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
        return "...".to_owned();
    }
    let head: String = chars.into_iter().take(max_chars - 1).collect();
    format!("{head}...")
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
    channel_points: Option<u64>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(status) = status {
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
    }
    if let Some(points) = channel_points {
        parts.push(format!("{} pts", format_points_short(points)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

fn format_points_short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => {
            let raw = format!("{:.1}", n as f64 / 1_000.0);
            format!("{}k", raw.trim_end_matches('0').trim_end_matches('.'))
        }
        _ => {
            let raw = format!("{:.1}", n as f64 / 1_000_000.0);
            format!("{}m", raw.trim_end_matches('0').trim_end_matches('.'))
        }
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

/// Build the list of currently-live channels passed to the slash parser so
/// `/live` can enumerate them without borrowing `CrustApp` internals.
fn collect_live_channel_entries(
    stream_statuses: &HashMap<String, StreamStatusInfo>,
) -> Vec<LiveChannelEntry> {
    let mut entries: Vec<LiveChannelEntry> = stream_statuses
        .iter()
        .filter(|(_, info)| info.is_live)
        .map(|(login, info)| LiveChannelEntry {
            login: login.clone(),
            game: info.game.clone(),
        })
        .collect();
    entries.sort_by(|a, b| a.login.cmp(&b.login));
    entries
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
    if login.starts_with('_') {
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
/// Pre-formatted live channel row passed to the slash-command parser for
/// `/live`. Held as a small struct so the parser stays borrow-free of
/// `CrustApp` internals.
pub(crate) struct LiveChannelEntry {
    pub login: String,
    pub game: Option<String>,
}

fn parse_slash_command(
    text: &str,
    channel: &ChannelId,
    reply_to_msg_id: Option<String>,
    reply: Option<ReplyInfo>,
    is_mod: bool,
    chatters_count: usize,
    kick_beta_enabled: bool,
    irc_beta_enabled: bool,
    active_username: Option<&str>,
    live_channels: &[LiveChannelEntry],
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

        "reloadplugins" | "pluginsreload" => Some(AppCommand::ReloadPlugins),

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
                    text: "Poll commands require moderator or broadcaster permissions.".to_owned(),
                });
            }

            let usage = "Usage: /poll --title \"<title>\" --choice \"<choice 1>\" --choice \"<choice 2>\" [--choice \"<choice 3>\"] [--duration <15..1800>|<60s|1m>] [--points <n>]";
            if rest.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            let parsed = parse_poll_flag_args(rest);

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
                    text: "Poll commands require moderator or broadcaster permissions.".to_owned(),
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
                    text: "Poll commands require moderator or broadcaster permissions.".to_owned(),
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

            let Some(choice_number) = rest.trim().parse::<u32>().ok().filter(|choice| *choice > 0)
            else {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /vote <choice number>".to_owned(),
                });
            };

            let _choice_number = choice_number;

            Some(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text: "Twitch poll voting is not available over IRC. Vote in the Twitch poll card instead.".to_owned(),
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

            let usage = "Usage: /announce <message> [--color primary|blue|green|orange|purple]";
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

        "logs" => Some(AppCommand::OpenLogsFolder),

        "live" => {
            let text = if live_channels.is_empty() {
                "No tracked Twitch channels are currently live.".to_owned()
            } else {
                let mut lines = String::from("Currently live:\n");
                for entry in live_channels {
                    match &entry.game {
                        Some(game) if !game.is_empty() => {
                            lines.push_str(&format!("  {}{}\n", entry.login, game));
                        }
                        _ => {
                            lines.push_str(&format!("  {}\n", entry.login));
                        }
                    }
                }
                lines.trim_end().to_owned()
            };
            Some(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text,
            })
        }

        "shield" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Shield Mode is only supported for Twitch channels.".to_owned(),
                });
            }
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Shield Mode requires moderator or broadcaster permissions.".to_owned(),
                });
            }
            let arg = rest.split_whitespace().next().unwrap_or("");
            let active = match arg.to_ascii_lowercase().as_str() {
                "on" | "enable" | "enabled" | "true" | "1" => true,
                "off" | "disable" | "disabled" | "false" | "0" => false,
                _ => {
                    return Some(AppCommand::InjectLocalMessage {
                        channel: channel.clone(),
                        text: "Usage: /shield <on|off>".to_owned(),
                    });
                }
            };
            Some(AppCommand::SetShieldMode {
                channel: channel.clone(),
                active,
            })
        }

        "setgame" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "/setgame is only supported for Twitch channels.".to_owned(),
                });
            }
            let is_broadcaster = active_username
                .map(|u| u.eq_ignore_ascii_case(channel.display_name()))
                .unwrap_or(false);
            if !is_broadcaster {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "/setgame requires broadcaster permissions on this channel.".to_owned(),
                });
            }
            if rest.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /setgame <category>".to_owned(),
                });
            }
            Some(AppCommand::UpdateChannelInfo {
                channel: channel.clone(),
                title: None,
                game_name: Some(rest.to_owned()),
            })
        }

        "settitle" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "/settitle is only supported for Twitch channels.".to_owned(),
                });
            }
            let is_broadcaster = active_username
                .map(|u| u.eq_ignore_ascii_case(channel.display_name()))
                .unwrap_or(false);
            if !is_broadcaster {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "/settitle requires broadcaster permissions on this channel.".to_owned(),
                });
            }
            if rest.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /settitle <title>".to_owned(),
                });
            }
            Some(AppCommand::UpdateChannelInfo {
                channel: channel.clone(),
                title: Some(rest.to_owned()),
                game_name: None,
            })
        }

        "follow-age" | "followage" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "/follow-age is only supported for Twitch channels.".to_owned(),
                });
            }
            let user = rest
                .split_whitespace()
                .next()
                .map(|s| s.trim_start_matches('@').to_owned())
                .or_else(|| active_username.map(str::to_owned));
            match user {
                None => Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /follow-age [user]  (sign in to default to yourself).".to_owned(),
                }),
                Some(u) => Some(AppCommand::FetchFollowAge {
                    channel: channel.clone(),
                    user: u,
                }),
            }
        }

        "account-age" | "accountage" => {
            if !channel.is_twitch() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "/account-age is only supported for Twitch channels.".to_owned(),
                });
            }
            let user = rest
                .split_whitespace()
                .next()
                .map(|s| s.trim_start_matches('@').to_owned())
                .or_else(|| active_username.map(str::to_owned));
            match user {
                None => Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /account-age [user]  (sign in to default to yourself)."
                        .to_owned(),
                }),
                Some(u) => Some(AppCommand::FetchAccountAge {
                    channel: channel.clone(),
                    user: u,
                }),
            }
        }

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
        "warn" => {
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Warn commands require moderator or broadcaster permissions.".to_owned(),
                });
            }
            let (target, reason) = rest
                .split_once(char::is_whitespace)
                .map(|(user, reason)| (user.trim(), reason.trim()))
                .unwrap_or((rest.trim(), ""));
            if target.is_empty() || reason.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /warn <user> <reason>".to_owned(),
                })
            } else {
                Some(AppCommand::WarnUser {
                    channel: channel.clone(),
                    login: target.trim_start_matches('@').to_ascii_lowercase(),
                    user_id: String::new(),
                    reason: reason.to_owned(),
                })
            }
        }
        "monitor" => {
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Low-trust commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            let target = rest.trim();
            if target.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /monitor <user>".to_owned(),
                })
            } else {
                Some(AppCommand::SetSuspiciousUser {
                    channel: channel.clone(),
                    login: target.trim_start_matches('@').to_ascii_lowercase(),
                    user_id: String::new(),
                    restricted: false,
                })
            }
        }
        "restrict" => {
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Low-trust commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            let target = rest.trim();
            if target.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Usage: /restrict <user>".to_owned(),
                })
            } else {
                Some(AppCommand::SetSuspiciousUser {
                    channel: channel.clone(),
                    login: target.trim_start_matches('@').to_ascii_lowercase(),
                    user_id: String::new(),
                    restricted: true,
                })
            }
        }
        "unmonitor" | "unrestrict" => {
            if !is_mod {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: "Low-trust commands require moderator or broadcaster permissions."
                        .to_owned(),
                });
            }
            let target = rest.trim();
            if target.is_empty() {
                Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: format!("Usage: /{cmd} <user>"),
                })
            } else {
                Some(AppCommand::ClearSuspiciousUser {
                    channel: channel.clone(),
                    login: target.trim_start_matches('@').to_ascii_lowercase(),
                    user_id: String::new(),
                })
            }
        }
        "banid" if !rest.is_empty() => {
            // /banid <userID>  ->  forward as /ban to IRC (uses ID not name).
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

        // /w <user> <message>  - Twitch whisper via Helix.
        "w" | "whisper" => {
            if !channel.is_twitch() {
                return Some(AppCommand::SendMessage {
                    channel: channel.clone(),
                    text: text.to_owned(),
                    reply_to_msg_id,
                    reply: reply.clone(),
                });
            }

            let usage = "Usage: /w <user> <message>";
            let (raw_target, raw_message) = rest
                .split_once(char::is_whitespace)
                .map(|(target, message)| (target.trim(), message.trim()))
                .unwrap_or((rest.trim(), ""));

            let Some(target_login) = parse_twitch_channel_login_arg(raw_target) else {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            };
            if raw_message.is_empty() {
                return Some(AppCommand::InjectLocalMessage {
                    channel: channel.clone(),
                    text: usage.to_owned(),
                });
            }

            Some(AppCommand::SendWhisper {
                target_login,
                text: raw_message.to_owned(),
            })
        }

        _ => {
            let plugin_commands = plugin_command_infos();
            if let Some(info) = plugin_commands.iter().find(|info| {
                info.name.eq_ignore_ascii_case(&cmd_lower)
                    || info
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(&cmd_lower))
            }) {
                let words = split_quoted_args(text);
                return Some(AppCommand::RunPluginCommand {
                    channel: channel.clone(),
                    command: info.name.clone(),
                    words,
                    reply_to_msg_id,
                    reply,
                    raw_text: text.to_owned(),
                });
            }

            // Everything else falls through to IRC
            // Standard Twitch chat commands (/ban, /timeout, /unban, /slow,
            // /subscribers, /emoteonly, /clear, /mod, /vip, /color, /delete,
            // /raid, /host, /uniquechat, /block, /unblock,
            // /r, etc.) are handled server-side.
            None
        }
    }
}

fn is_anonymous_local_command(cmd: &AppCommand) -> bool {
    matches!(
        cmd,
        AppCommand::InjectLocalMessage { .. }
            | AppCommand::ClearLocalMessages { .. }
            | AppCommand::ReloadPlugins
            | AppCommand::OpenUrl { .. }
            | AppCommand::ShowUserCard { .. }
            | AppCommand::RunPluginCommand { .. }
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

fn parse_poll_points_token(raw: &str) -> Option<u32> {
    raw.trim().parse::<u32>().ok().filter(|value| *value > 0)
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
            "--title" | "-t" => {
                let value = tokens.get(i + 1)?.trim();
                if value.is_empty() {
                    return None;
                }
                title = Some(value.to_owned());
                i += 2;
            }
            "--choice" | "-c" => {
                let value = tokens.get(i + 1)?.trim();
                if value.is_empty() {
                    return None;
                }
                choices.push(value.to_owned());
                i += 2;
            }
            "--duration" | "-d" => {
                let value = tokens.get(i + 1)?;
                duration_secs = parse_poll_duration_token(value);
                if duration_secs.is_none() {
                    return None;
                }
                i += 2;
            }
            "--points" | "-p" => {
                let value = tokens.get(i + 1)?;
                channel_points_per_vote = Some(parse_poll_points_token(value)?);
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
        if matches!(tokens[i], "--duration" | "-d") {
            if let Some(next) = tokens.get(i + 1) {
                if let Some(v) = parse_poll_duration_token(next) {
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
                if matches!(
                    candidate.as_str(),
                    "primary" | "blue" | "green" | "orange" | "purple"
                ) {
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

/// Build an emote code -> catalog entry lookup with provider priority
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

fn tokenize_whisper_text(
    text: &str,
    twitch_emotes: &[TwitchEmotePos],
    emote_map: &HashMap<&str, &EmoteCatalogEntry>,
) -> Vec<Span> {
    crust_core::format::tokenize(text, false, twitch_emotes, &|code| {
        emote_map.get(code).map(|e| {
            (
                e.code.clone(),
                e.code.clone(),
                e.url.clone(),
                e.provider.clone(),
                None,
            )
        })
    })
    .into_vec()
}

fn whisper_fit_size(w: u32, h: u32, target_h: f32) -> egui::Vec2 {
    if h == 0 || w == 0 {
        return egui::vec2(target_h, target_h);
    }
    let scale = target_h / h as f32;
    egui::vec2((w as f32 * scale).max(6.0), target_h)
}

#[cfg(test)]
mod tests {
    use super::{
        filter_channels_for_query, parse_slash_command, parse_twitch_channel_login_arg,
        quick_switch_priority_bucket, responsive_layout, toolbar_visibility, top_tab_metrics,
        whisper_thread_matches_query, TabVisualStyle,
    };
    use crate::theme as t;
    use crust_core::events::AppCommand;
    use crust_core::model::ChannelId;

    #[test]
    fn responsive_layout_prefers_top_tabs_on_narrow_windows() {
        let layout = responsive_layout(460.0);
        assert!(layout.force_top_tabs);
        assert_eq!(layout.min_central_width, 120.0);
        assert_eq!(layout.sidebar_min_width, t::SIDEBAR_COMPACT_MIN_W);
        assert_eq!(layout.status_bar_height, 40.0);
    }

    #[test]
    fn responsive_layout_compacts_further_on_very_narrow_windows() {
        let layout = responsive_layout(280.0);
        assert!(layout.force_top_tabs);
        assert_eq!(layout.status_bar_height, 40.0);
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
        let hidden = toolbar_visibility(900.0, false, true);
        assert!(!hidden.show_irc_toggle);
        assert!(!hidden.show_irc_in_overflow);

        let shown = toolbar_visibility(900.0, true, true);
        assert!(shown.show_irc_toggle);
    }

    #[test]
    fn toolbar_keeps_regular_icon_size_until_space_is_tight() {
        let visibility = toolbar_visibility(520.0, true, true);
        assert!(visibility.show_join_button);
        assert!(visibility.show_perf_toggle);
        assert!(visibility.show_stats_toggle);
        assert!(!visibility.compact_controls);
    }

    #[test]
    fn toolbar_hides_diagnostics_before_hiding_core_actions() {
        let visibility = toolbar_visibility(350.0, true, true);
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
    fn quick_switch_filter_matches_platform_prefixes_and_irc_hosts() {
        let irc = ChannelId::irc("irc.libera.chat", 6697, true, "rust");
        let channels = vec![
            ChannelId::new("forsen"),
            ChannelId::kick("xqc"),
            irc.clone(),
        ];

        assert_eq!(
            filter_channels_for_query(&channels, "twitch:for"),
            vec![ChannelId::new("forsen")]
        );
        assert_eq!(
            filter_channels_for_query(&channels, "kick:xq"),
            vec![ChannelId::kick("xqc")]
        );
        assert_eq!(filter_channels_for_query(&channels, "libera"), vec![irc]);
    }

    #[test]
    fn quick_switch_priority_orders_mentions_then_unread_then_other() {
        let mut buckets = vec![
            (0_u32, 0_u32),
            (0_u32, 3_u32),
            (2_u32, 2_u32),
            (1_u32, 0_u32),
        ];
        buckets.sort_by_key(|(mentions, unread)| quick_switch_priority_bucket(*mentions, *unread));
        assert_eq!(buckets, vec![(2, 2), (1, 0), (0, 3), (0, 0)]);
    }

    #[test]
    fn whisper_query_matches_prefix_login_and_display_name() {
        assert!(whisper_thread_matches_query(
            "some_user",
            "Some User",
            "w:some"
        ));
        assert!(whisper_thread_matches_query(
            "some_user",
            "Some User",
            "whisper:some"
        ));
        assert!(whisper_thread_matches_query(
            "some_user",
            "Some User",
            "@some user"
        ));
        assert!(!whisper_thread_matches_query(
            "some_user",
            "Some User",
            "notthere"
        ));
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
            None,
            &[],
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
            None,
            &[],
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
        let parsed =
            parse_slash_command(&long, &channel, None, None, true, 0, true, true, None, &[]);

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
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::CreateStreamMarker { description, .. }) => {
                assert_eq!(description.as_deref(), Some("clutch moment"));
            }
            other => panic!("expected CreateStreamMarker, got {other:?}"),
        }
    }

    #[test]
    fn slash_poll_pipe_syntax_is_rejected_in_favor_of_flag_syntax() {
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
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(
                    text,
                    "Usage: /poll --title \"<title>\" --choice \"<choice 1>\" --choice \"<choice 2>\" [--choice \"<choice 3>\"] [--duration <15..1800>|<60s|1m>] [--points <n>]"
                );
            }
            other => panic!("expected usage local message for /poll, got {other:?}"),
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
            None,
            &[],
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
    fn slash_poll_short_flags_map_to_create_poll() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/poll -t \"Best snack\" -c Chips -c Popcorn -d 90s -p 25",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::CreatePoll {
                title,
                choices,
                duration_secs,
                channel_points_per_vote,
                ..
            }) => {
                assert_eq!(title, "Best snack");
                assert_eq!(choices, vec!["Chips".to_owned(), "Popcorn".to_owned()]);
                assert_eq!(duration_secs, 90);
                assert_eq!(channel_points_per_vote, Some(25));
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
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::FetchUnbanRequests { channel }) => {
                assert_eq!(channel.as_str(), "somechannel");
            }
            other => panic!("expected FetchUnbanRequests, got {other:?}"),
        }
    }

    #[test]
    fn parse_twitch_channel_login_rejects_live_feed_sentinel() {
        assert_eq!(parse_twitch_channel_login_arg("__live_feed__"), None);
        assert_eq!(
            parse_twitch_channel_login_arg("forsen"),
            Some("forsen".to_owned())
        );
    }

    #[test]
    fn slash_requests_defaults_to_current_channel_queue() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/requests",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            None,
            &[],
        );

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
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::OpenUrl { url }) => {
                assert_eq!(
                    url,
                    "https://www.twitch.tv/popout/targetchannel/reward-queue"
                );
            }
            other => panic!("expected OpenUrl for reward queue, got {other:?}"),
        }
    }

    #[test]
    fn slash_vote_is_not_advertised_but_still_explains_locally() {
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
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(
                    text,
                    "Twitch poll voting is not available over IRC. Vote in the Twitch poll card instead."
                );
            }
            other => panic!("expected local explanation for /vote, got {other:?}"),
        }
    }

    #[test]
    fn slash_vote_hidden_guard_still_rejects_invalid_choice_numbers() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/vote winner",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(text, "Usage: /vote <choice number>");
            }
            other => panic!("expected usage local message for hidden /vote guard, got {other:?}"),
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
            None,
            &[],
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
            None,
            &[],
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
        let parsed = parse_slash_command(
            "/modtools",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );

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
            None,
            &[],
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
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::SendMessage { text, .. }) => {
                assert_eq!(text, "/unban troublemaker");
            }
            other => panic!("expected SendMessage forwarding to /unban, got {other:?}"),
        }
    }

    #[test]
    fn slash_whisper_maps_to_send_whisper_command() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/w @TargetUser hello there",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::SendWhisper { target_login, text }) => {
                assert_eq!(target_login, "targetuser");
                assert_eq!(text, "hello there");
            }
            other => panic!("expected SendWhisper command, got {other:?}"),
        }
    }

    #[test]
    fn slash_whisper_requires_target_and_message() {
        let channel = ChannelId::new("somechannel");
        let parsed = parse_slash_command(
            "/whisper targetonly",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );

        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(text, "Usage: /w <user> <message>");
            }
            other => panic!("expected usage local message for /whisper, got {other:?}"),
        }
    }

    #[test]
    fn slash_logs_opens_logs_folder() {
        let channel = ChannelId::new("x");
        let parsed = parse_slash_command(
            "/logs",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );
        assert!(matches!(parsed, Some(AppCommand::OpenLogsFolder)));
    }

    #[test]
    fn slash_shield_parses_on_off_arguments() {
        let channel = ChannelId::new("streamer");
        let on = parse_slash_command(
            "/shield on",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );
        assert!(matches!(
            on,
            Some(AppCommand::SetShieldMode { active: true, .. })
        ));

        let off = parse_slash_command(
            "/shield off",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );
        assert!(matches!(
            off,
            Some(AppCommand::SetShieldMode { active: false, .. })
        ));
    }

    #[test]
    fn slash_shield_requires_mod_permissions() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/shield on",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            None,
            &[],
        );
        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert!(text.contains("moderator"));
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_shield_rejects_invalid_argument() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/shield maybe",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            None,
            &[],
        );
        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert_eq!(text, "Usage: /shield <on|off>");
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_settitle_requires_broadcaster_match() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/settitle New Title",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            Some("notstreamer"),
            &[],
        );
        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert!(text.contains("broadcaster"));
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_settitle_accepts_broadcaster_issuer() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/settitle Playing Elden Ring",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            Some("streamer"),
            &[],
        );
        match parsed {
            Some(AppCommand::UpdateChannelInfo {
                title, game_name, ..
            }) => {
                assert_eq!(title.as_deref(), Some("Playing Elden Ring"));
                assert!(game_name.is_none());
            }
            other => panic!("expected UpdateChannelInfo, got {other:?}"),
        }
    }

    #[test]
    fn slash_setgame_maps_to_update_channel_info() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/setgame Just Chatting",
            &channel,
            None,
            None,
            true,
            0,
            true,
            true,
            Some("streamer"),
            &[],
        );
        match parsed {
            Some(AppCommand::UpdateChannelInfo {
                title, game_name, ..
            }) => {
                assert!(title.is_none());
                assert_eq!(game_name.as_deref(), Some("Just Chatting"));
            }
            other => panic!("expected UpdateChannelInfo, got {other:?}"),
        }
    }

    #[test]
    fn slash_followage_defaults_to_active_user_when_omitted() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/follow-age",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            Some("someviewer"),
            &[],
        );
        match parsed {
            Some(AppCommand::FetchFollowAge { user, .. }) => {
                assert_eq!(user, "someviewer");
            }
            other => panic!("expected FetchFollowAge, got {other:?}"),
        }
    }

    #[test]
    fn slash_followage_strips_leading_at_sign() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/follow-age @OtherViewer",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            Some("someviewer"),
            &[],
        );
        match parsed {
            Some(AppCommand::FetchFollowAge { user, .. }) => {
                assert_eq!(user, "OtherViewer");
            }
            other => panic!("expected FetchFollowAge, got {other:?}"),
        }
    }

    #[test]
    fn slash_accountage_without_user_and_without_login_explains_usage() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/account-age",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            None,
            &[],
        );
        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert!(text.contains("Usage: /account-age"));
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_live_with_empty_list_reports_none() {
        let channel = ChannelId::new("streamer");
        let parsed = parse_slash_command(
            "/live",
            &channel,
            None,
            None,
            false,
            0,
            true,
            true,
            None,
            &[],
        );
        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert!(text.contains("No tracked"));
            }
            other => panic!("expected usage local message, got {other:?}"),
        }
    }

    #[test]
    fn slash_live_lists_channels_with_game() {
        let channel = ChannelId::new("streamer");
        let live = vec![
            super::LiveChannelEntry {
                login: "alice".into(),
                game: Some("Just Chatting".into()),
            },
            super::LiveChannelEntry {
                login: "bob".into(),
                game: None,
            },
        ];
        let parsed = parse_slash_command(
            "/live", &channel, None, None, false, 0, true, true, None, &live,
        );
        match parsed {
            Some(AppCommand::InjectLocalMessage { text, .. }) => {
                assert!(text.contains("aliceJust Chatting"));
                assert!(text.contains("bob"));
            }
            other => panic!("expected local message listing live channels, got {other:?}"),
        }
    }
}
