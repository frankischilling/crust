use crate::image_cache::{ImageWorkStats, UiImageCache};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
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
    chat_input::{insert_picker_emote, invalidate_emote_completions, ChatInput},
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
        handle_hotkey_capture, parse_settings_lines, show_settings_page, SettingsPageState,
        SettingsSection, SettingsStats,
    },
    split_header::{show_split_header, split_header_height},
    user_profile_popup::{PopupAction, UserProfilePopup},
    unsent_messages::{UnsentMessage, UnsentMessages},
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
const MAX_DEFERRED_COMMANDS: usize = 128;
const MAX_DEFERRED_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
const MAX_COMMANDS_PER_FRAME: usize = 64;
const MAX_EVENTS_PER_FRAME: u32 = 200;
const EMOTE_REFRESH_ROWS_PER_FRAME: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandQueueError {
    Full,
    Closed,
}

impl CommandQueueError {
    fn message(self) -> &'static str {
        match self {
            Self::Full => "Crust is busy. The last action was not queued; try again in a moment.",
            Self::Closed => "The background service stopped. Pending actions could not be completed.",
        }
    }
}

/// A small UI-owned backlog keeps a full runtime channel from blocking a frame.
/// New actions join the back until older actions have reached the runtime.
#[derive(Default)]
struct DeferredCommands {
    commands: VecDeque<AppCommand>,
    upload_bytes: usize,
    error: Option<CommandQueueError>,
}

impl DeferredCommands {
    fn upload_size(command: &AppCommand) -> usize {
        match command {
            AppCommand::UploadImage { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }

    fn closed(&mut self) {
        self.commands.clear();
        self.upload_bytes = 0;
        self.error = Some(CommandQueueError::Closed);
    }

    fn send(&mut self, sender: &mpsc::Sender<AppCommand>, command: AppCommand) -> bool {
        if sender.is_closed() {
            self.closed();
            return false;
        }
        let command = if self.commands.is_empty() {
            match sender.try_send(command) {
                Ok(()) => return true,
                Err(TrySendError::Full(command)) => command,
                Err(TrySendError::Closed(_)) => {
                    self.closed();
                    return false;
                }
            }
        } else {
            command
        };
        let upload_size = Self::upload_size(&command);
        if self.commands.len() >= MAX_DEFERRED_COMMANDS
            || upload_size > MAX_DEFERRED_UPLOAD_BYTES.saturating_sub(self.upload_bytes)
        {
            self.error = Some(CommandQueueError::Full);
            return false;
        }
        self.upload_bytes += upload_size;
        self.commands.push_back(command);
        true
    }

    fn flush(&mut self, sender: &mpsc::Sender<AppCommand>) {
        for _ in 0..MAX_COMMANDS_PER_FRAME {
            let Some(command) = self.commands.pop_front() else {
                break;
            };
            let upload_size = Self::upload_size(&command);
            match sender.try_send(command) {
                Ok(()) => self.upload_bytes -= upload_size,
                Err(TrySendError::Full(command)) => {
                    self.commands.push_front(command);
                    break;
                }
                Err(TrySendError::Closed(_)) => {
                    self.closed();
                    break;
                }
            }
        }
    }
}

/// Shared by the app and its widgets so every UI action joins the same FIFO.
pub struct UiCommandSender {
    sender: mpsc::Sender<AppCommand>,
    deferred: RefCell<DeferredCommands>,
    repaint_context: RefCell<Option<Context>>,
}

impl UiCommandSender {
    pub fn new(sender: mpsc::Sender<AppCommand>) -> Self {
        Self {
            sender,
            deferred: RefCell::new(DeferredCommands::default()),
            repaint_context: RefCell::new(None),
        }
    }

    pub fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
        let mut queue = self.deferred.borrow_mut();
        let accepted = queue.send(&self.sender, command);
        if let Some(ctx) = self.repaint_context.borrow().as_ref() {
            if !accepted {
                ctx.request_repaint();
            } else if !queue.commands.is_empty() {
                ctx.request_repaint_after(std::time::Duration::from_millis(REPAINT_ANIM_MS));
            }
        }
        if accepted {
            Ok(())
        } else {
            Err(queue.error.expect("a rejected command records its reason"))
        }
    }

    /// Retryable background work waits until user actions have reached the runtime.
    pub fn try_send_background(&self, command: AppCommand) -> bool {
        self.deferred.borrow().commands.is_empty() && self.sender.try_send(command).is_ok()
    }

    fn flush(&mut self, ctx: &Context) -> Option<CommandQueueError> {
        self.repaint_context.get_mut().get_or_insert_with(|| ctx.clone());
        let queue = self.deferred.get_mut();
        queue.flush(&self.sender);
        if !queue.commands.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(REPAINT_ANIM_MS));
        }
        queue.error.take()
    }
}

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
    /// Private notices are cleared when this account is detached.
    owner_user_id: Option<String>,
    /// Accent tint used for the border (Sub = gold, Raid = cyan, Bits = orange).
    hue: Color32,
    /// Whether to draw celebratory confetti particles around the toast.
    confetti: bool,
    /// Wall-clock moment the toast was created.
    born: std::time::Instant,
}

impl EventToast {
    fn from_message(is_active: bool, message: &crust_core::model::ChatMessage) -> Option<Self> {
        if !is_active || message.flags.is_history || message.flags.suppress_notification {
            return None;
        }
        match &message.msg_kind {
            MsgKind::Sub { display_name, months, is_gift, plan, .. } => {
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
                Some(Self {
                    text,
                    owner_user_id: None,
                    hue: if gifted_to_me { t::raid_cyan() } else { t::gold() },
                    confetti: gifted_to_me,
                    born: std::time::Instant::now(),
                })
            }
            MsgKind::Raid { display_name, viewer_count, .. } => Some(Self {
                text: format!("🚀  {} is raiding with {} viewers!", display_name, viewer_count),
                owner_user_id: None,
                hue: t::raid_cyan(),
                confetti: false,
                born: std::time::Instant::now(),
            }),
            MsgKind::Bits { amount } if *amount >= 100 => Some(Self {
                text: format!("💎  {} cheered {} bits!", message.sender.display_name, amount),
                owner_user_id: None,
                hue: t::bits_orange(),
                confetti: false,
                born: std::time::Instant::now(),
            }),
            _ if message.flags.is_pinned => Some(Self {
                text: format!("📌  {} sent a pinned message", message.sender.display_name),
                owner_user_id: None,
                hue: t::gold(),
                confetti: false,
                born: std::time::Instant::now(),
            }),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct PendingReply {
    channel: ChannelId,
    info: ReplyInfo,
}

#[derive(Clone)]
struct WhisperLine {
    from_user_id: String,
    from_login: String,
    from_display_name: String,
    text: String,
    twitch_emotes: Vec<TwitchEmotePos>,
    spans: Vec<Span>,
    timestamp: DateTime<Utc>,
    is_self: bool,
}

#[derive(Default)]
struct WhisperComposer {
    text: String,
    pending: Vec<String>,
    failed: VecDeque<FailedWhisper>,
}

struct FailedWhisper {
    text: String,
    message: String,
}

impl WhisperComposer {
    fn fail_attempt(&mut self, text: &str, message: &str) -> bool {
        let Some(index) = self.pending.iter().position(|pending| pending == text) else {
            return false;
        };
        let text = self.pending.remove(index);
        if self.text.is_empty() {
            self.text.clone_from(&text);
        }
        self.failed.push_back(FailedWhisper {
            text,
            message: message.to_owned(),
        });
        true
    }

    fn acknowledge(&mut self, text: &str) {
        let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.trim() == text.trim())
        else {
            return;
        };
        let sent = self.pending.remove(index);
        if self.text == sent {
            self.text.clear();
        }
        self.failed.retain(|failed| failed.text != sent);
    }
}

enum EmoteRefreshJob {
    Channel(ChannelId, MessageId),
    Mentions(MessageId),
    Whispers(String),
}

// Split-pane state

fn chat_composer_id(pane: Option<u64>) -> egui::Id {
    egui::Id::new(("chat_composer", pane))
}

fn pane_pointer_pressed(ctx: &Context, rect: egui::Rect, layer: egui::LayerId) -> bool {
    let pointer = ctx.input(|input| {
        input.pointer.any_pressed().then(|| input.pointer.interact_pos()).flatten()
    });
    pointer.is_some_and(|pos| rect.contains(pos) && ctx.layer_id_at(pos) == Some(layer))
}

/// One pane in the split view.
#[derive(Clone)]
struct Pane {
    id: u64,
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

    /// Focus an existing channel pane or replace the focused pane's channel.
    fn activate_channel(&mut self, channel: &ChannelId) {
        if let Some(index) = self.panes.iter().position(|pane| &pane.channel == channel) {
            self.focused = index;
        } else if let Some(pane) = self.panes.get_mut(self.focused) {
            pane.channel = channel.clone();
        }
    }

    /// Keep both drafts when a redirected channel already has another pane.
    fn redirect_channel(&mut self, old: &ChannelId, new: &ChannelId) {
        for pane in &mut self.panes {
            if &pane.channel == old {
                pane.channel = new.clone();
            }
        }
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
            id: {
                static NEXT_PANE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                NEXT_PANE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            channel,
            input_buf: String::new(),
            frac: new_frac,
        };
        match insert_at {
            Some(i) if i <= self.panes.len() => {
                if !self.panes.is_empty() && i <= self.focused {
                    self.focused += 1;
                }
                self.panes.insert(i, pane);
            }
            _ => self.panes.push(pane),
        }
        self.normalize_fractions();
        self.clamp_focus();
    }

    /// Remove a pane by index.
    fn remove_pane(&mut self, idx: usize) {
        if idx < self.panes.len() {
            self.panes.remove(idx);
            if idx < self.focused {
                self.focused -= 1;
            }
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

#[derive(Clone, Copy, Debug)]
struct ChannelLayoutControls {
    sidebar_selected: bool,
    sidebar_enabled: bool,
    sidebar_label: &'static str,
    tabs_selected: bool,
}

impl ChannelLayoutControls {
    fn new(layout: ChannelLayout, sidebar_visible: bool, force_top_tabs: bool) -> Self {
        let sidebar_selected = layout == ChannelLayout::Sidebar && sidebar_visible && !force_top_tabs;
        Self {
            sidebar_selected,
            sidebar_enabled: !force_top_tabs,
            sidebar_label: if force_top_tabs {
                "Widen the window to use the channel sidebar"
            } else if sidebar_selected {
                "Hide channel sidebar"
            } else {
                "Show channel sidebar"
            },
            tabs_selected: layout == ChannelLayout::TopTabs
                || (force_top_tabs && sidebar_visible),
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
const TOOLBAR_MOD_BUTTON_W: f32 = 42.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolbarDegradeStep {
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

    if !visibility.compact_controls || visibility.show_overflow_menu
    {
        width += TOOLBAR_SEPARATOR_W;
    }

    if visibility.show_overflow_menu {
        width += TOOLBAR_OVERFLOW_W;
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
    };

    const STEPS: [ToolbarDegradeStep; 13] = [
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
    scroll_to_selected: bool,
}

impl ChannelQuickSwitch {
    fn search_id() -> egui::Id {
        egui::Id::new("channel_quick_switch_search")
    }
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
    cmd_tx: UiCommandSender,
    event_rx: mpsc::Receiver<AppEvent>,
    pending_auth: Option<String>,
    unsent_messages: UnsentMessages,
    emote_bytes: UiImageCache,
    previous_image_work: ImageWorkStats,
    join_dialog: JoinDialog,
    login_dialog: LoginDialog,
    quick_switch: ChannelQuickSwitch,
    emote_picker: EmotePicker,
    chat_input_buf: String,
    emote_catalog: Vec<EmoteCatalogEntry>,
    emote_catalog_initialized: bool,
    emote_catalog_index: HashMap<String, Vec<usize>>,
    input_emote_catalogs: HashMap<ChannelId, Vec<EmoteCatalogEntry>>,
    emote_picker_channel: Option<ChannelId>,
    pending_emote_refresh: VecDeque<EmoteRefreshJob>,
    emote_refresh_followup: bool,
    perf: PerfOverlay,
    /// Reply pending for the next send (set by right-click -> Reply).
    pending_reply: Option<PendingReply>,
    /// User profile card shown when clicking a username.
    user_profile_popup: UserProfilePopup,
    /// Cached link previews (Open-Graph metadata) keyed by URL.
    link_previews: HashMap<String, LinkPreview>,
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
    /// Drafts and pending sends belong to the signed-in account and recipient.
    whisper_composers: HashMap<String, WhisperComposer>,
    focus_whisper_composer: Option<String>,
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
    /// UI zoom last applied on top of the monitor's native pixel density.
    applied_ui_zoom: f32,
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
pub(crate) fn apply_theme_visuals(ctx: &egui::Context) {
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
    vis.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, t::text_secondary());
    vis.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, t::border_subtle());
    vis.widgets.inactive.corner_radius = t::RADIUS;

    vis.widgets.hovered.weak_bg_fill = t::hover_bg();
    vis.widgets.hovered.bg_fill = t::hover_bg();
    vis.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, t::text_primary());
    vis.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, t::border_accent());
    vis.widgets.hovered.corner_radius = t::RADIUS;

    vis.widgets.active.weak_bg_fill = t::accent_dim();
    vis.widgets.active.bg_fill = t::accent_dim();
    vis.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, t::text_on_accent());
    vis.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, t::accent());
    vis.widgets.active.corner_radius = t::RADIUS;

    vis.widgets.open.weak_bg_fill = t::bg_raised();
    vis.widgets.open.bg_fill = t::bg_raised();

    vis.selection.bg_fill = t::accent_dim();
    vis.selection.stroke = egui::Stroke::new(1.0_f32, t::accent());

    vis.window_corner_radius = t::RADIUS;
    vis.window_stroke = t::stroke_subtle();
    vis.menu_corner_radius = t::RADIUS;
    vis.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, t::border_subtle());

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
    consume_exact_key(input, binding_modifiers(binding), key)
}

pub(crate) fn consume_exact_key(
    input: &mut egui::InputState,
    modifiers: egui::Modifiers,
    key: egui::Key,
) -> bool {
    let mut consumed = false;
    input.events.retain(|event| {
        let matches = matches!(event, egui::Event::Key {
            key: event_key,
            pressed: true,
            modifiers: event_modifiers,
            ..
        } if *event_key == key && event_modifiers.matches_exact(modifiers));
        consumed |= matches;
        !matches
    });
    consumed
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

        Self::with_event_channels(cmd_tx, event_rx)
    }

    fn with_event_channels(
        cmd_tx: mpsc::Sender<AppCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Self {
        Self {
            state: AppState::default(),
            cmd_tx: UiCommandSender::new(cmd_tx),
            event_rx,
            pending_auth: None,
            unsent_messages: UnsentMessages::default(),
            emote_bytes: UiImageCache::default(),
            previous_image_work: ImageWorkStats::default(),
            join_dialog: JoinDialog::default(),
            login_dialog: LoginDialog::default(),
            quick_switch: ChannelQuickSwitch::default(),
            emote_picker: EmotePicker::default(),
            chat_input_buf: String::new(),
            emote_catalog: Vec::new(),
            emote_catalog_initialized: false,
            emote_catalog_index: HashMap::new(),
            input_emote_catalogs: HashMap::new(),
            emote_picker_channel: None,
            pending_emote_refresh: VecDeque::new(),
            emote_refresh_followup: false,
            perf: PerfOverlay::default(),
            pending_reply: None,
            user_profile_popup: UserProfilePopup::default(),
            link_previews: HashMap::new(),
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
            whisper_composers: HashMap::new(),
            focus_whisper_composer: None,
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
            applied_ui_zoom: 0.0,
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

    fn activate_sidebar_control(&mut self) {
        match self.channel_layout {
            ChannelLayout::TopTabs => self.set_channel_layout(ChannelLayout::Sidebar),
            ChannelLayout::Sidebar => self.toggle_sidebar_visible(),
        }
    }

    fn show_channel_layout_controls(
        &mut self,
        ui: &mut egui::Ui,
        force_top_tabs: bool,
        compact: bool,
    ) -> (egui::Response, egui::Response) {
        let controls = ChannelLayoutControls::new(self.channel_layout, self.sidebar_visible, force_top_tabs);
        let sidebar = ui.add_enabled_ui(controls.sidebar_enabled, |ui| {
            chrome::icon_button(ui, ChromeIcon::Sidebar, controls.sidebar_label,
                IconButtonState { selected: controls.sidebar_selected, compact, ..Default::default() })
        }).inner.on_disabled_hover_text(controls.sidebar_label);
        if sidebar.clicked() {
            self.activate_sidebar_control();
        }
        let tabs = chrome::icon_button(ui, ChromeIcon::Tabs, "Use top tabs for channels",
            IconButtonState { selected: controls.tabs_selected, compact, ..Default::default() });
        if tabs.clicked() {
            self.set_channel_layout(ChannelLayout::TopTabs);
        }
        (sidebar, tabs)
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
            owner_user_id: None,
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

    fn mod_tools_owns_keyboard(&self, ctx: &Context) -> bool {
        let window_id = egui::Id::new("Moderation Tools");
        let layer = egui::LayerId::new(egui::Order::Middle, window_id);
        let active_id = window_id.with("keyboard_active");
        let expanded = egui::collapsing_header::CollapsingState::load(
            ctx, window_id.with("collapsing"),
        ).is_some_and(|state| state.is_open());
        if !self.mod_tools_open || !expanded
            || self.settings_filter_editor_modal.open
            || ctx.is_context_menu_open()
            || !ctx.data(|data| data.get_temp::<bool>(active_id).unwrap_or(false))
        {
            return false;
        }
        if ctx.memory(|m| m.focused().is_some()) {
            // A widget (filter input, unban draft) has focus -- let it consume keys.
            return false;
        }
        let layers: Vec<_> = ctx.memory(|memory| memory.layer_ids()
            .filter(|layer| matches!(layer.order, egui::Order::Middle | egui::Order::Foreground)
                && memory.areas().is_visible(layer))
            .collect());
        let top = layers.into_iter().rev().find(|layer| {
            egui::AreaState::load(ctx, layer.id).is_some_and(|state| state.interactable)
        });
        top == Some(layer)
    }

    /// Capture before other widgets apply Tab traversal. Queue actions are
    /// checked again after the moderation window is rendered.
    fn consume_mod_hotkey_input(&self, ctx: &Context) -> ModHotkey {
        use crust_core::HotkeyAction;
        if !self.mod_tools_open {
            return ModHotkey::None;
        }
        let window_id = egui::Id::new("Moderation Tools");
        let layer = egui::LayerId::new(egui::Order::Middle, window_id);
        if ctx.input(|input| input.pointer.any_pressed()) {
            let activated = ctx.pointer_interact_pos()
                .is_some_and(|pos| ctx.layer_id_at(pos) == Some(layer));
            ctx.data_mut(|data| data.insert_temp(window_id.with("keyboard_active"), activated));
        }
        if !self.mod_tools_owns_keyboard(ctx) {
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

    fn filtered_mod_tab_len(&self, tab: ModToolsTab, channel: &ChannelId) -> usize {
        let filter = self.mod_tools_filter.trim().to_ascii_lowercase();
        match tab {
            ModToolsTab::AutoMod => self.automod_queue.get(channel).map_or(0, |items| {
                items.iter().filter(|item| Self::automod_matches_filter(item, &filter)).count()
            }),
            ModToolsTab::UnbanRequests => self.unban_requests.get(channel).map_or(0, |items| {
                items.iter().filter(|item| Self::unban_matches_filter(item, &filter)).count()
            }),
            ModToolsTab::LowTrust => self.state.channels.get(channel).map_or(0, |state| {
                state.low_trust_users.iter()
                    .filter(|(login, entry)| Self::low_trust_matches_filter(login, entry, &filter))
                    .count()
            }),
        }
    }

    /// Render the tabbed moderation tools window.
    fn render_mod_tools_window(
        &mut self,
        ctx: &Context,
        moderation_channel: Option<ChannelId>,
        mut hotkey: ModHotkey,
    ) {
        let mut window_open = self.mod_tools_open;
        let mut refresh_channel: Option<ChannelId> = None;
        let mut automod_actions: Vec<(String, String, String)> = Vec::new();
        let mut automod_bulk_action: Option<String> = None;
        let mut unban_actions: Vec<(String, bool, Option<String>)> = Vec::new();
        let mut unban_bulk_action: Option<bool> = None;
        let mut low_trust_actions: Vec<LowTrustAction> = Vec::new();

        // Apply tab-switching / focus-nav hotkeys before rendering so the tab
        // strip and focus highlight reflect the new state this frame.
        if let Some(channel) = moderation_channel.as_ref() {
            let visible_count = self.filtered_mod_tab_len(self.mod_tools_active_tab, channel);
            self.mod_tools_focused_index = self.mod_tools_focused_index
                .min(visible_count.saturating_sub(1));
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
                    if visible_count > 0 {
                        self.mod_tools_focused_index = (self.mod_tools_focused_index + 1) % visible_count;
                    }
                }
                ModHotkey::FocusPrev => {
                    if visible_count > 0 {
                        self.mod_tools_focused_index = if self.mod_tools_focused_index == 0 {
                            visible_count - 1
                        } else {
                            self.mod_tools_focused_index - 1
                        };
                    }
                }
                _ => {}
            }
        }

        let window_response = egui::Window::new("Moderation Tools")
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
                            .id(egui::Id::new("moderation_filter"))
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
        if matches!(hotkey, ModHotkey::NextTab | ModHotkey::PrevTab) {
            // egui computes traversal at begin_pass even when its Tab event is
            // consumed before widgets render. Keep the window's keyboard owner.
            ctx.memory_mut(|memory| memory.stop_text_input());
        }
        let expanded = egui::collapsing_header::CollapsingState::load(
            ctx, egui::Id::new("Moderation Tools").with("collapsing"),
        ).is_some_and(|state| state.is_open());
        if !window_open || !expanded || window_response.as_ref().is_none_or(|window| window.inner.is_none()) {
            hotkey = ModHotkey::None;
            ctx.data_mut(|data| data.remove::<bool>(egui::Id::new("Moderation Tools").with("keyboard_active")));
        }
        if !self.mod_tools_owns_keyboard(ctx) {
            hotkey = ModHotkey::None;
        }

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
                        frame = frame.stroke(egui::Stroke::new(1.5_f32, t::accent()));
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
                        frame = frame.stroke(egui::Stroke::new(1.5_f32, t::accent()));
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
                        frame = frame.stroke(egui::Stroke::new(1.5_f32, t::accent()));
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

    fn clear_whisper_state(&mut self) {
        self.event_toasts.retain(|toast| toast.owner_user_id.is_none());
        self.event_toast_queue.retain(|toast| toast.owner_user_id.is_none());
        self.whispers_visible = false;
        self.whisper_threads.clear();
        self.whisper_display_names.clear();
        self.whisper_order.clear();
        self.whisper_unread.clear();
        self.whisper_unread_mentions.clear();
        self.active_whisper_login = None;
        self.whisper_composers.clear();
        self.focus_whisper_composer = None;
    }

    fn invalidate_input_emote_catalogs(&mut self, ctx: &Context) {
        self.input_emote_catalogs.clear();
        self.emote_picker.invalidate_catalog();
        invalidate_emote_completions(ctx, chat_composer_id(None));
        for pane in &self.split_panes.panes {
            invalidate_emote_completions(ctx, chat_composer_id(Some(pane.id)));
        }
    }

    fn ensure_input_emote_catalog(&mut self, channel: &ChannelId) {
        if self.input_emote_catalogs.contains_key(channel) {
            return;
        }
        let sender = self.state.auth.user_id.as_deref().unwrap_or_default();
        let mut entries: Vec<_> = self.emote_catalog_index.keys().filter_map(|code| {
            resolve_catalog_emote(&self.emote_catalog, &self.emote_catalog_index,
                channel, sender, code).cloned()
        }).collect();
        entries.sort_by_cached_key(|entry| entry.code.to_ascii_lowercase());
        self.input_emote_catalogs.insert(channel.clone(), entries);
    }

    fn prepare_emote_picker(&mut self, channel: &ChannelId) {
        self.ensure_input_emote_catalog(channel);
        if self.emote_picker_channel.as_ref() != Some(channel) {
            self.emote_picker.invalidate_catalog();
            self.emote_picker_channel = Some(channel.clone());
        }
    }

    fn tokenize_whisper_text(&self, text: &str, twitch_emotes: &[TwitchEmotePos], sender_user_id: &str) -> Vec<Span> {
        tokenize_whisper_text(text, twitch_emotes, &self.emote_catalog, &self.emote_catalog_index, sender_user_id)
    }

    fn queue_emote_refresh(&mut self) {
        if !self.pending_emote_refresh.is_empty() {
            // Keep reaching older rows while catalogs change. The queued work
            // reads the latest catalog, then one follow-up pass revisits rows
            // that were processed before the newest snapshot arrived.
            self.emote_refresh_followup = true;
            return;
        }
        self.emote_refresh_followup = false;
        let mut seen = HashSet::new();
        for channel in self.state.active_channel.iter().chain(self.state.channel_order.iter()) {
            if seen.insert(channel) {
                if let Some(message) = self.state.channels.get(channel).and_then(|state| state.messages.back()) {
                    self.pending_emote_refresh.push_back(EmoteRefreshJob::Channel(channel.clone(), message.id));
                }
            }
        }
        if let Some(message) = self.state.mentions.back() {
            self.pending_emote_refresh.push_back(EmoteRefreshJob::Mentions(message.id));
        }
        for (login, lines) in &self.whisper_threads {
            if !lines.is_empty() {
                self.pending_emote_refresh.push_back(EmoteRefreshJob::Whispers(login.clone()));
            }
        }
    }

    fn refresh_emote_messages(&mut self, ctx: &Context) -> usize {
        let mut remaining = EMOTE_REFRESH_ROWS_PER_FRAME;
        let mut jobs = 0;
        while remaining > 0 && jobs < 16 {
            let Some(job) = self.pending_emote_refresh.pop_front() else { break };
            jobs += 1;
            match job {
                EmoteRefreshJob::Channel(channel, through) => {
                    let Some(state) = self.state.channels.get_mut(&channel) else { continue };
                    let (examined, next, changed) = refresh_message_emotes(&mut state.messages,
                        through, remaining, &self.emote_catalog, &self.emote_catalog_index);
                    remaining -= examined;
                    if !changed.is_empty() {
                        crate::widgets::message_list::invalidate_message_layouts(ctx, &channel, &changed);
                        self.global_search.invalidate();
                    }
                    if let Some(next) = next {
                        self.pending_emote_refresh.push_back(EmoteRefreshJob::Channel(channel, next));
                    }
                }
                EmoteRefreshJob::Mentions(through) => {
                    let (examined, next, _) = refresh_message_emotes(&mut self.state.mentions,
                        through, remaining, &self.emote_catalog, &self.emote_catalog_index);
                    remaining -= examined;
                    if let Some(next) = next {
                        self.pending_emote_refresh.push_back(EmoteRefreshJob::Mentions(next));
                    }
                }
                EmoteRefreshJob::Whispers(login) => {
                    let Some(lines) = self.whisper_threads.get_mut(&login) else { continue };
                    if lines.len() > remaining {
                        self.pending_emote_refresh.push_front(EmoteRefreshJob::Whispers(login));
                        break;
                    }
                    remaining -= lines.len();
                    for line in lines {
                        line.spans = tokenize_whisper_text(&line.text, &line.twitch_emotes,
                            &self.emote_catalog, &self.emote_catalog_index, &line.from_user_id);
                    }
                }
            }
        }
        if self.pending_emote_refresh.is_empty() && self.emote_refresh_followup {
            self.queue_emote_refresh();
        }
        if !self.pending_emote_refresh.is_empty() {
            ctx.request_repaint();
        }
        EMOTE_REFRESH_ROWS_PER_FRAME - remaining
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
        self.global_search.invalidate();
        let nicknames = &self.nicknames;
        for (channel, ch_state) in self.state.channels.iter_mut() {
            for msg in ch_state.messages.iter_mut() {
                crust_core::model::apply_sender_nickname(
                    nicknames,
                    channel.display_name(),
                    &mut msg.sender,
                );
            }
        }
        for msg in &mut self.state.mentions {
            crust_core::model::apply_sender_nickname(
                nicknames,
                msg.channel.display_name(),
                &mut msg.sender,
            );
        }
        self.user_profile_popup.apply_nicknames(nicknames);
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
        let Some(partner_login) = Self::normalize_whisper_login(partner_login) else {
            return;
        };
        self.whisper_threads
            .entry(partner_login.clone())
            .or_default();
        if !self.whisper_order.contains(&partner_login) {
            self.touch_whisper_thread_order(&partner_login);
        }
        self.whisper_composers
            .entry(partner_login.clone())
            .or_default();
        self.activate_whisper_thread(&partner_login);
        self.focus_whisper_composer = Some(partner_login);
    }

    fn whisper_composer_id(&self, partner_login: &str) -> egui::Id {
        egui::Id::new((
            "whisper_composer",
            self.state.auth.user_id.as_deref(),
            partner_login,
        ))
    }

    fn fail_pending_whispers(&mut self, message: &str) {
        let mut failed_recipients = Vec::new();
        for (recipient, composer) in &mut self.whisper_composers {
            if !composer.pending.is_empty() {
                failed_recipients.push(recipient.clone());
            }
            for text in composer.pending.clone() {
                composer.fail_attempt(&text, message);
            }
        }
        for recipient in failed_recipients {
            self.show_failed_whisper(&recipient);
        }
    }

    fn show_failed_whisper(&mut self, recipient: &str) {
        self.whisper_threads
            .entry(recipient.to_owned())
            .or_default();
        if !self.whisper_order.iter().any(|thread| thread == recipient) {
            self.touch_whisper_thread_order(recipient);
        }
        if !self.whispers_visible {
            self.activate_whisper_thread(recipient);
        }
    }

    fn render_whisper_composer(&mut self, ui: &mut egui::Ui, partner_login: &str) {
        let mut composer = self
            .whisper_composers
            .remove(partner_login)
            .unwrap_or_default();
        let signed_in = self.state.auth.logged_in
            && self
                .state
                .auth
                .user_id
                .as_ref()
                .is_some_and(|id| !id.is_empty());
        let ready = signed_in && self.pending_auth.is_none();
        let editor = ui.add_enabled(
            ready,
            egui::TextEdit::singleline(&mut composer.text)
                .id(self.whisper_composer_id(partner_login))
                .desired_width(ui.available_width())
                .hint_text(format!("Whisper to @{partner_login}")),
        );
        if self.focus_whisper_composer.as_deref() == Some(partner_login) && ready {
            self.focus_whisper_composer = None;
            editor.request_focus();
        }
        let enter = editor.lost_focus()
            && ui.input(|input| input.key_pressed(egui::Key::Enter) && input.modifiers.is_none());
        let count = composer.text.trim().chars().count();
        let can_send = ready && composer.pending.is_empty() && (1..=500).contains(&count);
        let mut send = enter && can_send;
        ui.horizontal(|ui| {
            send |= ui
                .add_enabled(can_send, egui::Button::new("Send"))
                .clicked();
            if !signed_in {
                if ui.button("Sign in").clicked() && !self.login_dialog.open {
                    self.login_dialog.toggle();
                }
            } else if self.pending_auth.is_some() {
                ui.label(RichText::new("Signing in…").font(t::tiny()));
            } else if !composer.pending.is_empty() {
                ui.label(RichText::new("Sending…").font(t::tiny()));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{count}/500")).font(t::tiny()).color(
                    if count > 500 {
                        t::red()
                    } else {
                        t::text_muted()
                    },
                ))
                .on_hover_text("Whispers are limited to 500 characters.");
            });
        });
        if let Some(failed) = composer.failed.front() {
            let mut dismiss = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Send failed").font(t::tiny()).color(t::red()))
                    .on_hover_text(&failed.message);
                if composer.text == failed.text {
                    dismiss = ui.small_button("Dismiss").clicked();
                } else if ui
                    .small_button("Copy text")
                    .on_hover_text(&failed.message)
                    .clicked()
                {
                    ui.ctx().copy_text(failed.text.clone());
                    dismiss = true;
                }
            });
            if dismiss {
                composer.failed.pop_front();
            }
        }
        if enter && ready {
            editor.request_focus();
        }
        if send {
            let text = composer.text.clone();
            if self.try_send_cmd(AppCommand::SendWhisper {
                target_login: partner_login.to_owned(),
                text: text.clone(),
            }) {
                composer.pending.push(text);
            } else {
                composer.failed.push_back(FailedWhisper {
                    text,
                    message: "The message could not be queued. Your draft was kept.".to_owned(),
                });
            }
            editor.request_focus();
        }
        self.whisper_composers
            .insert(partner_login.to_owned(), composer);
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
                            egui::Stroke::new(1.0_f32, t::accent())
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
                                                compact_badge_count(unread_mentions),
                                                t::text_primary(),
                                                t::mention_pill_bg(),
                                            );
                                        } else if unread > 0 {
                                            chrome::pill(
                                                ui,
                                                compact_badge_count(unread),
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

        if !compact_layout {
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
        }

        let row_height = ui.spacing().interact_size.y.max(
            ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().button_padding.y * 2.0,
        );
        let has_failure = self.whisper_composers
            .get(current_thread)
            .is_some_and(|composer| !composer.failed.is_empty());
        let composer_height = row_height * 2.0
            + ui.spacing().item_spacing.y
            + if has_failure {
                row_height + ui.spacing().item_spacing.y
            } else {
                0.0
            };
        egui::TopBottomPanel::bottom(egui::Id::new(("whisper_footer", current_thread)))
            .frame(egui::Frame::NONE)
            .exact_height(composer_height)
            .show_inside(ui, |ui| self.render_whisper_composer(ui, current_thread));

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
                            .id_salt(("whisper_messages", current_thread, compact_layout))
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
                                    }.min(ui.available_width());
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
                                                            .stroke(egui::Stroke::new(1.0_f32, self_stroke))
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
                                                            .stroke(egui::Stroke::new(1.0_f32, other_stroke))
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

        let bounds = ctx.screen_rect().shrink(8.0);
        let style = ctx.style();
        let frame = egui::Frame::window(&style);
        let title_height = ctx
            .fonts(|fonts| fonts.row_height(&egui::TextStyle::Heading.resolve(&style)))
            .max(style.spacing.interact_size.y)
            + frame.inner_margin.sum().y
            + frame.stroke.width;
        let max_size = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
            .max(egui::Vec2::splat(1.0));
        egui::Window::new("Whispers")
            .open(&mut window_open)
            .default_size(egui::vec2(760.0, 500.0).min(max_size))
            .min_size(egui::vec2(240.0, 180.0).min(max_size))
            .max_size(max_size)
            .constrain_to(bounds)
            .collapsible(false)
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
                    let display_name = self.whisper_display_names.get(&current_thread)
                        .map(String::as_str).unwrap_or(&current_thread);
                    egui::ComboBox::from_id_salt("whisper_thread_selector")
                        .width(ui.available_width())
                        .selected_text(format!("@{display_name}"))
                        .truncate()
                        .show_ui(ui, |ui| {
                            for thread in &thread_order {
                                let display = self.whisper_display_names.get(thread)
                                    .map(String::as_str).unwrap_or(thread);
                                let unread = self.whisper_unread.get(thread).copied().unwrap_or(0);
                                let label = if unread > 0 { format!("@{display} ({unread})") }
                                    else { format!("@{display}") };
                                if ui.selectable_label(current_thread == *thread, label).clicked() {
                                    current_thread.clone_from(thread);
                                    select_thread = Some(thread.clone());
                                }
                            }
                        });
                    self.render_whisper_conversation_panel(
                        ui,
                        &current_thread,
                        &mut mark_read_thread,
                        &mut compose_thread,
                        true,
                    );
                } else {
                    ui.allocate_ui_with_layout(ui.available_size(),
                        egui::Layout::left_to_right(egui::Align::Min), |ui| {
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
        self.global_search.on_event(&evt);

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
                    ConnectionState::Disconnected | ConnectionState::Error(_) => {
                        self.loading_screen.on_event(LoadEvent::Disconnected)
                    }
                }
            }
            AppEvent::LoggedOut => self.loading_screen.on_event(LoadEvent::Disconnected),
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
            AppEvent::HistoryLoaded { channel, messages, .. } => {
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
                self.input_emote_catalogs.remove(&channel);
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
                if self.state.redirect_channel(&old_channel, &new_channel) {
                    self.split_panes.redirect_channel(&old_channel, &new_channel);
                    self.sorted_chatters.remove(&old_channel);
                    self.sorted_chatters.remove(&new_channel);
                    self.chatters_dirty.remove(&old_channel);
                    self.chatters_dirty.insert(new_channel.clone());
                    if let Some(queue) = self.automod_queue.remove(&old_channel) {
                        let destination = self.automod_queue.entry(new_channel.clone()).or_default();
                        for item in queue {
                            if !destination.iter().any(|existing| existing.message_id == item.message_id) {
                                destination.push(item);
                            }
                        }
                    }
                    if let Some(requests) = self.unban_requests.remove(&old_channel) {
                        let destination = self.unban_requests.entry(new_channel.clone()).or_default();
                        for item in requests {
                            if !destination.iter().any(|existing| existing.request_id == item.request_id) {
                                destination.push(item);
                            }
                        }
                    }
                    if let Some(search) = self.message_search.remove(&old_channel) {
                        self.message_search.entry(new_channel.clone()).or_insert(search);
                    }
                    if let Some(message_id) = self.pending_scroll_to_message.remove(&old_channel) {
                        self.pending_scroll_to_message.entry(new_channel.clone()).or_insert(message_id);
                    }
                    if self.pending_active_snap.remove(&old_channel) {
                        self.pending_active_snap.insert(new_channel.clone());
                    }
                    let old_prefix = format!("{}::", old_channel.as_str());
                    let draft_keys: Vec<_> = self.unban_resolution_drafts.keys()
                        .filter(|key| key.starts_with(&old_prefix)).cloned().collect();
                    for key in draft_keys {
                        if let Some(draft) = self.unban_resolution_drafts.remove(&key) {
                            let suffix = &key[old_prefix.len()..];
                            self.unban_resolution_drafts
                                .entry(format!("{}::{suffix}", new_channel.as_str())).or_insert(draft);
                        }
                    }
                    if let Some(reply) = self.pending_reply.as_mut() {
                        if reply.channel == old_channel {
                            reply.channel = new_channel;
                        }
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
                crust_core::model::apply_sender_nickname(
                    &self.nicknames,
                    channel.display_name(),
                    &mut message.sender,
                );

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
                if highlight_rule.is_some() && !message.flags.is_highlighted {
                    message.flags.is_highlighted = true;
                    if !message.flags.is_history {
                        self.send_cmd(AppCommand::PersistMessageHighlight {
                            channel: channel.clone(),
                            server_id: message.server_id.clone(),
                            message_id: message.id,
                            timestamp_ms: message.timestamp.timestamp_millis(),
                        });
                    }
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
                if let Some(toast) = EventToast::from_message(is_active, &message) {
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
                                if ch.chatters.len() < MAX_TRACKED_CHATTERS
                                    && !ch.chatters.contains(display_name)
                                {
                                    ch.chatters.insert(display_name.to_owned());
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
                    let absorbed = (message.flags.is_self
                        && message.server_id.is_some()
                        && ch.absorb_own_echo(&message))
                        || ch.absorb_moderation_notice(&message);
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
                owner_user_id,
                from_user_id,
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
                let active_login = self.state.auth.username.as_deref()
                    .and_then(Self::normalize_whisper_login);
                let owner = if is_self { from_login_norm.as_ref() } else { target_login_norm.as_ref() };
                if !self.state.auth.logged_in || owner.is_none() || owner != active_login.as_ref()
                    || owner_user_id.is_empty() || self.state.auth.user_id.as_deref() != Some(owner_user_id.as_str())
                    || (is_self && from_user_id != owner_user_id)
                {
                    return;
                }
                let partner_login = if is_self {
                    target_login_norm
                } else {
                    from_login_norm
                };
                let Some(partner_login) = partner_login else {
                    return;
                };

                if is_self && !is_history {
                    if let Some(composer) = self.whisper_composers.get_mut(&partner_login) {
                        composer.acknowledge(&text);
                    }
                }

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
                let spans = self.tokenize_whisper_text(&text, &twitch_emotes, &from_user_id);
                let mentions_current_user =
                    self.whisper_message_mentions_current_user(&spans, &text);
                let notification_body_text = text.trim().to_owned();

                let line = WhisperLine {
                    from_user_id,
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
                } else if !is_history && !is_self {
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
                self.emote_bytes.complete(ctx, uri, width, height, raw_bytes);
            }
            AppEvent::EmoteCatalogUpdated { mut emotes } => {
                emotes.sort_by(|left, right| left.code.cmp(&right.code)
                    .then_with(|| left.provider.cmp(&right.provider))
                    .then_with(|| left.channel.as_ref().map(ChannelId::as_str).cmp(&right.channel.as_ref().map(ChannelId::as_str)))
                    .then_with(|| left.user_id.as_ref().map(|id| id.0.as_str()).cmp(&right.user_id.as_ref().map(|id| id.0.as_str())))
                    .then_with(|| left.url.cmp(&right.url))
                    .then_with(|| left.id.cmp(&right.id))
                    .then_with(|| left.url_hd.cmp(&right.url_hd))
                    .then_with(|| left.scope.cmp(&right.scope)));
                if self.emote_catalog_initialized && self.emote_catalog == emotes {
                    return;
                }
                self.emote_catalog_initialized = true;
                self.emote_catalog = emotes;
                self.emote_catalog_index = build_emote_catalog_index(&self.emote_catalog);
                self.invalidate_input_emote_catalogs(ctx);

                self.queue_emote_refresh();
            }
            AppEvent::AuthenticationPending { username } => {
                self.pending_auth = Some(username.unwrap_or_default());
            }
            AppEvent::MessageSendRejected { channel, text, reply_to_msg_id, reply, reason } => {
                let message = UnsentMessage { channel, text, reply_to_msg_id, reply, reason };
                let editing_this_frame = ctx.input(|input| input.events.iter().any(|event| {
                    matches!(event, egui::Event::Text(_) | egui::Event::Paste(_)
                        | egui::Event::Key { pressed: true, .. })
                }));
                if !editing_this_frame && self.restore_unsent_message(&message) {
                    self.push_event_toast(
                        format!("Message to #{} was not sent. Your draft was restored. {}", message.channel.display_name(), message.reason),
                        t::red(),
                        false,
                    );
                } else {
                    self.unsent_messages.push(message);
                }
            }
            AppEvent::Authenticated { username, user_id } => {
                let old_login = self.state.auth.username.as_deref()
                    .and_then(Self::normalize_whisper_login);
                if old_login != Self::normalize_whisper_login(&username)
                    || self.state.auth.user_id.as_deref() != Some(user_id.as_str())
                {
                    self.clear_whisper_state();
                }
                self.pending_auth = None;
                self.auth_refresh_inflight = false;
                // Clear the previous account's avatar so it doesn't flash
                // while the new one is fetched.
                self.state.auth.avatar_url = None;
                self.state.auth.logged_in = true;
                self.state.auth.username = Some(username);
                self.state.auth.user_id = Some(user_id);
                self.invalidate_input_emote_catalogs(ctx);
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
                self.pending_auth = None;
                self.auth_refresh_inflight = false;
                self.state.auth.logged_in = false;
                self.state.auth.username = None;
                self.state.auth.user_id = None;
                self.invalidate_input_emote_catalogs(ctx);
                self.state.auth.avatar_url = None;
                self.clear_whisper_state();
                if let Some(host) = plugin_host() {
                    host.update_auth_snapshot(PluginAuthSnapshot::default());
                }
            }
            AppEvent::WhisperSendFailed { owner_user_id, target_login, text, message } => {
                if !self.state.auth.logged_in || owner_user_id.is_empty()
                    || self.state.auth.user_id.as_deref() != Some(owner_user_id.as_str())
                {
                    return;
                }
                if let Some(partner) = Self::normalize_whisper_login(&target_login) {
                    let matched = self.whisper_composers.get_mut(&partner)
                        .is_some_and(|composer| composer.fail_attempt(&text, &message));
                    if matched {
                        self.show_failed_whisper(&partner);
                    }
                }
                self.enqueue_event_toast(EventToast {
                    text: message,
                    owner_user_id: Some(owner_user_id),
                    hue: t::red(),
                    confetti: false,
                    born: std::time::Instant::now(),
                });
            }
            AppEvent::Error { context, message } => {
                if matches!(context.as_str(), "Login" | "AddAccount" | "SwitchAccount") {
                    self.pending_auth = None;
                }
                if context == "RefreshAuth" {
                    self.auth_refresh_inflight = false;
                }
                if context == "SendWhisper" {
                    let had_pending = self.whisper_composers.values()
                        .any(|composer| !composer.pending.is_empty());
                    self.fail_pending_whispers(&message);
                    if had_pending && self.state.auth.logged_in {
                        if let Some(owner) = self.state.auth.user_id.as_ref().filter(|id| !id.is_empty()) {
                            self.enqueue_event_toast(EventToast {
                                text: message,
                                owner_user_id: Some(owner.clone()),
                                hue: t::red(),
                                confetti: false,
                                born: std::time::Instant::now(),
                            });
                        }
                    }
                    return;
                }
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
            AppEvent::MentionsLoaded { mut messages } => {
                // Backfill the cross-channel Mentions pseudo-tab from the
                // SQLite log on startup. Merge by timestamp so live mentions
                // already accumulated during startup interleave correctly.
                for message in &mut messages {
                    crust_core::model::apply_sender_nickname(
                        &self.nicknames,
                        message.channel.display_name(),
                        &mut message.sender,
                    );
                }
                let refresh_ids: HashSet<_> = messages.iter().map(|message| message.id).collect();
                self.state.prepend_mentions_history(messages);
                if self.emote_catalog_initialized {
                    if let Some(message) = self.state.mentions.iter().rev()
                        .find(|message| refresh_ids.contains(&message.id))
                    {
                        self.pending_emote_refresh.push_back(EmoteRefreshJob::Mentions(message.id));
                    }
                }
            }
            AppEvent::HistoryLoaded { channel, mut messages, local } => {
                for message in &mut messages {
                    crust_core::model::apply_sender_nickname(
                        &self.nicknames,
                        channel.display_name(),
                        &mut message.sender,
                    );
                }
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    if let Some(page) = &local {
                        let stale = match (page.before, ch.local_history_cursor) {
                            (None, Some(_)) => true,
                            (Some(before), Some(current)) => before != current,
                            _ => false,
                        };
                        if stale {
                            return;
                        }
                        ch.local_history_cursor = page.next.or(ch.local_history_cursor);
                        ch.local_history_exhausted = !page.has_more;
                        self.global_search.load_older_pending.remove(&channel);
                    }
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

                    let refresh_ids: HashSet<_> = messages.iter().map(|message| message.id).collect();
                    if let Some(page) = &local {
                        ch.prepend_local_history(messages, page);
                    } else {
                        ch.prepend_history(messages);
                    }
                    if self.emote_catalog_initialized {
                        if let Some(message) = ch.messages.iter().rev()
                            .find(|message| refresh_ids.contains(&message.id))
                        {
                            self.pending_emote_refresh.push_back(
                                EmoteRefreshJob::Channel(channel.clone(), message.id));
                        }
                    }

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

                    let popup_login = profile.login.clone();
                    let popup_channel = self.user_profile_popup.channel.clone();
                    self.user_profile_popup.set_profile(profile);
                    self.user_profile_popup.apply_nicknames(&self.nicknames);

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
            AppEvent::IvrLogsLoaded { channel, username, request_id, messages } => {
                self.user_profile_popup.apply_ivr_logs_result(
                    &channel, &username, request_id, Ok(messages),
                );
            }
            AppEvent::IvrLogsFailed { channel, username, request_id, error } => {
                self.user_profile_popup.apply_ivr_logs_result(
                    &channel, &username, request_id, Err(error),
                );
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
            AppEvent::ChannelChatCleared { channel } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.delete_all_chat_messages();
                }
            }
            AppEvent::SelfAvatarLoaded { account, avatar_url } => {
                let account = Self::normalize_whisper_login(&account);
                if self.state.auth.logged_in
                    && account.is_some()
                    && self.state.auth.username.as_deref()
                        .and_then(Self::normalize_whisper_login) == account
                {
                    self.state.auth.avatar_url = Some(avatar_url);
                }
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
                        text: "\u{26a0}\u{fe0f} Your Twitch token has expired. Use the account button in the top bar, then Add Account, to sign in with a new Twitch OAuth token.".into(),
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
                name_paint,
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
                    self.stv_avatars.insert(user_id.clone(), normalized);
                } else {
                    self.stv_avatars.remove(&user_id);
                }

                // Only the most-recent messages per channel are visible /
                // about-to-be-visible after virtualized scrolling.  Walking
                // every cached message across every channel for every new
                // sender cosmetic event is O(N * M) and is a major freeze
                // source on busy channels (many users resolve 7TV styles at
                // once).  Bound to the tail window.
                const COSMETICS_BACKFILL_TAIL: usize = 400;
                let apply = |sender: &mut crust_core::model::Sender| -> bool {
                    if sender.user_id.0 != user_id {
                        return false;
                    }
                    if let Some(ref c) = color {
                        sender.color = Some(c.clone());
                    }
                    let mut changed = sender.name_paint != name_paint;
                    if changed {
                        sender.name_paint = name_paint.clone();
                    }
                    if let Some(ref b) = badge {
                        if let Some(existing) = sender.badges.iter_mut()
                            .find(|badge| badge.name.eq_ignore_ascii_case("7tv"))
                        {
                            if existing != b {
                                *existing = b.clone();
                                changed = true;
                            }
                        } else {
                            sender.badges.insert(0, b.clone());
                            changed = true;
                        }
                    } else {
                        let before = sender.badges.len();
                        sender.badges.retain(|badge| !badge.name.eq_ignore_ascii_case("7tv"));
                        changed |= before != sender.badges.len();
                    }
                    changed
                };
                let mut any_changed = false;
                for (channel, ch) in &mut self.state.channels {
                    if !channel.is_twitch() {
                        continue;
                    }
                    let mut changed = Vec::new();
                    for msg in ch.messages.iter_mut().rev().take(COSMETICS_BACKFILL_TAIL) {
                        if apply(&mut msg.sender) {
                            changed.push(msg.id);
                        }
                    }
                    any_changed |= !changed.is_empty();
                    crate::widgets::message_list::invalidate_message_layouts(ctx, channel, &changed);
                }
                let mut changed_mentions = Vec::new();
                for msg in self.state.mentions.iter_mut().rev().take(COSMETICS_BACKFILL_TAIL) {
                    if msg.channel.is_twitch() && apply(&mut msg.sender) {
                        changed_mentions.push(msg.id);
                    }
                }
                any_changed |= !changed_mentions.is_empty();
                crate::widgets::message_list::invalidate_message_layouts(
                    ctx, &ChannelId::mentions(), &changed_mentions);
                if any_changed {
                    self.global_search.invalidate();
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
            AppEvent::HypeTrainCleared { channel, checked_at } => {
                self.state.clear_hype_train_if_stale(&channel, checked_at);
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
                self.state.apply_live_snapshot(channels);
            }
            AppEvent::LiveFeedError { message } => {
                self.state.apply_live_error(message);
            }
            AppEvent::LiveFeedPartialUpdate { channels, error } => {
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
        if self.split_panes.panes.len() > 1 {
            if let Some(pane) = self.split_panes.panes.iter_mut().find(|p| &p.channel == channel) {
                return &mut pane.input_buf;
            }
        }
        &mut self.chat_input_buf
    }

    fn send_cmd(&self, cmd: AppCommand) {
        self.try_send_cmd(cmd);
    }

    /// Returns false when the action was rejected, so editors can keep drafts.
    fn try_send_cmd(&self, cmd: AppCommand) -> bool {
        self.cmd_tx.try_send(cmd).is_ok()
    }

    fn request_ivr_logs(&mut self, channel: String, username: String) -> bool {
        let Some(request_id) = self.user_profile_popup
            .begin_ivr_logs_request(&channel, &username) else {
            return false;
        };
        if let Err(error) = self.cmd_tx.try_send(AppCommand::FetchIvrLogs {
            channel: channel.clone(),
            username: username.clone(),
            request_id: Some(request_id),
        }) {
            self.user_profile_popup.apply_ivr_logs_result(
                &channel, &username, Some(request_id), Err(error.message().to_owned()),
            );
            return false;
        }
        true
    }

    fn service_runtime_channels(&mut self, ctx: &Context) -> u32 {
        // Events can enqueue more commands. Drain events first so a runtime
        // waiting to publish one can resume and consume its command channel.
        let events = self.drain_events(ctx);
        if self.cmd_tx.sender.is_closed() {
            self.fail_pending_whispers(CommandQueueError::Closed.message());
            self.user_profile_popup.abort_ivr_logs_request(CommandQueueError::Closed.message());
        }
        if let Some(error) = self.cmd_tx.flush(ctx) {
            if error == CommandQueueError::Closed {
                self.fail_pending_whispers(error.message());
            }
            warn!("{}", error.message());
            self.push_event_toast(error.message().to_owned(), t::red(), false);
        }
        events
    }

    fn reply_for_composer(&self, channel: &ChannelId, pane: Option<usize>) -> Option<ReplyInfo> {
        if pane.is_some_and(|pane| pane != self.split_panes.focused) {
            return None;
        }
        self.pending_reply.as_ref().filter(|reply| &reply.channel == channel)
            .map(|reply| reply.info.clone())
    }

    fn unsent_input_pane(&self, message: &UnsentMessage) -> Option<Option<usize>> {
        if self.pending_reply.is_some() {
            return None;
        }
        if self.split_panes.panes.len() <= 1 {
            return (self.state.active_channel.as_ref() == Some(&message.channel)
                && self.chat_input_buf.is_empty()).then_some(None);
        }
        self.split_panes.panes.iter().enumerate()
            .filter(|(index, pane)| pane.channel == message.channel
                && pane.input_buf.is_empty()
                && ((message.reply.is_none() && message.reply_to_msg_id.is_none())
                    || *index == self.split_panes.focused))
            .min_by_key(|(index, _)| usize::from(*index != self.split_panes.focused))
            .map(|(index, _)| Some(index))
    }

    fn restore_unsent_message(&mut self, message: &UnsentMessage) -> bool {
        let Some(pane) = self.unsent_input_pane(message) else {
            return false;
        };
        if let Some(pane) = pane {
            self.split_panes.panes[pane].input_buf.clone_from(&message.text);
        } else {
            self.chat_input_buf.clone_from(&message.text);
        }
        self.pending_reply = message.reply_info().map(|info| PendingReply {
            channel: message.channel.clone(),
            info,
        });
        true
    }

    /// Dispatch either composer through the same validation and queue path.
    /// A rejected submission restores the exact draft in its original pane.
    fn submit_chat_input(
        &mut self,
        channel: &ChannelId,
        draft: String,
        reply: Option<ReplyInfo>,
        input_pane: Option<usize>,
    ) -> bool {
        let text = draft.trim();
        let can_moderate = self.state.channels.get(channel).is_some_and(|ch| ch.is_mod)
            || self.state.auth.username.as_deref().is_some_and(|login| {
                login.eq_ignore_ascii_case(channel.display_name())
            });
        let chatters_count = self.state.channels.get(channel).map_or(0, |ch| {
            ch.chatters.len().max(estimate_chatter_count(ch))
        });
        let command = match self.expand_outgoing_aliases(text, channel) {
            Err(error) => error,
            Ok(expanded) => {
                let outgoing = expanded.as_deref().unwrap_or(text);
                let live_channels = collect_live_channel_entries(&self.stream_statuses);
                let parsed = parse_slash_command(
                    outgoing,
                    channel,
                    reply.as_ref().map(|reply| reply.parent_msg_id.clone()),
                    reply.clone(),
                    can_moderate,
                    chatters_count,
                    self.kick_beta_enabled,
                    self.irc_beta_enabled,
                    self.state.auth.username.as_deref(),
                    &live_channels,
                );
                if self.state.auth.logged_in {
                    parsed.unwrap_or_else(|| AppCommand::SendMessage {
                        channel: channel.clone(),
                        text: outgoing.to_owned(),
                        reply_to_msg_id: reply.as_ref().map(|reply| reply.parent_msg_id.clone()),
                        reply: reply.clone(),
                    })
                } else {
                    match parsed {
                        Some(command) if is_anonymous_local_command(&command) => command,
                        Some(_) => AppCommand::InjectLocalMessage {
                            channel: channel.clone(),
                            text: "Anonymous mode allows local slash commands only. Log in to run server commands or send chat messages. Try /help.".to_owned(),
                        },
                        None => AppCommand::InjectLocalMessage {
                            channel: channel.clone(),
                            text: if outgoing.trim_start().starts_with('/') {
                                "That slash command is not available in anonymous mode. Use /help for local commands."
                            } else {
                                "Anonymous mode cannot send chat messages. Log in to chat, or run local commands like /help."
                            }.to_owned(),
                        },
                    }
                }
            }
        };

        let irc_text = match &command {
            AppCommand::SendMessage { text, .. } if channel.is_irc() => Some(text.clone()),
            _ => None,
        };
        let user_card = match &command {
            AppCommand::ShowUserCard { login, channel } => Some((login.clone(), channel.clone())),
            _ => None,
        };
        let whisper = match &command {
            AppCommand::SendWhisper { target_login, text } => Some((target_login.clone(), text.clone())),
            _ => None,
        };
        if !self.try_send_cmd(command) {
            if let Some(pane) = input_pane {
                self.split_panes.panes[pane].input_buf = draft;
            } else {
                self.chat_input_buf = draft;
            }
            return false;
        }

        if let Some((recipient, text)) = whisper {
            if let Some(recipient) = Self::normalize_whisper_login(&recipient) {
                self.whisper_composers.entry(recipient).or_default().pending.push(text);
            }
        }
        if reply.is_some() {
            self.pending_reply = None;
        }
        if self.message_history.last().map(String::as_str) != Some(text) {
            self.message_history.push(text.to_owned());
            if self.message_history.len() > 100 {
                self.message_history.remove(0);
            }
        }
        self.record_slash_usage_from_text(text);
        if let Some(text) = irc_text {
            self.irc_status_panel.note_outgoing(channel, &text);
        }
        if let Some((login, channel)) = user_card {
            self.user_profile_popup
                .set_loading(&login, vec![], Some(channel), can_moderate);
        }
        true
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
        let state = self.state.channels.get(channel);
        if state.is_some_and(|state| state.local_history_exhausted) {
            return;
        }
        let cursor = state.and_then(|state| state.local_history_cursor)
            .unwrap_or(crust_core::model::HistoryCursor {
                timestamp_ms: oldest_loaded_ts_ms,
                row_id: i64::MAX,
            });
        self.send_cmd(AppCommand::LoadOlderLocalHistory {
            channel: channel.clone(),
            before_ts_ms: cursor.timestamp_ms,
            before_row_id: Some(cursor.row_id),
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
        self.pending_scroll_to_message.insert(channel.clone(), msg_id);
        self.activate_channel(channel);
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

        self.emote_bytes.static_texture(ui.ctx(), url)
    }

    fn show_topbar_account_button(
        &mut self,
        ui: &mut egui::Ui,
        compact_account: bool,
        ultra_compact: bool,
    ) {
        if let Some(username) = self.pending_auth.as_ref() {
            let tooltip = if username.is_empty() {
                "Validating your account...".to_owned()
            } else {
                format!("Signing in as @{username}...")
            };
            let response = if compact_account {
                chrome::icon_button(ui, ChromeIcon::Account, &tooltip, IconButtonState {
                    compact: ultra_compact,
                    ..Default::default()
                })
            } else {
                ui.add(egui::Button::new(RichText::new("Signing in...").font(t::topbar_font())))
                    .on_hover_text(tooltip)
            };
            if response.clicked() {
                self.login_dialog.toggle();
            }
            return;
        }
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
                        egui::Stroke::new(1.0_f32, border),
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

        if (ui - self.applied_ui_zoom).abs() > f32::EPSILON {
            ctx.set_zoom_factor(ui);
            self.applied_ui_zoom = ui;
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
        let Some(channel) = self.active_search_target() else {
            return;
        };

        if open_search {
            self.message_search_mut(&channel).request_open();
        }
        if let Some(search) = self.message_search.get_mut(&channel) {
            if search.open
                && search.owns_keyboard(ctx)
                && !ctx.is_context_menu_open()
                && ctx.input_mut(|i| consume_exact_key(i, egui::Modifiers::NONE, egui::Key::Escape))
            {
                search.close();
            }
        }
    }

    fn handle_settings_hotkey_capture(&mut self, ctx: &Context) -> bool {
        if !self.settings_open || self.settings_section != SettingsSection::Hotkeys {
            self.hotkey_capture_target = None;
            return false;
        }
        let capturing = handle_hotkey_capture(ctx, &mut self.hotkey_capture_target,
            &mut self.settings_hotkey_bindings);
        if capturing && self.settings_hotkey_bindings != self.hotkey_bindings {
            self.hotkey_bindings = self.settings_hotkey_bindings.clone();
            self.send_cmd(AppCommand::SetHotkeyBindings {
                bindings: self.settings_hotkey_bindings.to_pairs(),
            });
        }
        capturing
    }

    fn open_channel_quick_switch(&mut self) {
        self.quick_switch.open = true;
        self.quick_switch.query.clear();
        self.quick_switch.selected = 0;
        self.quick_switch.focus_query = true;
        self.quick_switch.scroll_to_selected = true;
    }

    fn close_channel_quick_switch(&mut self) {
        self.quick_switch.open = false;
        self.quick_switch.focus_query = false;
        self.quick_switch.scroll_to_selected = false;
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
            let prefix = if channel.is_irc_query() {
                "@"
            } else if channel.is_kick() || channel.is_irc_server_tab() {
                ""
            } else {
                "#"
            };
            let subtitle = if channel.is_irc_query() {
                Some("IRC private chat".to_owned())
            } else if channel.is_kick() {
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

        let owns_keys = ctx.memory(|memory| {
            memory.has_focus(ChannelQuickSwitch::search_id())
                || (memory.focused().is_none()
                    && memory.had_focus_last_frame(ChannelQuickSwitch::search_id()))
        });
        if ctx.is_context_menu_open() || !owns_keys {
            return false;
        }

        let (close, up, down, submit) = ctx.input_mut(|i| {
            (
                consume_exact_key(i, egui::Modifiers::NONE, egui::Key::Escape),
                consume_exact_key(i, egui::Modifiers::NONE, egui::Key::ArrowUp),
                consume_exact_key(i, egui::Modifiers::NONE, egui::Key::ArrowDown),
                consume_exact_key(i, egui::Modifiers::NONE, egui::Key::Enter),
            )
        });

        if close {
            self.close_channel_quick_switch();
            self.focus_chat_composer(ctx);
            return true;
        }

        let candidates = self.quick_switch_candidates();
        if candidates.is_empty() {
            if submit {
                self.close_channel_quick_switch();
                self.focus_chat_composer(ctx);
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
        self.quick_switch.scroll_to_selected |= up || down;
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
        let available = ctx.screen_rect().shrink(12.0);
        let content_width = (available.width() - 20.0).clamp(80.0, 480.0);
        let pointer_moved = ctx.input(|input| input.pointer.delta() != egui::Vec2::ZERO);
        egui::Window::new("Quick Switch")
            .id(egui::Id::new("channel_quick_switch_window"))
            .collapsible(false)
            .resizable(false)
            .min_width(0.0)
            .max_width(content_width)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(content_width);
                ui.add(egui::Label::new(
                    RichText::new("Find and switch channels or whisper threads")
                        .font(t::small())
                        .color(t::text_secondary()),
                ).wrap());

                let input_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.quick_switch.query)
                        .id(ChannelQuickSwitch::search_id())
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
                    self.quick_switch.scroll_to_selected = true;
                }

                ui.add_space(6.0);
                ui.add(egui::Label::new(
                    RichText::new("Ctrl+K opens this palette, ↑/↓ navigates, Enter switches")
                        .font(t::tiny())
                        .color(t::text_muted()),
                ).wrap());
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
                let scroll_to_selected = self.quick_switch.scroll_to_selected;
                let max_height = (available.height() - 160.0).max(40.0);

                egui::ScrollArea::vertical()
                    .id_salt("channel_quick_switch_results")
                    .max_height((visible_rows as f32 * 28.0 + 8.0).min(max_height))
                    .show(ui, |ui| {
                        for (idx, candidate) in candidates.iter().enumerate() {
                            let is_selected = idx == self.quick_switch.selected;
                            let unread_count = candidate.unread_count;
                            let unread_mentions = candidate.unread_mentions;

                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 24.0),
                                egui::Sense::click(),
                            );
                            if is_selected && scroll_to_selected {
                                row_resp.scroll_to_me(Some(egui::Align::Center));
                            }
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
                                egui::Stroke::new(1.0_f32, t::border_subtle()),
                                egui::StrokeKind::Middle,
                            );

                            let mut row_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(row_rect.shrink2(egui::vec2(8.0, 3.0)))
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            );
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
                                    let label_width = ui.available_width();
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(label_width, row_rect.height()),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.add_sized(
                                                [label_width, row_rect.height()],
                                                egui::Label::new(
                                                    RichText::new(&candidate.label)
                                                        .font(t::small())
                                                        .color(if is_selected { t::text_primary() } else { t::text_secondary() })
                                                        .strong(),
                                                ).truncate(),
                                            );
                                        },
                                    );
                                },
                            );
                            if let Some(subtitle) = candidate.subtitle.as_deref() {
                                row_resp.clone().on_hover_text(format!("{}\n{subtitle}", candidate.label));
                            }

                            if row_resp.hovered() && pointer_moved && !scroll_to_selected {
                                self.quick_switch.selected = idx;
                            }
                            if row_resp.clicked() {
                                activate = Some(candidate.entry.clone());
                            }
                        }
                    });
                self.quick_switch.scroll_to_selected = false;
            });

        if !open {
            self.close_channel_quick_switch();
            self.focus_chat_composer(ctx);
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
        self.split_panes.activate_channel(&channel);
        self.state.active_channel = Some(channel.clone());
        self.persist_active_channel(&channel);
        if let Some(ctx) = self.egui_ctx.as_ref() {
            self.focus_chat_composer(ctx);
        }
    }

    fn focus_chat_composer(&self, ctx: &Context) {
        if self.state.active_channel.as_ref().is_some_and(ChannelId::is_virtual) {
            ctx.memory_mut(|memory| {
                if let Some(id) = memory.focused() {
                    memory.surrender_focus(id);
                }
            });
            return;
        }
        let pane = if self.split_panes.panes.len() > 1 {
            self.split_panes.panes.get(self.split_panes.focused).map(|pane| pane.id)
        } else {
            None
        };
        ctx.memory_mut(|memory| memory.request_focus(chat_composer_id(pane)));
    }

    fn sync_split_focus(&mut self, ctx: &Context) {
        if let Some(channel) = self.split_panes.focused_channel().cloned() {
            self.state.active_channel = Some(channel);
            self.focus_chat_composer(ctx);
        }
    }

    fn keep_split_pane(&mut self, index: usize, ctx: &Context) {
        if index >= self.split_panes.panes.len() {
            return;
        }
        let pane = self.split_panes.panes.remove(index);
        if let Some(state) = egui::TextEdit::load_state(ctx, chat_composer_id(Some(pane.id))) {
            egui::TextEdit::store_state(ctx, chat_composer_id(None), state);
        }
        self.state.active_channel = Some(pane.channel);
        self.chat_input_buf = pane.input_buf;
        self.split_panes.panes.clear();
        self.split_panes.focused = 0;
        self.focus_chat_composer(ctx);
    }

    fn close_split_pane(&mut self, index: usize, ctx: &Context) {
        if index >= self.split_panes.panes.len() {
            return;
        }
        self.split_panes.remove_pane(index);
        if self.split_panes.panes.len() == 1 {
            self.keep_split_pane(0, ctx);
        } else {
            self.sync_split_focus(ctx);
        }
    }

    fn close_channel_tab(&mut self, channel: ChannelId, ctx: &Context) {
        if self.pending_reply.as_ref().is_some_and(|reply| reply.channel == channel) {
            self.pending_reply = None;
        }
        let mut removed_pane = false;
        for index in (0..self.split_panes.panes.len()).rev() {
            if self.split_panes.panes[index].channel == channel {
                self.split_panes.remove_pane(index);
                removed_pane = true;
            }
        }
        self.send_cmd(AppCommand::LeaveChannel { channel: channel.clone() });
        self.state.leave_channel(&channel);
        self.sorted_chatters.remove(&channel);
        self.message_search.remove(&channel);

        if removed_pane {
            // Redirects can leave multiple drafts for one channel. Remove all
            // of its panes before choosing the surviving layout and focus.
            match self.split_panes.panes.len() {
                0 if self.state.active_channel.is_none() => {
                    ctx.memory_mut(|memory| memory.stop_text_input());
                }
                0 => self.focus_chat_composer(ctx),
                1 => self.keep_split_pane(0, ctx),
                _ => self.sync_split_focus(ctx),
            }
        }
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

        // Default tab bindings also accept Ctrl+PageDown and Alt+Right.
        // A customized binding disables those aliases.
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
                if consume_exact_key(i, egui::Modifiers::CTRL, egui::Key::PageDown)
                    || consume_exact_key(i, egui::Modifiers::ALT, egui::Key::ArrowRight)
                {
                    next = true;
                }
            }
            let mut prev = consume_binding(i, &prev_b);
            if prev_legacy {
                if consume_exact_key(i, egui::Modifiers::CTRL, egui::Key::PageUp)
                    || consume_exact_key(i, egui::Modifiers::ALT, egui::Key::ArrowLeft)
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
                self.sync_split_focus(ctx);
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
                self.sync_split_focus(ctx);
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
                self.sync_split_focus(ctx);
            } else if let Some(target) = self.state.channel_order.first().cloned() {
                self.activate_channel(target);
            }
            return;
        }
        if last {
            if split_mode {
                self.split_panes.focus_last();
                self.sync_split_focus(ctx);
            } else if let Some(target) = self.state.channel_order.last().cloned() {
                self.activate_channel(target);
            }
            return;
        }

        if split_prev {
            if split_mode {
                self.split_panes.focus_prev();
                self.sync_split_focus(ctx);
            }
            return;
        }
        if split_next {
            if split_mode {
                self.split_panes.focus_next();
                self.sync_split_focus(ctx);
            }
            return;
        }
        if split_move_left {
            if split_mode {
                self.split_panes.move_focused(-1);
                self.sync_split_focus(ctx);
            }
            return;
        }
        if split_move_right {
            if split_mode {
                self.split_panes.move_focused(1);
                self.sync_split_focus(ctx);
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

    /// Apply a `LiveFeedAction` produced by the LiveFeed widget.
    fn handle_live_feed_action(&mut self, action: crate::widgets::live_feed::LiveFeedAction) {
        use crate::widgets::live_feed::LiveFeedAction;
        match action {
            LiveFeedAction::OpenChannel(login) => {
                let id = crust_core::ChannelId::new(&login);
                if self.state.channel_order.contains(&id) {
                    self.activate_channel(id);
                } else if self.try_send_cmd(AppCommand::JoinChannel { channel: id.clone() }) {
                    // Navigate to the new tab immediately; the JoinChannel
                    // command's ChannelJoined event arrives later but won't
                    // change `active_channel` since join_channel only sets it
                    // when active_channel is None.
                    self.state.active_channel = Some(id);
                }
            }
            LiveFeedAction::Refresh => {
                self.send_cmd(AppCommand::LiveFeedRefresh);
            }
            LiveFeedAction::OpenStreamlink(login) => {
                self.send_cmd(AppCommand::OpenStreamlink { channel: login });
            }
            LiveFeedAction::OpenInPlayer(login) => {
                self.send_cmd(AppCommand::OpenPlayer { channel: login });
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
        self.update_ui(ctx);
    }
}

impl CrustApp {
    fn update_ui(&mut self, ctx: &Context) {
        let frame_start = std::time::Instant::now();
        let mut phase_start = frame_start;
        let mut finish_phase = || {
            let now = std::time::Instant::now();
            let elapsed_ms = now.duration_since(phase_start).as_secs_f64() * 1000.0;
            phase_start = now;
            elapsed_ms
        };
        self.emote_bytes.begin_frame(ctx);
        let image_begin_ms = finish_phase();

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

        if !self.settings_open {
            self.settings_filter_editor_modal.close();
        }
        if !self.handle_settings_hotkey_capture(ctx) && !self.settings_filter_editor_modal.open {
            let picker_was_open = self.emote_picker.open;
            self.emote_picker.handle_shortcuts(ctx);
            if picker_was_open && !self.emote_picker.open {
                self.focus_chat_composer(ctx);
            }
            let quick_switch_consumed = self.handle_quick_switch_shortcuts(ctx);
            if !quick_switch_consumed {
                self.handle_channel_shortcuts(ctx);
                self.handle_search_shortcuts(ctx);
                self.handle_font_zoom_shortcuts(ctx);
            }
        }

        let moderation_hotkey = self.consume_mod_hotkey_input(ctx);
        let setup_ms = finish_phase();
        let events = self.service_runtime_channels(ctx);
        let events_ms = finish_phase();
        let emote_rows = self.refresh_emote_messages(ctx);
        let emote_backfill_ms = finish_phase();
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

        // Startup status stays visible while chat and settings remain usable.
        if self.loading_screen.is_active() {
            self.loading_screen.show(ctx);
        }
        let appearance_before = self.appearance_snapshot();

        self.perf.emote_count = self.emote_bytes.len();
        self.perf.emote_ram_kb = self.emote_bytes.byte_len() / 1024;
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

        let maintenance_ms = finish_phase();

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
                    self.request_ivr_logs(channel, username);
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
            self.render_mod_tools_window(ctx, moderation_channel, moderation_hotkey);
        }

        show_plugin_windows(ctx, &plugin_ui_snapshot, &mut self.plugin_ui_session);

        self.show_whispers_window(ctx);
        let dialogs_ms = finish_phase();

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
                    .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
            )
            .show(ctx, |ui| {
                let bar_width = ui.available_width();
                let visibility =
                    toolbar_visibility(bar_width, self.irc_beta_enabled, moderation_available);
                let compact_account = visibility.compact_account;
                let (dot_color, conn_label) = if self.pending_auth.is_some() {
                    (t::yellow(), "Signing in...")
                } else {
                    connection_indicator(&self.state.connection, self.state.auth.logged_in)
                };
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
                                        let _ = self.show_channel_layout_controls(ui, responsive.force_top_tabs,
                                            visibility.compact_controls);
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

                                    let layout_controls = ChannelLayoutControls::new(self.channel_layout,
                                        self.sidebar_visible, responsive.force_top_tabs);
                                    if ui
                                        .add_enabled(layout_controls.sidebar_enabled,
                                            egui::Button::new(RichText::new(layout_controls.sidebar_label)
                                                .font(t::topbar_font()))
                                                .selected(layout_controls.sidebar_selected))
                                        .clicked()
                                    {
                                        self.activate_sidebar_control();
                                        ui.close_menu();
                                    }

                                    if ui
                                        .add(egui::Button::new(RichText::new("Use top tabs")
                                            .font(t::topbar_font())).selected(layout_controls.tabs_selected))
                                        .clicked()
                                    {
                                        self.set_channel_layout(ChannelLayout::TopTabs);
                                        ui.close_menu();
                                    }

                                    ui.separator();

                                    if visibility.show_perf_in_overflow
                                        && ui
                                            .button(
                                                RichText::new("Performance")
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

                            chrome::toolbar_group_frame().show(ui, |ui| {
                                let action_count = 1 + usize::from(visibility.show_perf_toggle)
                                    + usize::from(visibility.show_stats_toggle)
                                    + usize::from(visibility.show_whispers_toggle)
                                    + usize::from(visibility.show_irc_toggle);
                                let action_width = estimate_toolbar_group_width(action_count, 4.0,
                                    visibility.compact_controls) - TOOLBAR_GROUP_FRAME_W
                                    + if visibility.show_mod_button { TOOLBAR_MOD_BUTTON_W + 4.0 } else { 0.0 };
                                ui.set_width(action_width.min(ui.available_width()));
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
                            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
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
                                            egui::Stroke::new(1.0_f32, t::border_accent())
                                        } else {
                                            egui::Stroke::new(1.0_f32, t::border_subtle())
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
                                            egui::Stroke::new(1.0_f32, t::border_accent())
                                        } else {
                                            egui::Stroke::new(1.0_f32, t::border_subtle())
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
                                        let prefix = if ch.is_irc_query() {
                                            "@"
                                        } else if ch.is_kick() || ch.is_irc_server_tab() {
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
                                            egui::Stroke::new(1.0_f32, t::border_accent())
                                        } else {
                                            egui::Stroke::new(1.0_f32, t::border_subtle())
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
                                                    RichText::new(if ch.is_irc_query() { "Switch to private chat" } else { "Switch to channel" })
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
                                                    RichText::new(if ch.is_irc_query() { "Copy nickname" } else { "Copy channel" }).font(t::small()),
                                                )
                                                .clicked()
                                            {
                                                let copy = if ch.is_irc_query() {
                                                    ch.display_name().to_owned()
                                                } else if ch.is_kick() {
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
                            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
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
            self.close_channel_tab(ch, ctx);
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
                            self.split_panes.panes[0].input_buf =
                                std::mem::take(&mut self.chat_input_buf);
                        }
                    }
                }
                self.split_panes.add_pane(ch.clone(), None);
                self.split_panes.focused = self.split_panes.panes.len().saturating_sub(1);
                self.state.active_channel = Some(ch);
            }
            self.focus_chat_composer(ctx);
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
                                .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
                        )
                        .show(ctx, |ui| {
                            self.analytics_panel.show(ui, ch_state);
                        });
                }
            }
        }

        let navigation_ms = finish_phase();

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
                                egui::Stroke::new(1.0_f32, t::border_subtle()),
                            );
                        }

                        // Child controls also select their pane before dispatching actions.
                        if pane_pointer_pressed(ctx, pane_rect, ui.layer_id()) {
                            self.split_panes.focused = pi;
                            self.state.active_channel = Some(ch.clone());
                        }
                        let is_focused = pi == self.split_panes.focused;
                        let bg_resp = ui.interact(
                            pane_rect,
                            egui::Id::new("split_pane_bg").with(self.split_panes.panes[pi].id),
                            egui::Sense::click(),
                        );
                        if bg_resp.clicked() {
                            self.focus_chat_composer(ctx);
                        }

                        let mut pane_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .id_salt(("split_pane", self.split_panes.panes[pi].id))
                                .max_rect(pane_rect)
                                .layout(egui::Layout::top_down(egui::Align::LEFT)),
                        );
                        pane_ui.set_clip_rect(pane_rect);
                        pane_ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

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
                        let split_meta = if ch.is_twitch() && !ch.is_virtual() {
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

                        if ch.is_live_feed() {
                            use crate::widgets::live_feed::LiveFeed;
                            use std::collections::HashSet;
                            let joined: HashSet<String> = self
                                .state
                                .channel_order
                                .iter()
                                .filter(|c| c.is_twitch() && !c.is_virtual())
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
                            if let Some(action) = action {
                                self.handle_live_feed_action(action);
                            }
                            continue;
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

                        // Pane chat input (bottom)
                        let pane_reply = self.reply_for_composer(&ch, Some(pi));
                        let input_h = (t::chat_font_size() + 16.0).max(t::bar_h())
                            + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom)
                                as f32
                            + pane_inner_pad
                            + crate::widgets::chat_input::chat_input_counter_height(
                                ctx, pane_rect.width() - 2.0 * pane_inner_pad,
                                &ch, &self.split_panes.panes[pi].input_buf,
                            )
                            + if pane_reply.is_some() { 36.0 } else { 0.0 };
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
                            egui::Stroke::new(1.0_f32, t::border_subtle()),
                        );
                        {
                        let mut inp_ui = pane_ui.new_child(
                            egui::UiBuilder::new()
                                .id_salt("chat_input_tray")
                                .max_rect(input_rect)
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        inp_ui.set_clip_rect(input_rect);
                            self.ensure_input_emote_catalog(&ch);
                            let chatters_sorted: &[String] = self
                                .sorted_chatters
                                .get(&ch)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]);
                            let chat = ChatInput {
                                id: Some(chat_composer_id(Some(self.split_panes.panes[pi].id))),
                                channel: &ch,
                                logged_in: self.state.auth.logged_in,
                                username: self
                                    .state
                                    .auth
                                    .username
                                    .as_deref(),
                                emote_catalog: self.input_emote_catalogs.get(&ch).map(Vec::as_slice).unwrap_or(&[]),
                                emote_bytes: &self.emote_bytes,
                                pending_reply: pane_reply.as_ref(),
                                message_history: &self.message_history,
                                slash_usage_counts: &self.slash_usage_counts,
                                known_channels: &self.state.channel_order,
                                chatters: chatters_sorted,
                                prevent_overlong_twitch_messages: self
                                    .prevent_overlong_twitch_messages,
                                animate_emotes: animations_allowed,
                                accept_dropped_files: ctx.pointer_latest_pos().map_or(is_focused, |pos| {
                                    pane_rect.contains(pos)
                                        && ctx.layer_id_at(pos) == Some(pane_ui.layer_id())
                                }),
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
                            if inp.dismiss_reply && pane_reply.is_some() {
                                self.pending_reply = None;
                            }
                            if let Some(text) = inp.send {
                                self.submit_chat_input(&ch, text, pane_reply, Some(pi));
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
                                    search.local_history_exhausted = ch_state.local_history_exhausted;
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
                                    .id_salt("messages")
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
                            let scroll_to = if is_focused {
                                self.pending_scroll_to_message.remove(&ch)
                            } else {
                                None
                            };
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
                            .with_pane(self.split_panes.panes[pi].id, is_focused)
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
                        self.keep_split_pane(keep_idx, ctx);
                    } else if let Some(idx) = close_pane {
                        self.close_split_pane(idx, ctx);
                    }

                    // Emote picker -> focused pane
                    let picker_channel = self.split_panes.focused_channel().cloned()
                        .or_else(|| self.state.active_channel.clone());
                    if let Some(channel) = picker_channel.as_ref() {
                        self.prepare_emote_picker(channel);
                    }
                    let picker_was_open = self.emote_picker.open;
                    if let Some(code) = self.emote_picker.show(
                        ctx,
                        picker_channel.as_ref().and_then(|channel| self.input_emote_catalogs.get(channel))
                            .map(Vec::as_slice).unwrap_or(&[]),
                        &self.emote_bytes,
                        &self.cmd_tx,
                        animations_allowed,
                    ) {
                        if let Some(pane) = self
                            .split_panes
                            .panes
                            .get_mut(self.split_panes.focused)
                        {
                            insert_picker_emote(
                                ctx, chat_composer_id(Some(pane.id)),
                                &mut pane.input_buf, &code,
                            );
                        } else {
                            insert_picker_emote(ctx, chat_composer_id(None), &mut self.chat_input_buf, &code);
                        }
                        self.focus_chat_composer(ctx);
                    } else if picker_was_open && !self.emote_picker.open {
                        self.focus_chat_composer(ctx);
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
                    let input_panel_h = input_panel_h
                        + crate::widgets::chat_input::chat_input_counter_height(
                            ctx, ui.available_width(), &active_ch, &self.chat_input_buf,
                        );
                    TopBottomPanel::bottom("chat_input_panel")
                        .resizable(false)
                        .exact_height(input_panel_h)
                        .frame(
                            Frame::new()
                                .fill(t::bg_surface())
                                .inner_margin(Margin::ZERO)
                                .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
                        )
                        .show_inside(ui, |ui| {
                            // Collect sorted chatters for @username autocomplete
                            self.ensure_input_emote_catalog(&active_ch);
                            let chatters_sorted: &[String] = self
                                .sorted_chatters
                                .get(&active_ch)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]);
                            let chat = ChatInput {
                                id: Some(chat_composer_id(None)),
                                channel: &active_ch,
                                logged_in: self.state.auth.logged_in,
                                username: self.state.auth.username.as_deref(),
                                emote_catalog: self.input_emote_catalogs.get(&active_ch).map(Vec::as_slice).unwrap_or(&[]),
                                emote_bytes: &self.emote_bytes,
                                pending_reply: active_reply.as_ref(),
                                message_history: &self.message_history,
                                slash_usage_counts: &self.slash_usage_counts,
                                known_channels: &self.state.channel_order,
                                chatters: chatters_sorted,
                                prevent_overlong_twitch_messages: self
                                    .prevent_overlong_twitch_messages,
                                animate_emotes: animations_allowed,
                                accept_dropped_files: ctx.pointer_latest_pos().map_or(true, |pos| {
                                    ctx.layer_id_at(pos) == Some(ui.layer_id())
                                }),
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
                                self.submit_chat_input(&active_ch, text, active_reply, None);
                            }
                            if result.toggle_emote_picker {
                                self.emote_picker.toggle();
                            }
                        });

                    // Emote picker floating window
                    self.prepare_emote_picker(&active_ch);
                    let picker_was_open = self.emote_picker.open;
                    if let Some(code) = self.emote_picker.show(
                        ctx,
                        self.input_emote_catalogs.get(&active_ch).map(Vec::as_slice).unwrap_or(&[]),
                        &self.emote_bytes,
                        &self.cmd_tx,
                        animations_allowed,
                    ) {
                        insert_picker_emote(ctx, chat_composer_id(None), &mut self.chat_input_buf, &code);
                        self.focus_chat_composer(ctx);
                    } else if picker_was_open && !self.emote_picker.open {
                        self.focus_chat_composer(ctx);
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
                                search.local_history_exhausted = state.local_history_exhausted;
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

        let chat_ms = finish_phase();
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
                        egui::Stroke::new(2.0_f32, t::alpha(ac, border_alpha)),
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

        let can_restore_unsent = self.unsent_messages.front()
            .is_some_and(|message| self.unsent_input_pane(message).is_some());
        if let Some(message) = self.unsent_messages.show(ctx, can_restore_unsent) {
            if !self.restore_unsent_message(&message) {
                self.unsent_messages.push(message);
            }
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
                            1.6_f32,
                            rainbow_color(
                                (time * 0.16 + age * 0.35).fract(),
                                (200.0 * opacity) as u8,
                            ),
                        )
                    } else {
                        egui::Stroke::new(1.5_f32, t::alpha(toast.hue, (160.0 * opacity) as u8))
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

        self.perf.set_chat_stats(frame_chat_stats.clone());
        self.perf.show(ctx);
        let overlays_ms = finish_phase();
        self.emote_bytes.end_frame(ctx, |command| self.cmd_tx.try_send_background(command));
        let image_finish_ms = finish_phase();

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

        let finalization_ms = finish_phase();
        let frame_ms = frame_start.elapsed().as_secs_f64() * 1000.0;
        self.perf.record_update(frame_ms as f32);
        let image_work = self.emote_bytes.work_stats(ctx);
        // Include worker completions while the UI was idle between updates.
        let image_work_before = std::mem::replace(&mut self.previous_image_work, image_work);
        static SLOW_THRESHOLD_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let slow_threshold_ms = *SLOW_THRESHOLD_MS.get_or_init(|| {
            std::env::var("CRUST_SLOW_FRAME_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(100)
        });
        // These phases measure elapsed time inside update_ui, excluding GPU
        // presentation and event-loop waits. Enable crust_ui::timing=debug to
        // compare ordinary updates with warnings from the same workload.
        macro_rules! record_update {
            ($level:ident) => {
                tracing::$level!(target: "crust_ui::timing",
                    frame_ms, image_begin_ms, setup_ms, events_ms,
                    emote_backfill_ms, maintenance_ms, dialogs_ms, navigation_ms,
                    chat_ms, overlays_ms, image_finish_ms, finalization_ms,
                    events, event_limit_reached = events >= MAX_EVENTS_PER_FRAME,
                    events_pending = self.event_rx.len(), emote_rows,
                    emote_jobs_pending = self.pending_emote_refresh.len(),
                    retained_rows = frame_chat_stats.retained_rows,
                    eligible_rows = frame_chat_stats.eligible_rows,
                    active_rows = frame_chat_stats.active_rows,
                    rendered_rows = frame_chat_stats.rendered_rows,
                    hidden_rows = frame_chat_stats.boundary_hidden_rows,
                    prefix_rebuilds = frame_chat_stats.prefix_rebuilds,
                    height_cache_misses = frame_chat_stats.height_cache_misses,
                    animation_bytes = image_work.animation_bytes,
                    pending_decodes = image_work.pending_decodes,
                    completed_decodes = image_work.completed_decodes.saturating_sub(image_work_before.completed_decodes),
                    background_decode_us = image_work.decode_micros.saturating_sub(image_work_before.decode_micros),
                    texture_uploads = image_work.texture_uploads.saturating_sub(image_work_before.texture_uploads),
                    texture_upload_us = image_work.texture_upload_micros.saturating_sub(image_work_before.texture_upload_micros),
                    channels = self.state.channels.len(),
                    "UI update"
                );
            };
        }
        if frame_ms >= slow_threshold_ms as f64 {
            record_update!(warn);
        } else {
            record_update!(debug);
        }
    }
}

// Helper functions

fn channel_tab_badge(ui: &mut egui::Ui, label: String, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .stroke(egui::Stroke::new(1.0_f32, fg.gamma_multiply(0.35)))
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

fn fallback_font_data(path: &str, bytes: Vec<u8>) -> egui::FontData {
    let mut data = egui::FontData::from_owned(bytes);
    if path.ends_with("YuGothR.ttc") || path.ends_with("YuGothM.ttc") {
        // Yu Gothic includes a large line gap. egui centers that gap when the
        // font is used as a fallback, lifting CJK glyphs into the previous row.
        // Restore their baseline relative to the default body font.
        data.tweak.baseline_offset_factor = 0.26;
    }
    data
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
                .insert(key.clone(), fallback_font_data(path, bytes).into());
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

#[cfg(all(test, windows))]
mod font_tests {
    #[test]
    fn yu_gothic_fallback_fits_message_lines_at_multiple_sizes_and_densities() {
        let font = ["C:\\Windows\\Fonts\\YuGothR.ttc", "C:\\Windows\\Fonts\\YuGothM.ttc"]
            .into_iter().find_map(|path| std::fs::read(path).ok().map(|bytes| (path, bytes)));
        // The Japanese language font is optional on Windows installations.
        let Some((path, bytes)) = font else { return };
        let mut definitions = egui::FontDefinitions::default();
        definitions.font_data.insert("japanese".to_owned(), super::fallback_font_data(path, bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            definitions.families.entry(family).or_default().push("japanese".to_owned());
        }
        for density in [1.0, 1.25, 1.875] {
            for size in [9.0, 13.5, 20.0, 32.0] {
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    let ctx = egui::Context::default();
                    ctx.set_fonts(definitions.clone());
                    ctx.set_pixels_per_point(density);
                    let _ = ctx.run(Default::default(), |ctx| {
                        let galley = ctx.fonts(|fonts| fonts.layout_job(egui::text::LayoutJob::simple(
                            "Latin text\nLatin 日本語\nLatin text".to_owned(),
                            egui::FontId::new(size, family.clone()), egui::Color32::WHITE, 1000.0,
                        )));
                        for rows in galley.rows.windows(2) {
                            assert!(rows[0].visuals.mesh_bounds.max.y <= rows[1].visuals.mesh_bounds.min.y + 1.0 / density,
                                "overlapping lines at {size}pt/{density}dpi/{family:?}: {:?} and {:?}",
                                rows[0].visuals.mesh_bounds, rows[1].visuals.mesh_bounds);
                        }
                        let row = &galley.rows[1];
                        assert!(row.visuals.mesh_bounds.min.y >= row.rect.min.y - 1.0 / density,
                            "fallback extends into the preceding line at {size}pt/{density}dpi/{family:?}");
                    });
                }
            }
        }
    }
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

fn build_emote_catalog_index(catalog: &[EmoteCatalogEntry]) -> HashMap<String, Vec<usize>> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (position, entry) in catalog.iter().enumerate() {
        index.entry(entry.code.clone()).or_default().push(position);
    }
    for candidates in index.values_mut() {
        candidates.sort_by_key(|&position| std::cmp::Reverse(catalog[position].resolution_priority()));
    }
    index
}

fn resolve_catalog_emote<'a>(
    catalog: &'a [EmoteCatalogEntry],
    index: &HashMap<String, Vec<usize>>,
    channel: &ChannelId,
    sender_user_id: &str,
    code: &str,
) -> Option<&'a EmoteCatalogEntry> {
    index.get(code)?.iter().map(|&position| &catalog[position])
        .find(|entry| entry.is_available_for(channel, sender_user_id))
}

fn catalog_emote_data(entry: &EmoteCatalogEntry) -> (String, String, String, String, Option<String>) {
    (
        if entry.id.is_empty() { entry.code.clone() } else { entry.id.clone() },
        entry.code.clone(), entry.url.clone(), entry.provider.clone(), entry.url_hd.clone(),
    )
}

fn refresh_message_emotes(
    messages: &mut VecDeque<ChatMessage>,
    through: MessageId,
    limit: usize,
    catalog: &[EmoteCatalogEntry],
    index: &HashMap<String, Vec<usize>>,
) -> (usize, Option<MessageId>, Vec<MessageId>) {
    let Some(end) = messages.iter().rposition(|message| message.id == through).map(|position| position + 1) else {
        return (0, None, Vec::new());
    };
    let start = end.saturating_sub(limit);
    let next = start.checked_sub(1).map(|position| messages[position].id);
    let mut changed = Vec::new();
    for message in messages.range_mut(start..end) {
        if !matches!(message.msg_kind, MsgKind::Chat | MsgKind::Bits { .. } | MsgKind::SuspiciousUserMessage) {
            continue;
        }
        let spans = crust_core::format::tokenize(&message.raw_text, message.flags.is_action,
            &message.twitch_emotes, &|code| resolve_catalog_emote(catalog, index,
                &message.channel, &message.sender.user_id.0, code).map(catalog_emote_data));
        if message.spans != spans {
            message.spans = spans;
            changed.push(message.id);
        }
    }
    (end - start, next, changed)
}

fn tokenize_whisper_text(
    text: &str,
    twitch_emotes: &[TwitchEmotePos],
    catalog: &[EmoteCatalogEntry],
    index: &HashMap<String, Vec<usize>>,
    sender_user_id: &str,
) -> Vec<Span> {
    let channel = ChannelId::mentions();
    crust_core::format::tokenize(text, false, twitch_emotes, &|code| {
        resolve_catalog_emote(catalog, index, &channel, sender_user_id, code).map(catalog_emote_data)
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
        whisper_thread_matches_query, EventToast, SplitPanes, TabVisualStyle,
    };
    use crate::theme as t;
    use crust_core::events::AppCommand;
    use crust_core::model::ChannelId;

    fn test_app() -> (super::CrustApp, tokio::sync::mpsc::Receiver<AppCommand>) {
        let (commands, receiver) = tokio::sync::mpsc::channel(64);
        let (_, events) = tokio::sync::mpsc::channel(64);
        let mut app = super::CrustApp::with_event_channels(commands, events);
        // Other tests may be changing the process-wide theme while this fixture is created.
        app.chat_font_size = t::DEFAULT_CHAT_FONT_SIZE;
        app.ui_font_size = t::DEFAULT_UI_FONT_SIZE;
        (app, receiver)
    }

    #[test]
    fn image_work_completed_between_ui_updates_is_reported_once() {
        #[derive(Default)]
        struct WorkRecord {
            completed_decodes: u64,
            decode_micros: u64,
        }
        impl tracing::field::Visit for WorkRecord {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                match field.name() {
                    "completed_decodes" => self.completed_decodes = value,
                    "background_decode_us" => self.decode_micros = value,
                    _ => {}
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        struct Capture(Option<std::sync::Arc<std::sync::Mutex<Vec<WorkRecord>>>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
                self.0.is_some() && metadata.target() == "crust_ui::timing"
            }
            fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
                Some(if self.0.is_some() {
                    tracing::metadata::LevelFilter::TRACE
                } else {
                    tracing::metadata::LevelFilter::OFF
                })
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let Some(records) = &self.0 else { return };
                let mut record = WorkRecord::default();
                event.record(&mut record);
                records.lock().unwrap().push(record);
            }
        }

        let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::<WorkRecord>::new()));
        // Register both interests before enabling tracing, so another test thread cannot
        // cache a new callsite using only its empty thread-local subscriber.
        let _disabled = tracing::Dispatch::new(Capture(None));
        tracing::subscriber::with_default(Capture(Some(records.clone())), || {
            fn capture_probe() {
                tracing::debug!(target: "crust_ui::timing", "capture probe");
            }
            // A parallel test can register a callsite before the capturing thread uses it.
            std::thread::spawn(capture_probe).join().unwrap();
            assert!(records.lock().unwrap().is_empty(), "foreign-thread events must not be captured");
            capture_probe();
            assert_eq!(records.lock().unwrap().len(), 1,
                "a callsite first visited without a subscriber must still be captured");
            records.lock().unwrap().clear();

            let (mut app, _commands) = test_app();
            let ctx = egui::Context::default();
            production_ui_frame(&ctx, &mut app, Vec::new());
            assert!(!records.lock().unwrap().is_empty(), "the UI timing event must be captured");
            records.lock().unwrap().clear();

            let mut raw = Vec::new();
            {
                let mut encoder = image::codecs::gif::GifEncoder::new(&mut raw);
                for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
                    encoder.encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(64, 64, image::Rgba(color)),
                        0, 0, image::Delay::from_numer_denom_ms(100, 1),
                    )).unwrap();
                }
            }
            ctx.include_bytes("bytes://between-updates.gif", raw);
            assert!(matches!(ctx.try_load_image(
                "bytes://between-updates.gif#0", egui::load::SizeHint::default(),
            ), Ok(egui::load::ImagePoll::Pending { .. })));

            // Let the real decoder finish while no UI update is running.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let completed = loop {
                let work = app.emote_bytes.work_stats(&ctx);
                if work.completed_decodes == 1 && work.pending_decodes == 0 && work.decode_micros > 0 {
                    break work;
                }
                assert!(std::time::Instant::now() < deadline, "background decode did not finish");
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            assert!(completed.decode_micros > 0);

            production_ui_frame(&ctx, &mut app, Vec::new());
            {
                let records = records.lock().unwrap();
                assert_eq!(records.iter().map(|record| record.completed_decodes).sum::<u64>(), 1);
                assert_eq!(records.iter().map(|record| record.decode_micros).sum::<u64>(),
                    completed.decode_micros);
            }

            records.lock().unwrap().clear();
            production_ui_frame(&ctx, &mut app, Vec::new());
            let records = records.lock().unwrap();
            assert!(!records.is_empty());
            assert!(records.iter().all(|record| record.completed_decodes == 0
                && record.decode_micros == 0), "completed work must not be counted twice");
        });
    }

    fn scoped_emote(url: &str, channel: Option<ChannelId>, user: Option<&str>) -> crust_core::EmoteCatalogEntry {
        crust_core::EmoteCatalogEntry {
            id: url.to_owned(),
            code: "SharedCode".to_owned(),
            provider: "7tv".to_owned(),
            url: url.to_owned(),
            url_hd: Some(format!("{url}/hd")),
            scope: if user.is_some() { "personal" } else if channel.is_some() { "channel" } else { "global" }.to_owned(),
            channel,
            user_id: user.map(|user| crust_core::UserId(user.to_owned())),
        }
    }

    fn emote_url(message: &crust_core::ChatMessage) -> Option<&str> {
        message.spans.iter().find_map(|span| match span {
            crust_core::Span::Emote { url, .. } => Some(url.as_str()),
            _ => None,
        })
    }

    #[test]
    fn emote_catalogs_respect_channels_and_the_message_sender() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        for channel in [&first, &second] {
            app.state.join_channel(channel.clone());
            for (id, user) in [(1, "owner"), (2, "other")] {
                let mut message = search_message(channel.clone());
                message.id = crust_core::MessageId(id);
                message.sender.user_id = crust_core::UserId(user.to_owned());
                message.raw_text = "SharedCode".to_owned();
                app.state.channels.get_mut(channel).unwrap().messages.push_back(message);
            }
        }
        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated { emotes: vec![
            scoped_emote("global", None, None),
            scoped_emote("channel", Some(first.clone()), None),
            scoped_emote("personal", None, Some("owner")),
        ] }, &ctx);
        app.refresh_emote_messages(&ctx);
        let messages = &app.state.channels[&first].messages;
        assert_eq!(emote_url(&messages[0]), Some("personal"));
        assert_eq!(emote_url(&messages[1]), Some("channel"));
        let messages = &app.state.channels[&second].messages;
        assert_eq!(emote_url(&messages[0]), Some("personal"));
        assert_eq!(emote_url(&messages[1]), Some("global"));

        app.apply_event(crust_core::AppEvent::Authenticated {
            username: "owner".to_owned(), user_id: "owner".to_owned(),
        }, &ctx);
        app.ensure_input_emote_catalog(&first);
        assert_eq!(app.input_emote_catalogs[&first][0].url, "personal");
        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        app.ensure_input_emote_catalog(&first);
        app.ensure_input_emote_catalog(&second);
        assert_eq!(app.input_emote_catalogs[&first][0].url, "channel");
        assert_eq!(app.input_emote_catalogs[&second][0].url, "global");
    }

    #[test]
    fn follower_catalog_entries_require_the_account_and_channel_in_picker_and_chat() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("followed");
        let other = ChannelId::new("other");
        app.apply_event(crust_core::AppEvent::Authenticated {
            username: "owner".into(), user_id: "owner".into(),
        }, &ctx);
        let mut follower = scoped_emote("follower", Some(channel.clone()), Some("owner"));
        follower.provider = "twitch".into();
        let mut account = scoped_emote("subscription", None, Some("owner"));
        account.provider = "twitch".into();
        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
            emotes: vec![follower, account],
        }, &ctx);
        app.ensure_input_emote_catalog(&channel);
        app.ensure_input_emote_catalog(&other);
        assert_eq!(app.input_emote_catalogs[&channel][0].url, "follower");
        assert_eq!(app.input_emote_catalogs[&other][0].url, "subscription");
        let resolve = |channel: &ChannelId, owner: &str| super::resolve_catalog_emote(
            &app.emote_catalog, &app.emote_catalog_index, channel, owner, "SharedCode",
        ).map(|entry| entry.url.as_str());
        assert_eq!(resolve(&channel, "owner"), Some("follower"));
        assert_eq!(resolve(&other, "owner"), Some("subscription"));
        assert_eq!(resolve(&channel, "someone_else"), None);
        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        app.ensure_input_emote_catalog(&channel);
        assert!(app.input_emote_catalogs[&channel].is_empty());
    }

    #[test]
    fn same_count_catalog_updates_replace_metadata_and_remove_old_emotes() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        let mut message = search_message(channel.clone());
        message.raw_text = "SharedCode".to_owned();
        app.state.mentions.push_back(message.clone());
        app.state.channels.get_mut(&channel).unwrap().messages.push_back(message);
        for url in ["first", "replacement"] {
            app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
                emotes: vec![scoped_emote(url, Some(channel.clone()), None)],
            }, &ctx);
            app.refresh_emote_messages(&ctx);
            app.ensure_input_emote_catalog(&channel);
            let message = &app.state.channels[&channel].messages[0];
            assert_eq!(emote_url(message), Some(url));
            assert_eq!(emote_url(&app.state.mentions[0]), Some(url));
            assert_eq!(app.input_emote_catalogs[&channel][0].url, url);
            assert!(matches!(&message.spans[0], crust_core::Span::Emote { id, url_hd, .. }
                if id == url && url_hd.as_deref() == Some(format!("{url}/hd").as_str())));
        }
        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated { emotes: Vec::new() }, &ctx);
        app.refresh_emote_messages(&ctx);
        assert_eq!(emote_url(&app.state.channels[&channel].messages[0]), None);
        assert_eq!(emote_url(&app.state.mentions[0]), None);
        app.ensure_input_emote_catalog(&channel);
        assert!(app.input_emote_catalogs[&channel].is_empty());
    }

    fn fill_emote_refresh_history(app: &mut super::CrustApp, channel: &ChannelId, count: u64) {
        app.state.join_channel(channel.clone());
        for id in 0..count {
            let mut message = search_message(channel.clone());
            message.id = crust_core::MessageId(id);
            if id % 10 == 0 {
                message.msg_kind = crust_core::model::MsgKind::SuspiciousUserMessage;
            }
            message.raw_text = "SharedCode".to_owned();
            app.state.channels.get_mut(channel).unwrap().messages.push_back(message);
        }
    }

    #[test]
    fn emote_refresh_covers_older_rows_in_bounded_passes_and_replaces_queued_catalogs() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("channel");
        fill_emote_refresh_history(&mut app, &channel, 900);
        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
            emotes: vec![scoped_emote("first", None, None)],
        }, &ctx);
        app.refresh_emote_messages(&ctx);
        assert_eq!(app.state.channels[&channel].messages.iter().filter(|message| emote_url(message).is_some()).count(),
            super::EMOTE_REFRESH_ROWS_PER_FRAME);
        assert_eq!(emote_url(&app.state.channels[&channel].messages[0]), None);

        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
            emotes: vec![scoped_emote("replacement", None, None)],
        }, &ctx);
        for _ in 0..5 {
            app.refresh_emote_messages(&ctx);
        }
        assert!(app.pending_emote_refresh.is_empty());
        assert!(app.state.channels[&channel].messages.iter().all(|message| emote_url(message) == Some("replacement")));
    }

    #[test]
    fn repeated_catalog_snapshots_do_not_starve_older_message_rows() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("channel");
        fill_emote_refresh_history(&mut app, &channel, 900);
        for revision in 0..9 {
            app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
                emotes: vec![scoped_emote(&format!("revision-{revision}"), None, None)],
            }, &ctx);
            app.refresh_emote_messages(&ctx);
            assert!(app.pending_emote_refresh.len() <= 1);
            if revision == 2 {
                assert_eq!(emote_url(&app.state.channels[&channel].messages[0]), Some("revision-2"));
            }
        }
        for _ in 0..6 {
            app.refresh_emote_messages(&ctx);
        }
        assert!(app.pending_emote_refresh.is_empty());
        assert!(app.state.channels[&channel].messages.iter()
            .all(|message| emote_url(message) == Some("revision-8")));
    }

    #[test]
    fn history_loaded_after_the_catalog_refreshes_saved_emotes_in_bounded_passes() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated { emotes: Vec::new() }, &ctx);
        assert!(app.pending_emote_refresh.is_empty());
        let stale_emote = scoped_emote("removed", None, None);
        let messages: Vec<_> = (0..900).map(|id| {
            let mut message = search_message(channel.clone());
            message.id = crust_core::MessageId(id);
            message.server_id = Some(format!("saved-{id}"));
            message.raw_text = "SharedCode".to_owned();
            message.flags.is_history = true;
            message.spans = crust_core::format::tokenize(&message.raw_text, false, &[],
                &|_| Some(super::catalog_emote_data(&stale_emote)));
            message
        }).collect();
        app.apply_event(crust_core::AppEvent::HistoryLoaded {
            channel: channel.clone(), messages: messages.clone(), local: None,
        }, &ctx);
        app.apply_event(crust_core::AppEvent::MentionsLoaded { messages }, &ctx);
        app.refresh_emote_messages(&ctx);
        assert_eq!(app.state.channels[&channel].messages.iter()
            .chain(app.state.mentions.iter()).filter(|message| emote_url(message).is_none()).count(),
            super::EMOTE_REFRESH_ROWS_PER_FRAME);
        for _ in 0..5 {
            app.refresh_emote_messages(&ctx);
        }
        assert!(app.pending_emote_refresh.is_empty());
        assert!(app.state.channels[&channel].messages.iter().chain(app.state.mentions.iter())
            .all(|message| emote_url(message).is_none()));
    }

    #[test]
    fn emote_refresh_discards_work_when_its_remaining_rows_are_evicted() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("channel");
        fill_emote_refresh_history(&mut app, &channel, 900);
        app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
            emotes: vec![scoped_emote("current", None, None)],
        }, &ctx);
        app.refresh_emote_messages(&ctx);
        app.state.channels.get_mut(&channel).unwrap().messages.drain(..550);
        app.refresh_emote_messages(&ctx);
        assert!(app.pending_emote_refresh.is_empty());
        assert!(app.state.channels[&channel].messages.iter().all(|message| emote_url(message) == Some("current")));
    }

    fn saturated_app() -> (super::CrustApp, tokio::sync::mpsc::Receiver<AppCommand>) {
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        let (_, events) = tokio::sync::mpsc::channel(1);
        let app = super::CrustApp::with_event_channels(commands, events);
        for _ in 0..=super::MAX_DEFERRED_COMMANDS {
            assert!(app.try_send_cmd(AppCommand::LiveFeedRefresh));
        }
        (app, receiver)
    }

    fn reply_context() -> crust_core::ReplyInfo {
        crust_core::ReplyInfo {
            parent_msg_id: "parent-message".to_owned(),
            parent_user_login: "viewer".to_owned(),
            parent_display_name: "Viewer".to_owned(),
            parent_msg_body: "original message".to_owned(),
        }
    }

    #[test]
    fn saturated_runtime_and_event_channels_do_not_block_rendering_or_reorder_actions() {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        commands.try_send(AppCommand::Logout).unwrap();
        let (events, event_receiver) = tokio::sync::mpsc::channel(1);
        events.try_send(crust_core::AppEvent::Error {
            context: "QueueTest".to_owned(),
            message: "event while both channels are full".to_owned(),
        }).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut app = super::CrustApp::with_event_channels(commands, event_receiver);
            let channel = ChannelId::new("queued");
            app.state.join_channel(channel.clone());
            app.send_cmd(AppCommand::JoinChannel { channel: channel.clone() });
            let ctx = egui::Context::default();
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                app.service_runtime_channels(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("The UI remains responsive.");
                });
            });
            assert!(!output.shapes.is_empty());
            app.send_cmd(AppCommand::LeaveChannel { channel });
            done.send(()).unwrap();
            for _ in 0..3 {
                if resumed.recv_timeout(std::time::Duration::from_secs(2)).is_err() {
                    return;
                }
                app.service_runtime_channels(&ctx);
                done.send(()).unwrap();
            }
            assert!(app.cmd_tx.deferred.borrow().commands.is_empty());
        });

        // A blocking send cannot reach this notification until a receiver
        // drains a channel. Neither receiver is drained before this assertion.
        finished.recv_timeout(std::time::Duration::from_secs(2))
            .expect("rendering blocked on a full runtime queue");
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::Logout));
        for expected in 0..3 {
            resume.send(()).unwrap();
            finished.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            let command = receiver.try_recv().unwrap();
            match expected {
                0 => assert!(matches!(command, AppCommand::JoinChannel { .. })),
                1 => assert!(matches!(command, AppCommand::InjectLocalMessage { .. })),
                _ => assert!(matches!(command, AppCommand::LeaveChannel { .. })),
            }
        }
        worker.join().unwrap();
    }

    #[test]
    fn widget_actions_cannot_overtake_a_deferred_account_change() {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut sender = super::UiCommandSender::new(commands);
        sender.try_send(AppCommand::LiveFeedRefresh).unwrap();
        sender.try_send(AppCommand::Logout).unwrap();
        receiver.try_recv().unwrap();

        // This is the same sender API used by message menus and widgets.
        sender.try_send(AppCommand::SendMessage {
            channel: ChannelId::new("channel"),
            text: "later action".to_owned(),
            reply_to_msg_id: None,
            reply: None,
        }).unwrap();
        assert!(receiver.try_recv().is_err());
        let ctx = egui::Context::default();
        sender.flush(&ctx);
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::Logout));
        sender.flush(&ctx);
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::SendMessage { .. }));
    }

    #[test]
    fn background_requests_wait_for_user_actions_without_failure_toasts() {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut sender = super::UiCommandSender::new(commands);
        let image = || AppCommand::FetchImage { url: "https://example.test/image.png".to_owned() };
        sender.try_send(AppCommand::LiveFeedRefresh).unwrap();
        assert!(!sender.try_send_background(image()));
        sender.try_send(AppCommand::Logout).unwrap();
        receiver.try_recv().unwrap();
        assert!(!sender.try_send_background(image()));
        assert!(sender.deferred.borrow().error.is_none());
        sender.flush(&egui::Context::default());
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::Logout));
        assert!(sender.try_send_background(image()));
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::FetchImage { .. }));
    }

    #[test]
    fn rejected_ivr_log_requests_end_loading_and_allow_retry() {
        let (mut app, mut receiver) = saturated_app();
        app.user_profile_popup.set_loading("alice", vec![], Some(ChannelId::new("room")), false);
        assert!(!app.request_ivr_logs("room".into(), "alice".into()));
        assert!(!app.user_profile_popup.abort_ivr_logs_request("already rejected"));

        while receiver.try_recv().is_ok() {}
        while !app.cmd_tx.deferred.borrow().commands.is_empty() {
            app.cmd_tx.flush(&egui::Context::default());
            while receiver.try_recv().is_ok() {}
        }
        assert!(app.request_ivr_logs("room".into(), "alice".into()));
        let AppCommand::FetchIvrLogs { channel, username, request_id } = receiver.try_recv().unwrap()
            else { panic!("retry did not queue the log request") };
        assert!(app.user_profile_popup.apply_ivr_logs_result(&channel, &username, request_id, Ok(vec![])));

        drop(receiver);
        assert!(!app.request_ivr_logs("room".into(), "alice".into()));
        assert!(!app.user_profile_popup.abort_ivr_logs_request("already rejected"));
    }

    #[test]
    fn losing_the_runtime_ends_pending_ivr_log_requests() {
        let (mut app, mut receiver) = test_app();
        app.user_profile_popup.set_loading("alice", vec![], Some(ChannelId::new("room")), false);
        assert!(app.request_ivr_logs("room".into(), "alice".into()));
        let AppCommand::FetchIvrLogs { channel, username, request_id } = receiver.try_recv().unwrap()
            else { panic!("log request was not queued") };
        drop(receiver);
        app.service_runtime_channels(&egui::Context::default());
        assert!(!app.user_profile_popup.abort_ivr_logs_request("already stopped"));
        assert!(!app.user_profile_popup.apply_ivr_logs_result(&channel, &username, request_id, Ok(vec![])));
    }

    #[test]
    fn command_overflow_preserves_the_draft_reply_and_history() {
        let (mut app, _receiver) = saturated_app();
        let channel = ChannelId::new("channel");
        let reply = reply_context();
        app.state.auth.logged_in = true;
        app.pending_reply = Some(super::PendingReply {
            channel: channel.clone(),
            info: reply.clone(),
        });
        let draft = "  /me  keeps this draft 🦀  ";
        assert!(!app.submit_chat_input(&channel, draft.to_owned(), Some(reply), None));
        assert_eq!(app.chat_input_buf, draft);
        assert_eq!(app.pending_reply.as_ref().unwrap().info.parent_msg_id, "parent-message");
        assert!(app.message_history.is_empty());
        assert!(app.slash_usage_counts.is_empty());
        assert_eq!(app.cmd_tx.deferred.borrow().commands.len(), super::MAX_DEFERRED_COMMANDS);
        app.service_runtime_channels(&egui::Context::default());
        assert!(app.event_toasts.iter().any(|toast| toast.text.contains("not queued")));
    }

    #[test]
    fn rejected_split_submission_restores_only_its_own_composer() {
        let (mut app, _receiver) = saturated_app();
        let channel = ChannelId::new("channel");
        app.state.auth.logged_in = true;
        app.split_panes.add_pane(channel.clone(), None);
        app.split_panes.add_pane(channel.clone(), None);
        app.split_panes.panes[0].input_buf = "first split draft".to_owned();
        app.chat_input_buf = "single pane draft".to_owned();
        assert!(!app.submit_chat_input(&channel, "second split draft".to_owned(), None, Some(1)));
        assert_eq!(app.split_panes.panes[0].input_buf, "first split draft");
        assert_eq!(app.split_panes.panes[1].input_buf, "second split draft");
        assert_eq!(app.chat_input_buf, "single pane draft");
    }

    #[test]
    fn deferred_chat_submission_keeps_reply_context_until_delivery() {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        commands.try_send(AppCommand::Logout).unwrap();
        let (_, events) = tokio::sync::mpsc::channel(1);
        let mut app = super::CrustApp::with_event_channels(commands, events);
        let channel = ChannelId::new("channel");
        let reply = reply_context();
        app.state.auth.logged_in = true;
        app.pending_reply = Some(super::PendingReply {
            channel: channel.clone(),
            info: reply.clone(),
        });
        assert!(app.submit_chat_input(&channel, "  queued reply  ".to_owned(), Some(reply), None));
        assert!(app.pending_reply.is_none());
        assert_eq!(app.message_history, ["queued reply"]);
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::Logout));
        app.service_runtime_channels(&egui::Context::default());
        match receiver.try_recv().unwrap() {
            AppCommand::SendMessage { text, reply_to_msg_id, reply, .. } => {
                assert_eq!(text, "queued reply");
                assert_eq!(reply_to_msg_id.as_deref(), Some("parent-message"));
                assert_eq!(reply.unwrap().parent_msg_body, "original message");
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn a_closed_runtime_preserves_the_chat_draft_and_reports_failure() {
        let (mut app, receiver) = test_app();
        drop(receiver);
        app.state.auth.logged_in = true;
        assert!(!app.submit_chat_input(&ChannelId::new("channel"), "unsent".to_owned(), None, None));
        assert_eq!(app.chat_input_buf, "unsent");
        app.service_runtime_channels(&egui::Context::default());
        assert!(app.event_toasts.iter().any(|toast| toast.text.contains("background service stopped")));
    }

    #[test]
    fn deferred_uploads_have_a_byte_limit_and_release_it_on_delivery() {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        commands.try_send(AppCommand::Logout).unwrap();
        let mut queue = super::DeferredCommands::default();
        let upload = |size| AppCommand::UploadImage {
            channel: ChannelId::new("channel"),
            bytes: vec![0; size],
            format: "png".to_owned(),
            source_path: None,
        };
        assert!(queue.send(&commands, upload(super::MAX_DEFERRED_UPLOAD_BYTES)));
        assert!(!queue.send(&commands, upload(1)));
        assert_eq!(queue.commands.len(), 1);
        assert_eq!(queue.error, Some(super::CommandQueueError::Full));
        receiver.try_recv().unwrap();
        queue.flush(&commands);
        assert_eq!(queue.upload_bytes, 0);
        assert!(queue.send(&commands, upload(1)));
        assert_eq!(queue.upload_bytes, 1);
    }

    #[test]
    fn hidden_live_feed_snapshots_do_not_request_thumbnail_images() {
        let (mut app, mut receiver) = test_app();
        let ctx = egui::Context::default();
        let snapshot = crust_core::model::LiveChannelSnapshot {
            user_id: "1".to_owned(),
            user_login: "channel".to_owned(),
            user_name: "Channel".to_owned(),
            viewer_count: 1,
            thumbnail_url: "https://example.test/thumbnail.png".to_owned(),
            started_at: String::new(),
        };
        app.apply_event(crust_core::AppEvent::LiveFeedUpdated { channels: vec![snapshot.clone()] }, &ctx);
        app.apply_event(crust_core::AppEvent::LiveFeedPartialUpdate {
            channels: vec![snapshot], error: "temporarily unavailable".to_owned(),
        }, &ctx);
        assert!(receiver.try_recv().is_err());
        app.emote_bytes.end_frame(&ctx, |_| panic!("a hidden thumbnail was requested"));
    }

    #[test]
    fn runtime_rejection_restores_an_empty_matching_composer_and_reply() {
        let (mut app, _receiver) = test_app();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.apply_event(crust_core::AppEvent::MessageSendRejected {
            channel,
            text: "unsent reply".to_owned(),
            reply_to_msg_id: Some("parent-message".to_owned()),
            reply: Some(reply_context()),
            reason: "The account is still connecting.".to_owned(),
        }, &egui::Context::default());
        assert_eq!(app.chat_input_buf, "unsent reply");
        assert_eq!(app.pending_reply.as_ref().unwrap().info.parent_msg_id, "parent-message");
        assert!(app.unsent_messages.front().is_none());
    }

    #[test]
    fn single_persisted_pane_restores_into_the_visible_composer() {
        let (mut app, _receiver) = test_app();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.split_panes.add_pane(channel.clone(), None);
        app.apply_event(crust_core::AppEvent::MessageSendRejected {
            channel: channel.clone(),
            text: "visible draft".to_owned(),
            reply_to_msg_id: None,
            reply: None,
            reason: "The account changed.".to_owned(),
        }, &egui::Context::default());
        assert_eq!(app.chat_input_buf, "visible draft");
        assert!(app.split_panes.panes[0].input_buf.is_empty());
        app.input_buf_for_channel_mut(&channel).push_str(" with upload");
        assert_eq!(app.chat_input_buf, "visible draft with upload");
    }

    #[test]
    fn rejected_split_reply_keeps_its_target_when_resubmitted() {
        let (mut app, mut receiver) = test_app();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        for channel in [&first, &second] {
            app.state.join_channel(channel.clone());
            app.split_panes.add_pane(channel.clone(), None);
        }
        app.state.auth.logged_in = true;
        app.split_panes.focused = 1;
        app.state.active_channel = Some(second.clone());
        app.apply_event(crust_core::AppEvent::MessageSendRejected {
            channel: second.clone(),
            text: "reply from split".to_owned(),
            reply_to_msg_id: Some("parent-message".to_owned()),
            reply: Some(reply_context()),
            reason: "The account was connecting.".to_owned(),
        }, &egui::Context::default());
        assert!(app.split_panes.panes[0].input_buf.is_empty());
        assert!(app.reply_for_composer(&first, Some(0)).is_none());
        let reply = app.reply_for_composer(&second, Some(1));
        let draft = std::mem::take(&mut app.split_panes.panes[1].input_buf);
        assert!(app.submit_chat_input(&second, draft, reply, Some(1)));
        assert!(app.pending_reply.is_none());
        match receiver.try_recv().unwrap() {
            AppCommand::SendMessage { channel, text, reply_to_msg_id, reply } => {
                assert_eq!(channel, second);
                assert_eq!(text, "reply from split");
                assert_eq!(reply_to_msg_id.as_deref(), Some("parent-message"));
                assert_eq!(reply.unwrap().parent_msg_body, "original message");
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn rejected_reply_cannot_attach_to_a_newer_draft_in_a_duplicate_channel_pane() {
        let (mut app, _receiver) = test_app();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.split_panes.add_pane(channel.clone(), None);
        app.split_panes.add_pane(channel.clone(), None);
        app.split_panes.focused = 0;
        app.split_panes.panes[0].input_buf = "newer draft".to_owned();
        app.apply_event(crust_core::AppEvent::MessageSendRejected {
            channel: channel.clone(),
            text: "older reply".to_owned(),
            reply_to_msg_id: Some("parent-message".to_owned()),
            reply: Some(reply_context()),
            reason: "The account changed.".to_owned(),
        }, &egui::Context::default());
        assert_eq!(app.split_panes.panes[0].input_buf, "newer draft");
        assert!(app.split_panes.panes[1].input_buf.is_empty());
        assert!(app.pending_reply.is_none());
        let rejected = app.unsent_messages.front().unwrap().clone();
        assert!(!app.restore_unsent_message(&rejected));
        app.split_panes.focused = 1;
        assert!(app.restore_unsent_message(&rejected));
        assert_eq!(app.split_panes.panes[0].input_buf, "newer draft");
        assert_eq!(app.split_panes.panes[1].input_buf, "older reply");
        assert!(app.reply_for_composer(&channel, Some(0)).is_none());
        assert_eq!(app.reply_for_composer(&channel, Some(1)).unwrap().parent_msg_id, "parent-message");
    }

    #[test]
    fn runtime_rejection_does_not_restore_under_a_new_keypress() {
        let (mut app, _receiver) = test_app();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        let ctx = egui::Context::default();
        let _ = ctx.run(shortcut_input(egui::Key::Enter, egui::Modifiers::NONE), |ctx| {
            app.apply_event(crust_core::AppEvent::MessageSendRejected {
                channel: channel.clone(),
                text: "unsent".to_owned(),
                reply_to_msg_id: None,
                reply: None,
                reason: "The account changed.".to_owned(),
            }, ctx);
        });
        assert!(app.chat_input_buf.is_empty());
        assert_eq!(app.unsent_messages.front().unwrap().text, "unsent");
    }

    #[test]
    fn runtime_rejection_retains_recovery_without_overwriting_a_newer_draft_or_reply() {
        let (mut app, _receiver) = test_app();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.chat_input_buf = "newer draft".to_owned();
        let mut new_reply = reply_context();
        new_reply.parent_msg_id = "newer-parent".to_owned();
        app.pending_reply = Some(super::PendingReply { channel: channel.clone(), info: new_reply });
        app.apply_event(crust_core::AppEvent::MessageSendRejected {
            channel,
            text: "older rejected message".to_owned(),
            reply_to_msg_id: Some("parent-message".to_owned()),
            reply: Some(reply_context()),
            reason: "The account is still connecting.".to_owned(),
        }, &egui::Context::default());
        assert_eq!(app.chat_input_buf, "newer draft");
        assert_eq!(app.pending_reply.as_ref().unwrap().info.parent_msg_id, "newer-parent");
        let rejected = app.unsent_messages.front().unwrap().clone();
        assert_eq!(rejected.text, "older rejected message");
        assert_eq!(rejected.reply.as_ref().unwrap().parent_msg_id, "parent-message");
        assert!(!app.restore_unsent_message(&rejected));
        app.chat_input_buf.clear();
        assert!(!app.restore_unsent_message(&rejected));
        app.pending_reply = None;
        assert!(app.restore_unsent_message(&rejected));
        assert_eq!(app.chat_input_buf, "older rejected message");
        assert_eq!(app.pending_reply.as_ref().unwrap().info.parent_msg_id, "parent-message");
    }

    #[test]
    fn runtime_rejection_does_not_insert_into_an_unrelated_channel() {
        let (mut app, _receiver) = test_app();
        app.state.join_channel(ChannelId::new("active"));
        app.apply_event(crust_core::AppEvent::MessageSendRejected {
            channel: ChannelId::new("background"),
            text: "unsent".to_owned(),
            reply_to_msg_id: Some("parent-without-metadata".to_owned()),
            reply: None,
            reason: "The account changed.".to_owned(),
        }, &egui::Context::default());
        assert!(app.chat_input_buf.is_empty());
        let rejected = app.unsent_messages.front().unwrap();
        assert_eq!(rejected.channel, ChannelId::new("background"));
        assert_eq!(rejected.reply_info().unwrap().parent_msg_id, "parent-without-metadata");
    }

    #[test]
    fn authentication_progress_clears_on_completion_logout_and_failure() {
        let (mut app, _receiver) = test_app();
        let ctx = egui::Context::default();
        for outcome in [
            crust_core::AppEvent::Authenticated { username: "new-account".to_owned(), user_id: "42".to_owned() },
            crust_core::AppEvent::LoggedOut,
            crust_core::AppEvent::Error { context: "SwitchAccount".to_owned(), message: "unavailable".to_owned() },
        ] {
            app.apply_event(crust_core::AppEvent::AuthenticationPending { username: Some("new-account".to_owned()) }, &ctx);
            assert_eq!(app.pending_auth.as_deref(), Some("new-account"));
            app.apply_event(outcome, &ctx);
            assert!(app.pending_auth.is_none());
        }
    }

    #[test]
    fn transient_refresh_errors_allow_retry_without_signing_out() {
        let (mut app, _receiver) = test_app();
        app.state.auth.logged_in = true;
        app.auth_refresh_inflight = true;
        app.pending_auth = Some("new-account".to_owned());
        app.apply_event(crust_core::AppEvent::Error {
            context: "RefreshAuth".to_owned(),
            message: "temporarily unavailable".to_owned(),
        }, &egui::Context::default());
        assert!(!app.auth_refresh_inflight);
        assert!(app.state.auth.logged_in);
        assert_eq!(app.pending_auth.as_deref(), Some("new-account"));
    }

    fn shortcut_input(key: egui::Key, modifiers: egui::Modifiers) -> egui::RawInput {
        egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        }
    }

    fn quick_switch_frame(
        ctx: &egui::Context,
        app: &mut super::CrustApp,
        size: egui::Vec2,
        events: Vec<egui::Event>,
        other_focus: bool,
    ) -> (bool, egui::FullOutput) {
        let mut consumed = false;
        let output = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
            events,
            ..Default::default()
        }, |ctx| {
            consumed = app.handle_quick_switch_shortcuts(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                let id = super::chat_composer_id(None);
                if other_focus {
                    ui.memory_mut(|memory| memory.request_focus(id));
                }
                ui.add(egui::TextEdit::singleline(&mut app.chat_input_buf).id(id));
            });
            app.show_channel_quick_switch(ctx);
        });
        (consumed, output)
    }

    fn visible_text_rect(output: &egui::FullOutput, wanted: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                let rect = text.galley.rect.translate(text.pos.to_vec2());
                (text.galley.text() == wanted && shape.clip_rect.contains_rect(rect)).then_some(rect)
            } else {
                None
            }
        })
    }

    fn production_ui_frame(ctx: &egui::Context, app: &mut super::CrustApp,
        events: Vec<egui::Event>) -> egui::FullOutput {
        production_ui_frame_with_response(ctx, app, events, None).0
    }

    fn production_ui_frame_with_response(ctx: &egui::Context, app: &mut super::CrustApp,
        events: Vec<egui::Event>, response_id: Option<egui::Id>)
        -> (egui::FullOutput, Option<egui::Response>) {
        let mut response = None;
        let output = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 800.0))),
            time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
            events,
            ..Default::default()
        }, |ctx| {
            app.update_ui(ctx);
            // After ctx.run returns, read_response can prefer the preceding pass.
            // Replace this on every pass so it matches the final paint output.
            response = response_id.and_then(|id| ctx.read_response(id));
        });
        (output, response)
    }

    #[test]
    fn hotkey_capture_records_ctrl_k_without_opening_quick_switch() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        app.state.join_channel(ChannelId::new("channel"));
        app.settings_open = true;
        app.settings_section = super::SettingsSection::Hotkeys;
        let action = crust_core::HotkeyAction::ZoomIn;
        let label = app.settings_hotkey_bindings.get(action).display_label();
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = production_ui_frame(&ctx, &mut app, Vec::new());
        }
        let bind = visible_text_rect(&output, &label).expect("binding button is visible").center();
        for pressed in [true, false] {
            production_ui_frame(&ctx, &mut app, vec![
                egui::Event::PointerMoved(bind),
                egui::Event::PointerButton { pos: bind, button: egui::PointerButton::Primary,
                    pressed, modifiers: egui::Modifiers::NONE },
            ]);
        }
        assert_eq!(app.hotkey_capture_target, Some(action));
        let mut events = shortcut_input(egui::Key::K, egui::Modifiers::CTRL).events;
        events.push(egui::Event::Text("k".into()));
        production_ui_frame(&ctx, &mut app, events);
        assert_eq!(app.hotkey_bindings.get(action), crust_core::KeyBinding::new("K").with_ctrl());
        assert!(app.hotkey_capture_target.is_none());
        assert!(!app.quick_switch.open);
        assert!(app.chat_input_buf.is_empty());
        assert!(std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, AppCommand::SetHotkeyBindings { .. })));
    }

    #[test]
    fn captured_keys_do_not_reach_open_popups_or_the_composer() {
        for key in [egui::Key::Escape, egui::Key::Enter, egui::Key::Tab] {
            let (mut app, mut commands) = test_app();
            let ctx = egui::Context::default();
            let channel = ChannelId::new("channel");
            app.state.join_channel(channel.clone());
            app.state.auth.logged_in = true;
            app.chat_input_buf = "draft stays here".into();
            app.settings_open = true;
            app.settings_section = super::SettingsSection::Hotkeys;
            app.emote_picker.open = true;
            app.message_search_mut(&channel).request_open();
            production_ui_frame(&ctx, &mut app, Vec::new());
            ctx.memory_mut(|memory| memory.request_focus(super::chat_composer_id(None)));
            let action = crust_core::HotkeyAction::ZoomIn;
            let previous = app.hotkey_bindings.get(action);
            app.hotkey_capture_target = Some(action);
            production_ui_frame(&ctx, &mut app, shortcut_input(key, egui::Modifiers::NONE).events);
            assert!(app.hotkey_capture_target.is_none());
            assert!(app.emote_picker.open, "{key:?} closed the picker");
            assert!(app.message_search.get(&channel).unwrap().open, "{key:?} closed search");
            assert_eq!(app.chat_input_buf, "draft stays here");
            assert!(ctx.input(|input| !input.key_pressed(key)));
            if key == egui::Key::Escape {
                assert_eq!(app.hotkey_bindings.get(action), previous);
            } else {
                assert_eq!(app.hotkey_bindings.get(action).key, super::egui_key_name(key));
            }
            assert!(!std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, AppCommand::SendMessage { .. })));
        }
    }

    #[test]
    fn invisible_hotkey_capture_does_not_suppress_app_shortcuts() {
        for (open, section) in [(false, super::SettingsSection::Hotkeys),
            (true, super::SettingsSection::Appearance)] {
            let (mut app, _commands) = test_app();
            let ctx = egui::Context::default();
            app.settings_open = open;
            app.settings_section = section;
            app.hotkey_capture_target = Some(crust_core::HotkeyAction::ZoomIn);
            let previous = app.hotkey_bindings.clone();
            production_ui_frame(&ctx, &mut app, shortcut_input(egui::Key::K, egui::Modifiers::CTRL).events);
            assert!(app.hotkey_capture_target.is_none());
            assert_eq!(app.hotkey_bindings, previous);
            assert!(app.quick_switch.open);
        }
    }

    #[test]
    fn expression_editor_owns_shortcuts_and_escape_before_chat_popups() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.settings_open = true;
        app.settings_section = super::SettingsSection::Highlights;
        app.settings_filter_editor_modal.open_highlight(0, "message.length > 10");
        app.emote_picker.open = true;
        app.message_search_mut(&channel).request_open();
        app.chat_input_buf = "draft stays here".into();
        for _ in 0..3 {
            production_ui_frame(&ctx, &mut app, Vec::new());
        }
        for key in [egui::Key::K, egui::Key::W] {
            production_ui_frame(&ctx, &mut app, shortcut_input(key, egui::Modifiers::CTRL).events);
            assert!(app.settings_filter_editor_modal.open);
            assert!(!app.quick_switch.open);
            assert!(app.state.channels.contains_key(&channel));
        }
        production_ui_frame(&ctx, &mut app,
            shortcut_input(egui::Key::Escape, egui::Modifiers::NONE).events);
        assert!(!app.settings_filter_editor_modal.open);
        assert!(app.settings_filter_editor_modal.draft.is_empty());
        assert!(app.settings_open);
        assert!(app.emote_picker.open);
        assert!(app.message_search.get(&channel).unwrap().open);
        assert_eq!(app.chat_input_buf, "draft stays here");
        assert!(!std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, AppCommand::SendMessage { .. })));

        app.settings_filter_editor_modal.open_highlight(0, "unsaved draft");
        app.settings_open = false;
        production_ui_frame(&ctx, &mut app, Vec::new());
        assert!(!app.settings_filter_editor_modal.open);
        assert!(app.settings_filter_editor_modal.draft.is_empty());
    }

    #[test]
    fn quick_switch_leaves_another_composers_navigation_keys_alone() {
        let (mut app, mut commands) = test_app();
        app.state.join_channel(ChannelId::new("first"));
        app.state.join_channel(ChannelId::new("second"));
        app.state.active_channel = Some(ChannelId::new("second"));
        app.state.auth.logged_in = true;
        app.chat_input_buf = "message for second".to_owned();
        let ctx = egui::Context::default();
        let size = egui::vec2(800.0, 600.0);
        app.open_channel_quick_switch();
        let input = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input(Vec::new()), |ctx| app.update_ui(ctx));
        ctx.memory_mut(|memory| memory.request_focus(super::chat_composer_id(None)));
        for key in [egui::Key::ArrowDown, egui::Key::ArrowUp, egui::Key::Enter] {
            let events = shortcut_input(key, egui::Modifiers::NONE).events;
            let _ = ctx.run(input(events), |ctx| app.update_ui(ctx));
            assert_eq!(app.state.active_channel, Some(ChannelId::new("second")));
            assert!(app.quick_switch.open);
        }
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| {
            matches!(command, AppCommand::SendMessage { channel, text, .. }
                if channel == ChannelId::new("second") && text == "message for second")
        }));
    }

    #[test]
    fn message_search_escape_respects_dialog_and_composer_focus() {
        fn frame(ctx: &egui::Context, app: &mut super::CrustApp, width: f32,
            events: Vec<egui::Event>) {
            let _ = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 800.0))),
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                events,
                ..Default::default()
            }, |ctx| app.update_ui(ctx));
        }

        for width in [1100.0, 620.0] {
            for dialog in ["accounts", "join", "settings"] {
                let ctx = egui::Context::default();
                let (mut app, _commands) = test_app();
                let channel = ChannelId::new("channel");
                app.state.join_channel(channel.clone());
                app.message_search_mut(&channel).request_open();
                for _ in 0..3 {
                    frame(&ctx, &mut app, width, Vec::new());
                }
                let dialog_input = match dialog {
                    "accounts" => {
                        app.login_dialog.toggle();
                        egui::Id::new("login_token_input")
                    }
                    "join" => {
                        app.join_dialog.toggle();
                        egui::Id::new("join_channel_input")
                    }
                    _ => {
                        app.settings_open = true;
                        app.settings_section = super::SettingsSection::Appearance;
                        egui::Id::new("settings_search")
                    }
                };
                for _ in 0..3 {
                    frame(&ctx, &mut app, width, Vec::new());
                }
                ctx.memory_mut(|memory| memory.request_focus(dialog_input));
                frame(&ctx, &mut app, width, if dialog == "settings" {
                    vec![egui::Event::Text("updates".to_owned())]
                } else { Vec::new() });
                assert!(ctx.memory(|memory| memory.has_focus(dialog_input)),
                    "{dialog} owns its field at width {width}");
                if dialog == "settings" {
                    assert!(app.settings_section == super::SettingsSection::Updates);
                }
                frame(&ctx, &mut app, width,
                    shortcut_input(egui::Key::Escape, egui::Modifiers::NONE).events);
                assert!(app.message_search[&channel].open,
                    "Escape in {dialog} must leave background search open at width {width}");
                match dialog {
                    "accounts" => assert!(!app.login_dialog.open),
                    "join" => assert!(!app.join_dialog.open),
                    _ => {
                        assert!(app.settings_open);
                        assert!(app.settings_section == super::SettingsSection::Appearance);
                        app.settings_open = false;
                    }
                }

                ctx.memory_mut(|memory| memory.request_focus(super::chat_composer_id(None)));
                frame(&ctx, &mut app, width, Vec::new());
                frame(&ctx, &mut app, width,
                    shortcut_input(egui::Key::Escape, egui::Modifiers::NONE).events);
                assert!(app.message_search[&channel].open,
                    "Escape in the composer must not dismiss an unfocused search");

                let keyword = egui::Id::new("message_search_field")
                    .with(channel.as_str()).with("keyword");
                ctx.memory_mut(|memory| memory.request_focus(keyword));
                frame(&ctx, &mut app, width, Vec::new());
                assert!(ctx.memory(|memory| memory.has_focus(keyword)));
                frame(&ctx, &mut app, width,
                    shortcut_input(egui::Key::Escape, egui::Modifiers::NONE).events);
                assert!(!app.message_search[&channel].open,
                    "the focused search still closes on Escape at width {width}");
            }
        }
    }

    #[test]
    fn quick_switch_keyboard_selection_survives_stationary_hover_and_scrolls_into_view() {
        let (mut app, _commands) = test_app();
        for index in 0..24 {
            app.state.join_channel(ChannelId::new(format!("room{index:02}")));
        }
        let ctx = egui::Context::default();
        let size = egui::vec2(500.0, 420.0);
        app.open_channel_quick_switch();
        let mut output = quick_switch_frame(&ctx, &mut app, size, Vec::new(), false).1;
        for _ in 0..3 {
            output = quick_switch_frame(&ctx, &mut app, size, Vec::new(), false).1;
        }
        let first = visible_text_rect(&output, "#room00").expect("first row is visible");
        quick_switch_frame(&ctx, &mut app, size, vec![egui::Event::PointerMoved(first.center())], false);
        for index in 1..24 {
            let events = shortcut_input(egui::Key::ArrowDown, egui::Modifiers::NONE).events;
            quick_switch_frame(&ctx, &mut app, size, events, false);
            assert_eq!(app.quick_switch.selected, index);
        }
        for _ in 0..5 {
            output = quick_switch_frame(&ctx, &mut app, size, Vec::new(), false).1;
        }
        assert!(visible_text_rect(&output, "#room23").is_some());
        assert_eq!(app.quick_switch.selected, 23);
    }

    #[test]
    fn quick_switch_stays_inside_narrow_viewports() {
        for size in [egui::vec2(300.0, 500.0), egui::vec2(420.0, 300.0)] {
            let (mut app, _commands) = test_app();
            app.state.join_channel(ChannelId::new("a_long_channel_name_to_truncate"));
            let ctx = egui::Context::default();
            app.open_channel_quick_switch();
            for _ in 0..4 {
                quick_switch_frame(&ctx, &mut app, size, Vec::new(), false);
            }
            let window = ctx.memory(|memory| memory.area_rect(egui::Id::new("channel_quick_switch_window"))).unwrap();
            assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(window), "{window:?} exceeds {size:?}");
        }
    }

    #[test]
    fn startup_status_keeps_the_composer_and_join_dialog_usable() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("rustlang");
        app.state.join_channel(channel.clone());
        app.state.active_channel = Some(channel);
        app.state.auth.logged_in = true;
        let input = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input(Vec::new()), |ctx| app.update_ui(ctx));
        ctx.memory_mut(|memory| memory.request_focus(super::chat_composer_id(None)));
        let _ = ctx.run(input(vec![egui::Event::Text("startup draft".to_owned())]), |ctx| app.update_ui(ctx));
        assert_eq!(app.chat_input_buf, "startup draft");
        assert!(app.loading_screen.is_active());

        app.join_dialog.toggle();
        let _ = ctx.run(input(Vec::new()), |ctx| app.update_ui(ctx));
        let _ = ctx.run(input(vec![egui::Event::Text("anotherchannel".to_owned())]), |ctx| app.update_ui(ctx));
        let events = shortcut_input(egui::Key::Enter, egui::Modifiers::NONE).events;
        let _ = ctx.run(input(events), |ctx| app.update_ui(ctx));
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| {
            matches!(command, AppCommand::JoinChannel { channel } if channel == ChannelId::new("anotherchannel"))
        }));
        assert_eq!(app.chat_input_buf, "startup draft");
        assert!(app.loading_screen.is_active());
    }

    fn whisper_event(owner_user_id: &str, from: &str, target: &str, is_self: bool, is_history: bool) -> crust_core::AppEvent {
        crust_core::AppEvent::WhisperReceived {
            owner_user_id: owner_user_id.to_owned(),
            from_user_id: if is_self { owner_user_id.to_owned() } else { String::new() },
            from_login: from.to_owned(),
            from_display_name: from.to_owned(),
            target_login: target.to_owned(),
            text: "private message".to_owned(),
            twitch_emotes: Vec::new(),
            is_self,
            timestamp: chrono::Utc::now(),
            is_history,
        }
    }

    #[test]
    fn incoming_and_replayed_whispers_resolve_their_own_senders_emotes() {
        for is_history in [false, true] {
            let (mut app, _commands) = test_app();
            let ctx = egui::Context::default();
            app.apply_event(crust_core::AppEvent::Authenticated {
                username: "alice".into(), user_id: "10".into(),
            }, &ctx);
            app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated {
                emotes: vec![scoped_emote("global", None, None)],
            }, &ctx);
            for (sender_id, from, target, is_self) in [
                ("20", "bob", "alice", false),
                ("30", "carol", "alice", false),
                ("", "unknown", "alice", false),
                ("10", "alice", "bob", true),
            ] {
                let mut event = whisper_event("10", from, target, is_self, is_history);
                if let crust_core::AppEvent::WhisperReceived { from_user_id, text, .. } = &mut event {
                    *from_user_id = sender_id.into();
                    *text = "SharedCode".into();
                }
                app.apply_event(event, &ctx);
            }
            app.apply_event(crust_core::AppEvent::EmoteCatalogUpdated { emotes: vec![
                scoped_emote("global", None, None),
                scoped_emote("bob-emote", None, Some("20")),
                scoped_emote("carol-emote", None, Some("30")),
                scoped_emote("alice-emote", None, Some("10")),
            ] }, &ctx);
            app.refresh_emote_messages(&ctx);
            let url = |line: &super::WhisperLine| line.spans.iter().find_map(|span| match span {
                crust_core::Span::Emote { url, .. } => Some(url.clone()),
                _ => None,
            });
            assert_eq!(app.whisper_threads["bob"][0].from_user_id, "20");
            assert_eq!(url(&app.whisper_threads["bob"][0]).as_deref(), Some("bob-emote"));
            assert_eq!(url(&app.whisper_threads["bob"][1]).as_deref(), Some("alice-emote"));
            assert_eq!(url(&app.whisper_threads["carol"][0]).as_deref(), Some("carol-emote"));
            assert_eq!(url(&app.whisper_threads["unknown"][0]).as_deref(), Some("global"));

            let mut live = whisper_event("10", "bob", "alice", false, is_history);
            if let crust_core::AppEvent::WhisperReceived { from_user_id, text, .. } = &mut live {
                *from_user_id = "20".into();
                *text = "SharedCode".into();
            }
            app.apply_event(live, &ctx);
            assert_eq!(url(app.whisper_threads["bob"].back().unwrap()).as_deref(), Some("bob-emote"));
        }
    }

    #[test]
    fn sender_cosmetics_update_owned_rows_and_clear_removed_selections_without_prefetching() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("room");
        app.state.join_channel(channel.clone());
        let kick = ChannelId::kick("room");
        app.state.join_channel(kick.clone());
        let mut kick_message = search_message(kick.clone());
        kick_message.sender.user_id = crust_core::UserId("painted".to_owned());
        app.state.channels.get_mut(&kick).unwrap().messages.push_back(kick_message);
        for index in 0..450 {
            let mut message = search_message(channel.clone());
            message.id = crust_core::MessageId(index);
            message.sender.user_id = crust_core::UserId(
                if index == 449 { "other" } else { "painted" }.to_owned());
            message.sender.badges = vec![crust_core::model::Badge {
                name: "moderator".to_owned(), version: "1".to_owned(), url: None,
            }];
            if index >= 448 {
                app.state.mentions.push_back(message.clone());
            }
            app.state.channels.get_mut(&channel).unwrap().messages.push_back(message);
        }
        let paint = crust_core::model::SenderNamePaint {
            function: "url".to_owned(),
            image_url: Some("https://example.test/paint.webp".to_owned()),
            ..Default::default()
        };
        let update = |name_paint: Option<crust_core::model::SenderNamePaint>| {
            let selected = name_paint.is_some();
            crust_core::AppEvent::SenderCosmeticsUpdated {
                user_id: "painted".to_owned(), color: None, name_paint,
                badge: selected.then(|| crust_core::model::Badge {
                    name: "7tv".to_owned(), version: "supporter".to_owned(),
                    url: Some("https://example.test/badge.webp".to_owned()),
                }),
                avatar_url: selected.then(|| "https://example.test/avatar.webp".to_owned()),
            }
        };
        let heights_key = egui::Id::new("msg_row_h").with(channel.as_str());
        let mentions_key = egui::Id::new("msg_row_h").with(ChannelId::mentions().as_str());
        ctx.data_mut(|data| {
            data.insert_temp(heights_key, std::collections::HashMap::from([
                (49u64, 20.0f32), (50, 20.0), (449, 20.0),
            ]));
            data.insert_temp(mentions_key, std::collections::HashMap::from([
                (448u64, 20.0f32), (449, 20.0),
            ]));
        });
        app.stv_avatars.insert("other".to_owned(), "other-avatar".to_owned());
        app.apply_event(update(Some(paint.clone())), &ctx);
        let messages = &app.state.channels[&channel].messages;
        assert!(messages[49].sender.name_paint.is_none(), "older rows stay outside the bounded backfill");
        assert_eq!(messages[50].sender.name_paint.as_ref().unwrap().image_url.as_deref(),
            Some("https://example.test/paint.webp"));
        assert!(messages[449].sender.name_paint.is_none());
        assert!(app.state.channels[&kick].messages[0].sender.name_paint.is_none());
        assert!(app.state.mentions[0].sender.name_paint.is_some());
        assert!(app.state.mentions[1].sender.name_paint.is_none());
        assert_eq!(messages[50].sender.badges[0].name, "7tv");
        assert!(app.stv_avatars.contains_key("painted"));
        ctx.data_mut(|data| {
            let heights = data.get_temp::<std::collections::HashMap<u64, f32>>(heights_key).unwrap();
            assert!(heights.contains_key(&49));
            assert!(!heights.contains_key(&50));
            assert!(heights.contains_key(&449));
            let mentions = data.get_temp::<std::collections::HashMap<u64, f32>>(mentions_key).unwrap();
            assert!(!mentions.contains_key(&448));
            assert!(mentions.contains_key(&449));
            data.insert_temp(heights_key, std::collections::HashMap::from([(50u64, 25.0f32)]));
        });
        app.apply_event(update(Some(paint)), &ctx);
        assert!(ctx.data_mut(|data| data.get_temp::<std::collections::HashMap<u64, f32>>(heights_key)
            .unwrap().contains_key(&50)), "unchanged cosmetics retain measured rows");
        app.emote_bytes.end_frame(&ctx, |_| panic!("paint metadata must not prefetch a hidden image"));
        assert!(commands.try_recv().is_err());

        app.apply_event(update(None), &ctx);
        let sender = &app.state.channels[&channel].messages[50].sender;
        assert!(sender.name_paint.is_none());
        assert_eq!(sender.badges.iter().map(|badge| badge.name.as_str()).collect::<Vec<_>>(), ["moderator"]);
        assert!(app.state.mentions[0].sender.name_paint.is_none());
        assert_eq!(app.state.mentions[0].sender.badges.len(), 1);
        assert!(!app.stv_avatars.contains_key("painted"));
        assert_eq!(app.stv_avatars.get("other").map(String::as_str), Some("other-avatar"));
        assert!(!ctx.data_mut(|data| data.get_temp::<std::collections::HashMap<u64, f32>>(heights_key)
            .unwrap().contains_key(&50)), "removing a badge invalidates its row measurement");
        app.emote_bytes.end_frame(&ctx, |_| panic!("removing cosmetics must not request images"));
    }

    #[test]
    fn self_avatar_responses_follow_the_active_account() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let auth = |login: &str| crust_core::AppEvent::Authenticated {
            username: login.to_owned(), user_id: login.to_owned(),
        };
        let avatar = |account: &str, url: &str| crust_core::AppEvent::SelfAvatarLoaded {
            account: account.to_owned(), avatar_url: url.to_owned(),
        };
        app.apply_event(avatar("first", "logged-out"), &ctx);
        assert!(app.state.auth.avatar_url.is_none());
        app.apply_event(auth("First"), &ctx);
        app.apply_event(avatar("first", "first-image"), &ctx);
        assert_eq!(app.state.auth.avatar_url.as_deref(), Some("first-image"));

        app.apply_event(auth("Second"), &ctx);
        app.apply_event(avatar("first", "late-first-image"), &ctx);
        assert!(app.state.auth.avatar_url.is_none());
        app.apply_event(avatar("SECOND", "second-image"), &ctx);
        app.apply_event(avatar("first", "another-late-first-image"), &ctx);
        assert_eq!(app.state.auth.avatar_url.as_deref(), Some("second-image"));

        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        app.apply_event(avatar("second", "late-second-image"), &ctx);
        app.apply_event(avatar("", "unowned-image"), &ctx);
        assert!(app.state.auth.avatar_url.is_none());
    }

    #[test]
    fn whispers_reject_logged_out_and_other_account_live_or_history_events() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        for is_history in [false, true] {
            app.apply_event(whisper_event("1", "friend", "old", false, is_history), &ctx);
            app.apply_event(whisper_event("1", "old", "friend", true, is_history), &ctx);
        }
        assert!(app.whisper_threads.is_empty());
        app.apply_event(crust_core::AppEvent::Authenticated { username: "current".to_owned(), user_id: "2".to_owned() }, &ctx);
        for is_history in [false, true] {
            app.apply_event(whisper_event("1", "friend", "old", false, is_history), &ctx);
            app.apply_event(whisper_event("1", "old", "friend", true, is_history), &ctx);
            app.apply_event(whisper_event("2", "friend", "", false, is_history), &ctx);
            app.apply_event(whisper_event("2", "", "friend", true, is_history), &ctx);
        }
        assert!(app.whisper_threads.is_empty());
        assert!(app.whisper_order.is_empty());
        assert!(app.whisper_unread.is_empty());

        app.apply_event(whisper_event("2", "Friend", "CURRENT", false, false), &ctx);
        app.apply_event(whisper_event("2", "CURRENT", "Friend", true, false), &ctx);
        assert_eq!(app.whisper_threads["friend"].len(), 2);
        assert_eq!(app.whisper_unread["friend"], 1);
        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        app.apply_event(whisper_event("2", "friend", "current", false, true), &ctx);
        assert!(app.whisper_threads.is_empty());
    }

    #[test]
    fn late_whisper_failures_do_not_reach_a_replacement_account() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        let channel = ChannelId::new("room");
        app.state.join_channel(channel.clone());
        app.state.active_channel = Some(channel.clone());
        let auth = |id: &str| crust_core::AppEvent::Authenticated {
            username: "alice".into(), user_id: id.into(),
        };
        let failed = |owner: &str| crust_core::AppEvent::WhisperSendFailed {
            owner_user_id: owner.into(), target_login: "bob".into(), text: "hello".into(),
            message: "Could not send whisper to bob: unavailable".into(),
        };
        app.apply_event(auth("1"), &ctx);
        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        app.apply_event(failed("1"), &ctx);
        assert!(commands.try_recv().is_err());
        assert!(app.event_toasts.is_empty());
        assert!(app.event_toast_queue.is_empty());
        app.apply_event(auth("2"), &ctx);
        app.apply_event(failed("1"), &ctx);
        app.apply_event(failed(""), &ctx);
        assert!(commands.try_recv().is_err());
        assert!(app.event_toasts.is_empty());
        assert!(app.event_toast_queue.is_empty());
        app.apply_event(failed("2"), &ctx);
        assert_eq!(app.event_toasts.len(), 1);
        assert_eq!(app.event_toasts[0].text, "Could not send whisper to bob: unavailable");
        assert_eq!(app.event_toasts[0].owner_user_id.as_deref(), Some("2"));
        assert!(app.state.channels[&channel].messages.is_empty());
        assert!(commands.try_recv().is_err());

        // An account switch clears both displayed and delayed private notices.
        let private = app.event_toasts[0].clone();
        app.event_toast_queue.push_back(private.clone());
        let mut unrelated = private;
        unrelated.owner_user_id = None;
        unrelated.text = "Public channel notice".into();
        app.event_toasts.push(unrelated.clone());
        app.event_toast_queue.push_back(unrelated);
        app.apply_event(auth("3"), &ctx);
        assert_eq!(app.event_toasts.len(), 1);
        assert_eq!(app.event_toast_queue.len(), 1);
        assert!(app.event_toasts.iter().chain(app.event_toast_queue.iter())
            .all(|toast| toast.owner_user_id.is_none() && toast.text == "Public channel notice"));

        app.apply_event(failed("3"), &ctx);
        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        assert_eq!(app.event_toasts.len() + app.event_toast_queue.len(), 2);
        assert!(app.event_toasts.iter().chain(app.event_toast_queue.iter())
            .all(|toast| toast.owner_user_id.is_none()));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn reused_account_logins_cannot_receive_the_previous_owners_whispers() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let auth = |user_id: &str| crust_core::AppEvent::Authenticated {
            username: "alice".to_owned(), user_id: user_id.to_owned(),
        };
        app.apply_event(auth("1"), &ctx);
        app.apply_event(whisper_event("1", "bob", "alice", false, false), &ctx);
        assert_eq!(app.whisper_threads["bob"].len(), 1);
        app.apply_event(auth("2"), &ctx);
        assert!(app.whisper_threads.is_empty());
        for is_history in [false, true] {
            app.apply_event(whisper_event("1", "bob", "alice", false, is_history), &ctx);
            app.apply_event(whisper_event("1", "alice", "bob", true, is_history), &ctx);
            app.apply_event(whisper_event("", "bob", "alice", false, is_history), &ctx);
        }
        assert!(app.whisper_threads.is_empty());
        assert!(app.whisper_unread.is_empty());
        app.apply_event(whisper_event("2", "bob", "alice", false, false), &ctx);
        app.apply_event(whisper_event("2", "alice", "bob", true, false), &ctx);
        assert_eq!(app.whisper_threads["bob"].len(), 2);
    }

    #[test]
    fn direct_account_change_clears_whispers_but_same_account_refresh_keeps_them() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        let auth = |name: &str| crust_core::AppEvent::Authenticated { username: name.to_owned(), user_id: name.to_ascii_lowercase() };
        app.apply_event(auth("first"), &ctx);
        app.apply_event(whisper_event("first", "friend", "first", false, true), &ctx);
        app.chat_input_buf = "draft to keep".to_owned();
        app.apply_event(auth("FIRST"), &ctx);
        assert_eq!(app.whisper_threads["friend"].len(), 1);
        app.apply_event(auth("second"), &ctx);
        assert!(app.whisper_threads.is_empty());
        assert!(app.active_whisper_login.is_none());
        assert_eq!(app.chat_input_buf, "draft to keep");
        app.apply_event(whisper_event("first", "friend", "first", false, true), &ctx);
        app.apply_event(whisper_event("second", "friend", "second", false, true), &ctx);
        assert_eq!(app.whisper_threads["friend"].len(), 1);
    }

    #[test]
    fn whisper_reply_keeps_channel_and_recipient_drafts_separate() {
        let (mut app, _commands) = test_app();
        let channel = ChannelId::new("rustlang");
        app.state.join_channel(channel.clone());
        app.state.join_channel(ChannelId::new("other"));
        app.split_panes.add_pane(ChannelId::new("other"), None);
        app.split_panes.add_pane(channel.clone(), None);
        app.split_panes.focused = 1;
        app.state.active_channel = Some(channel);
        app.chat_input_buf = "hidden draft".to_owned();
        app.split_panes.panes[0].input_buf = "other draft".to_owned();
        app.split_panes.panes[1].input_buf = "current draft".to_owned();
        app.open_whisper_compose("friend");
        app.whisper_composers.get_mut("friend").unwrap().text = "private draft".to_owned();
        assert_eq!(app.split_panes.panes[1].input_buf, "current draft");
        assert_eq!(app.chat_input_buf, "hidden draft");
        assert_eq!(app.split_panes.panes[0].input_buf, "other draft");
        app.open_whisper_compose("someoneelse");
        assert_eq!(app.split_panes.panes[1].input_buf, "current draft");
        assert_eq!(app.active_whisper_login.as_deref(), Some("someoneelse"));
        assert_eq!(app.whisper_composers["friend"].text, "private draft");
        assert!(app.whisper_composers["someoneelse"].text.is_empty());
        app.open_whisper_compose("@FRIEND");
        assert_eq!(app.active_whisper_login.as_deref(), Some("friend"));
        assert_eq!(app.whisper_composers["friend"].text, "private draft");
    }

    fn whisper_ui_frame(
        ctx: &egui::Context,
        app: &mut super::CrustApp,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                events,
                ..Default::default()
            },
            |ctx| app.update_ui(ctx),
        )
    }

    #[test]
    fn whisper_reply_sends_from_the_account_without_a_joined_channel() {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(64);
        let (events, event_receiver) = tokio::sync::mpsc::channel(64);
        let mut app = super::CrustApp::with_event_channels(commands, event_receiver);
        let ctx = egui::Context::default();
        events
            .try_send(crust_core::AppEvent::Authenticated {
                username: "alice".into(),
                user_id: "10".into(),
            })
            .unwrap();
        events
            .try_send(whisper_event("10", "friend", "alice", false, false))
            .unwrap();
        production_ui_frame(&ctx, &mut app, Vec::new());
        app.whispers_visible = true;
        app.chat_input_buf = "public draft to keep".into();
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = production_ui_frame(&ctx, &mut app, Vec::new());
        }
        let reply = visible_text_rect(&output, "Reply").expect("reply action must be visible");
        click_production_ui(&ctx, &mut app, reply.center());
        production_ui_frame(&ctx, &mut app, Vec::new());
        assert!(ctx.memory(|memory| memory.has_focus(app.whisper_composer_id("friend"))));
        production_ui_frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("  account reply  ".into())],
        );
        production_ui_frame(
            &ctx,
            &mut app,
            shortcut_input(egui::Key::Enter, egui::Modifiers::NONE).events,
        );
        let mut sends = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            match command {
                AppCommand::SendWhisper { target_login, text } => sends.push((target_login, text)),
                AppCommand::SendMessage { .. } | AppCommand::JoinChannel { .. } => {
                    panic!("a whisper must not join or send to a channel")
                }
                _ => {}
            }
        }
        assert_eq!(
            sends,
            vec![("friend".to_owned(), "  account reply  ".to_owned())]
        );
        assert!(app.state.channel_order.is_empty());
        assert!(app.state.active_channel.is_none());
        assert_eq!(app.chat_input_buf, "public draft to keep");
        assert_eq!(app.whisper_composers["friend"].text, "  account reply  ");
        assert_eq!(app.whisper_composers["friend"].pending.len(), 1);
        let mut sent = whisper_event("10", "alice", "friend", true, false);
        if let crust_core::AppEvent::WhisperReceived { text, .. } = &mut sent {
            *text = "account reply".into();
        }
        events.try_send(sent).unwrap();
        production_ui_frame(&ctx, &mut app, Vec::new());
        assert!(app.whisper_composers["friend"].text.is_empty());
        assert!(app.whisper_composers["friend"].pending.is_empty());
    }

    #[test]
    fn whisper_failures_preserve_newer_drafts_and_reject_cleared_attempts() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        let auth = |user: &str| crust_core::AppEvent::Authenticated {
            username: user.into(),
            user_id: user.into(),
        };
        let failure = |owner: &str, text: &str| crust_core::AppEvent::WhisperSendFailed {
            owner_user_id: owner.into(),
            target_login: "friend".into(),
            text: text.into(),
            message: "The recipient cannot receive this whisper.".into(),
        };
        app.apply_event(auth("alice"), &ctx);
        let channel = ChannelId::new("room");
        assert!(app.submit_chat_input(&channel, "/w friend original message".into(), None, None));
        assert!(matches!(
            commands.try_recv(),
            Ok(AppCommand::SendWhisper { .. })
        ));
        app.whisper_composers.get_mut("friend").unwrap().text = "newer draft".into();
        app.apply_event(failure("alice", "original message"), &ctx);
        let composer = &app.whisper_composers["friend"];
        assert_eq!(composer.text, "newer draft");
        assert!(composer.pending.is_empty());
        assert_eq!(composer.failed.front().unwrap().text, "original message");
        assert!(app.whispers_visible);
        assert_eq!(app.active_whisper_login.as_deref(), Some("friend"));

        app.state.join_channel(channel.clone());
        app.state.active_channel = Some(channel.clone());
        assert!(app.submit_chat_input(&channel, "/w friend another attempt".into(), None, None));
        app.apply_event(
            crust_core::AppEvent::Error {
                context: "SendWhisper".into(),
                message: "Account switching is in progress.".into(),
            },
            &ctx,
        );
        assert_eq!(app.whisper_composers["friend"].text, "newer draft");
        assert!(app.whisper_composers["friend"].pending.is_empty());
        assert_eq!(app.whisper_composers["friend"].failed.len(), 2);
        while let Ok(command) = commands.try_recv() {
            assert!(
                !matches!(command, AppCommand::InjectLocalMessage { .. }),
                "private send errors must stay out of public channel history"
            );
        }
        assert!(app.state.channels[&channel].messages.is_empty());
        assert!(app
            .event_toasts
            .iter()
            .chain(&app.event_toast_queue)
            .all(|toast| toast.owner_user_id.as_deref() == Some("alice")));

        assert!(app.submit_chat_input(&channel, "/w friend delayed attempt".into(), None, None));
        app.apply_event(auth("bob"), &ctx);
        assert!(app.whisper_composers.is_empty());
        assert!(app.focus_whisper_composer.is_none());
        app.apply_event(auth("alice"), &ctx);
        app.apply_event(failure("alice", "delayed attempt"), &ctx);
        assert!(app.whisper_composers.is_empty());
        assert!(app.whisper_threads.is_empty());
        assert!(!app.whispers_visible);

        assert!(app.submit_chat_input(&channel, "/w friend recover this".into(), None, None));
        app.apply_event(failure("alice", "recover this"), &ctx);
        assert_eq!(app.whisper_composers["friend"].text, "recover this");
        app.apply_event(crust_core::AppEvent::LoggedOut, &ctx);
        assert!(app.whisper_composers.is_empty());
    }

    #[test]
    fn whisper_window_keeps_the_composer_visible_and_validates_unicode_at_narrow_sizes() {
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        app.apply_event(
            crust_core::AppEvent::Authenticated {
                username: "alice".into(),
                user_id: "10".into(),
            },
            &ctx,
        );
        app.apply_event(
            whisper_event("10", "long_recipient_name", "alice", false, false),
            &ctx,
        );
        app.open_whisper_compose("long_recipient_name");
        for size in [
            egui::vec2(1100.0, 800.0),
            egui::vec2(320.0, 360.0),
            egui::vec2(220.0, 200.0),
        ] {
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = whisper_ui_frame(&ctx, &mut app, size, Vec::new());
            }
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            let window = ctx
                .memory(|memory| memory.area_rect(egui::Id::new("Whispers")))
                .unwrap();
            assert!(
                screen.contains_rect(window),
                "whisper window {window:?} exceeds {screen:?}"
            );
            let editor = ctx
                .read_response(app.whisper_composer_id("long_recipient_name"))
                .unwrap();
            assert!(
                screen.contains_rect(editor.rect),
                "composer {:?} exceeds {screen:?}",
                editor.rect
            );
            assert!(window.contains_rect(editor.rect));
            assert!(
                visible_text_rect(&output, "Send").is_some(),
                "Send must remain visible at {size:?}"
            );
            for pressed in [true, false] {
                whisper_ui_frame(
                    &ctx,
                    &mut app,
                    size,
                    vec![
                        egui::Event::PointerMoved(editor.rect.center()),
                        egui::Event::PointerButton {
                            pos: editor.rect.center(),
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(ctx.memory(|memory| memory.has_focus(editor.id)),
                "whisper editor {:?} at {size:?} did not receive its click; focused={:?}, layer={:?}, hit={:?}, after={:?}",
                editor.rect, ctx.memory(|memory| memory.focused()), editor.layer_id,
                ctx.layer_id_at(editor.rect.center()), ctx.read_response(editor.id).map(|response| response.rect));
        }
        let size = egui::vec2(220.0, 200.0);
        whisper_ui_frame(
            &ctx,
            &mut app,
            size,
            vec![egui::Event::Text("界".repeat(501))],
        );
        let output = whisper_ui_frame(
            &ctx,
            &mut app,
            size,
            shortcut_input(egui::Key::Enter, egui::Modifiers::NONE).events,
        );
        assert!(visible_text_rect(&output, "501/500").is_some());
        while let Ok(command) = commands.try_recv() {
            assert!(!matches!(command, AppCommand::SendWhisper { .. }));
        }
        assert_eq!(
            app.whisper_composers["long_recipient_name"]
                .text
                .chars()
                .count(),
            501
        );
        whisper_ui_frame(
            &ctx,
            &mut app,
            size,
            shortcut_input(egui::Key::Backspace, egui::Modifiers::NONE).events,
        );
        let output = whisper_ui_frame(&ctx, &mut app, size, Vec::new());
        assert!(visible_text_rect(&output, "500/500").is_some());
        let send = visible_text_rect(&output, "Send").unwrap();
        for pressed in [true, false] {
            whisper_ui_frame(
                &ctx,
                &mut app,
                size,
                vec![
                    egui::Event::PointerMoved(send.center()),
                    egui::Event::PointerButton {
                        pos: send.center(),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok())
            .filter_map(|command| match command {
                AppCommand::SendWhisper { target_login, text } => Some((target_login, text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![("long_recipient_name".to_owned(), "界".repeat(500))]
        );
        drop(commands);
        whisper_ui_frame(&ctx, &mut app, size, Vec::new());
        assert!(app.whisper_composers["long_recipient_name"]
            .pending
            .is_empty());
        assert_eq!(
            app.whisper_composers["long_recipient_name"].text,
            "界".repeat(500)
        );
    }

    #[test]
    fn whisper_success_keeps_edits_made_while_the_previous_message_was_sending() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        app.apply_event(
            crust_core::AppEvent::Authenticated {
                username: "alice".into(),
                user_id: "10".into(),
            },
            &ctx,
        );
        app.submit_chat_input(
            &ChannelId::new("room"),
            "/w friend previous message".into(),
            None,
            None,
        );
        app.whisper_composers.get_mut("friend").unwrap().text = "next message".into();
        let mut sent = whisper_event("10", "alice", "friend", true, false);
        if let crust_core::AppEvent::WhisperReceived { text, .. } = &mut sent {
            *text = "previous message".into();
        }
        app.apply_event(sent, &ctx);
        assert_eq!(app.whisper_composers["friend"].text, "next message");
        assert!(app.whisper_composers["friend"].pending.is_empty());
    }

    fn search_message(channel: ChannelId) -> crust_core::ChatMessage {
        crust_core::ChatMessage {
            id: crust_core::MessageId(1),
            server_id: Some("search-server-id".to_owned()),
            timestamp: chrono::Utc::now(),
            channel,
            sender: crust_core::model::Sender {
                user_id: crust_core::model::UserId("1".to_owned()),
                login: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: "search target".to_owned(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: Default::default(),
            reply: None,
            msg_kind: crust_core::model::MsgKind::Chat,
            shared: None,
        }
    }

    #[test]
    fn ui_zoom_preserves_native_density_and_monitor_changes() {
        let (mut app, _receiver) = test_app();
        let original_ui_size = t::ui_font_size();
        let ctx = egui::Context::default();
        for (native_density, zoom) in [(1.25, 1.0), (2.0, 1.0), (1.5, 1.25), (1.0, 1.25), (2.0, 0.75)] {
            app.ui_font_size = zoom;
            for _ in 0..2 {
                let mut input = egui::RawInput::default();
                input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap()
                    .native_pixels_per_point = Some(native_density);
                let _ = ctx.run(input, |ctx| app.apply_font_scale(ctx));
            }
            assert!((ctx.zoom_factor() - zoom).abs() < 0.001);
            assert!((ctx.pixels_per_point() - native_density * zoom).abs() < 0.001);
            let (fresh, _receiver) = test_app();
            assert_eq!(fresh.chat_font_size, t::DEFAULT_CHAT_FONT_SIZE);
            assert_eq!(fresh.ui_font_size, t::DEFAULT_UI_FONT_SIZE,
                "a fresh fixture inherited another app's zoom {zoom}");
        }
        t::set_ui_font_size(original_ui_size);
    }

    #[test]
    fn previous_tab_shortcut_is_not_consumed_as_next_tab() {
        let (mut app, _commands) = test_app();
        for channel in ["first", "second", "third"] {
            app.state.join_channel(ChannelId::new(channel));
        }
        app.state.active_channel = Some(ChannelId::new("second"));
        let ctx = egui::Context::default();
        let _ = ctx.run(shortcut_input(egui::Key::Tab, egui::Modifiers::CTRL | egui::Modifiers::SHIFT), |ctx| {
            app.handle_channel_shortcuts(ctx);
        });
        assert_eq!(app.state.active_channel, Some(ChannelId::new("first")));
    }

    #[test]
    fn tab_and_split_movement_do_not_trigger_navigation_aliases() {
        let (mut app, _commands) = test_app();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        app.state.join_channel(first.clone());
        app.state.join_channel(second.clone());
        app.state.active_channel = Some(second.clone());
        let ctx = egui::Context::default();
        let _ = ctx.run(shortcut_input(egui::Key::ArrowLeft, egui::Modifiers::ALT | egui::Modifiers::SHIFT), |ctx| {
            app.handle_channel_shortcuts(ctx);
        });
        assert_eq!(app.state.active_channel, Some(second.clone()));
        assert_eq!(app.state.channel_order, vec![second.clone(), first.clone()]);

        app.split_panes.add_pane(first.clone(), None);
        app.split_panes.add_pane(second.clone(), None);
        let _ = ctx.run(shortcut_input(egui::Key::PageDown, egui::Modifiers::CTRL | egui::Modifiers::ALT), |ctx| {
            app.handle_channel_shortcuts(ctx);
        });
        assert_eq!(app.split_panes.focused, 1);
        assert_eq!(app.split_panes.panes[0].channel, first);
        assert_eq!(app.split_panes.panes[1].channel, second);
    }

    #[test]
    fn split_navigation_moves_text_input_to_the_selected_pane() {
        let (mut app, _commands) = test_app();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        app.split_panes.add_pane(first, None);
        app.split_panes.add_pane(second.clone(), None);
        let ctx = egui::Context::default();
        let mut input = shortcut_input(egui::Key::PageDown, egui::Modifiers::CTRL | egui::Modifiers::ALT);
        input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 200.0)));
        input.events.push(egui::Event::Text("selected pane".to_owned()));
        let _ = ctx.run(input, |ctx| {
            ctx.memory_mut(|memory| memory.request_focus(super::chat_composer_id(Some(app.split_panes.panes[0].id))));
            app.handle_channel_shortcuts(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                for pane in &mut app.split_panes.panes {
                    ui.add(egui::TextEdit::singleline(&mut pane.input_buf)
                        .id(super::chat_composer_id(Some(pane.id))));
                }
            });
        });
        assert_eq!(app.state.active_channel, Some(second));
        assert!(app.split_panes.panes[0].input_buf.is_empty());
        assert_eq!(app.split_panes.panes[1].input_buf, "selected pane");
    }

    #[test]
    fn removing_and_reordering_other_splits_keeps_the_selected_composer() {
        let mut splits = SplitPanes::default();
        for channel in ["first", "selected", "last"] {
            splits.add_pane(ChannelId::new(channel), None);
        }
        splits.focused = 1;
        let id = splits.panes[1].id;
        splits.remove_pane(0);
        assert_eq!(splits.panes[splits.focused].id, id);
        splits.add_pane(ChannelId::new("new"), Some(0));
        assert_eq!(splits.panes[splits.focused].id, id);
        splits.move_focused(1);
        assert_eq!(splits.panes[splits.focused].id, id);
        assert_eq!(splits.focused_channel(), Some(&ChannelId::new("selected")));
    }

    #[test]
    fn replacing_a_split_channel_keeps_its_draft_selection_and_reply_scope() {
        let (mut app, _commands) = test_app();
        let ctx = egui::Context::default();
        for name in ["first", "second", "third"] {
            app.state.join_channel(ChannelId::new(name));
        }
        app.split_panes.add_pane(ChannelId::new("first"), None);
        app.split_panes.add_pane(ChannelId::new("second"), None);
        app.split_panes.panes[0].input_buf = "other draft".to_owned();
        app.split_panes.panes[1].input_buf = "keep 🙂 text".to_owned();
        app.activate_channel(ChannelId::new("second"));
        let id = super::chat_composer_id(Some(app.split_panes.panes[1].id));
        for _ in 0..3 {
            production_ui_frame(&ctx, &mut app, Vec::new());
        }
        let mut editor = egui::TextEdit::load_state(&ctx, id).expect("split editor state");
        editor.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0), egui::text::CCursor::new(4),
        )));
        egui::TextEdit::store_state(&ctx, id, editor);
        app.pending_reply = Some(super::PendingReply {
            channel: ChannelId::new("second"),
            info: crust_core::ReplyInfo {
                parent_msg_id: "second-parent".into(),
                parent_user_login: "sender".into(),
                parent_display_name: "Sender".into(),
                parent_msg_body: "original message".into(),
            },
        });

        app.activate_channel(ChannelId::new("third"));
        assert_eq!(app.split_panes.panes[1].input_buf, "keep 🙂 text");
        assert!(app.reply_for_composer(&ChannelId::new("third"), Some(1)).is_none());
        production_ui_frame(&ctx, &mut app, vec![egui::Event::Text("kept".to_owned())]);
        assert_eq!(app.split_panes.panes[1].input_buf, "kept 🙂 text");
        assert_eq!(app.split_panes.panes[0].input_buf, "other draft");
        assert!(ctx.memory(|memory| memory.has_focus(id)));
        app.activate_channel(ChannelId::new("second"));
        assert_eq!(app.split_panes.panes[1].input_buf, "kept 🙂 text");
        assert_eq!(app.reply_for_composer(&ChannelId::new("second"), Some(1))
            .unwrap().parent_msg_id, "second-parent");
    }

    #[test]
    fn closing_splits_routes_typing_to_the_surviving_draft() {
        for (pane_count, focused, close, keep, expected, remaining) in [
            (3, 1, Some(1), None, "third", 2),
            (2, 0, Some(0), None, "second", 0),
            (3, 0, None, Some(1), "second", 0),
            (3, 2, Some(0), None, "third", 2),
        ] {
            let (mut app, _commands) = test_app();
            let ctx = egui::Context::default();
            for name in ["first", "second", "third"].into_iter().take(pane_count) {
                app.split_panes.add_pane(ChannelId::new(name), None);
                app.split_panes.panes.last_mut().unwrap().input_buf = format!("{name} draft");
            }
            app.split_panes.focused = focused;
            let render = |app: &mut super::CrustApp, ctx: &egui::Context| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if app.split_panes.panes.is_empty() {
                        ui.add(egui::TextEdit::singleline(&mut app.chat_input_buf)
                            .id(super::chat_composer_id(None)));
                    } else {
                        for pane in &mut app.split_panes.panes {
                            ui.add(egui::TextEdit::singleline(&mut pane.input_buf)
                                .id(super::chat_composer_id(Some(pane.id))));
                        }
                    }
                });
            };
            let _ = ctx.run(Default::default(), |ctx| {
                app.sync_split_focus(ctx);
                render(&mut app, ctx);
                if let Some(index) = keep {
                    app.keep_split_pane(index, ctx);
                } else if let Some(index) = close {
                    app.close_split_pane(index, ctx);
                }
            });
            let input = egui::RawInput {
                events: vec![egui::Event::Text(" continued".to_owned())],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| render(&mut app, ctx));
            assert_eq!(app.state.active_channel, Some(ChannelId::new(expected)));
            assert_eq!(app.split_panes.panes.len(), remaining);
            if remaining == 0 {
                assert_eq!(app.chat_input_buf, format!("{expected} draft continued"));
                assert!(ctx.memory(|memory| memory.has_focus(super::chat_composer_id(None))));
            } else {
                for (index, pane) in app.split_panes.panes.iter().enumerate() {
                    let suffix = if index == app.split_panes.focused { " continued" } else { "" };
                    assert_eq!(pane.input_buf, format!("{} draft{suffix}", pane.channel.display_name()));
                }
                let pane = &app.split_panes.panes[app.split_panes.focused];
                assert!(ctx.memory(|memory| memory.has_focus(super::chat_composer_id(Some(pane.id)))));
            }
        }
    }

    #[test]
    fn virtual_split_close_buttons_preserve_the_other_chat_and_draft() {
        for (virtual_channel, zoom) in [ChannelId::live_feed(), ChannelId::mentions()]
            .into_iter()
            .flat_map(|channel| [0.75, 1.0, 1.25, 1.75].into_iter()
                .map(move |zoom| (channel.clone(), zoom)))
        {
            for virtual_index in 0..2 {
                let (mut app, _commands) = test_app();
                app.ui_font_size = zoom;
                let ctx = egui::Context::default();
                let room = ChannelId::new("room");
                app.state.join_channel(room.clone());
                app.split_panes.add_pane(room.clone(), None);
                app.split_panes.panes[0].input_buf = "kept draft".to_owned();
                app.split_panes.add_pane(virtual_channel.clone(), Some(virtual_index));
                app.split_panes.focused = virtual_index;
                app.state.active_channel = Some(virtual_channel.clone());
                let pane_id = app.split_panes.panes[virtual_index].id;
                let response_id = egui::Id::new("split_pane_bg").with(pane_id);
                let mut pane = None;
                for _ in 0..3 {
                    pane = production_ui_frame_with_response(
                        &ctx, &mut app, Vec::new(), Some(response_id)).1;
                }
                let pane = pane.expect("virtual pane is visible in the current pass").rect;
                let close = egui::pos2(
                    pane.right() - 6.0 - t::icon_btn_sm() / 2.0,
                    pane.top() + crate::widgets::split_header::split_header_height() / 2.0,
                );
                for pressed in [true, false] {
                    production_ui_frame(&ctx, &mut app, vec![
                        egui::Event::PointerMoved(close),
                        egui::Event::PointerButton {
                            pos: close,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ]);
                }
                assert!(app.split_panes.panes.is_empty(),
                    "virtual split close did not respond at zoom {zoom}; channel={virtual_channel:?}, index={virtual_index}, pane={pane:?}, close={close:?}, screen={:?}, pixels_per_point={}",
                    ctx.screen_rect(), ctx.pixels_per_point());
                assert_eq!(app.state.active_channel.as_ref(), Some(&room));
                assert_eq!(app.chat_input_buf, "kept draft");
                assert!(app.state.channels.contains_key(&room));
                production_ui_frame(&ctx, &mut app, vec![egui::Event::Text(" continued".to_owned())]);
                assert_eq!(app.chat_input_buf, "kept draft continued");
                assert!(!app.message_search.contains_key(&virtual_channel));
            }
        }
    }

    #[test]
    fn virtual_split_menus_can_close_other_panes_without_offering_search() {
        for (virtual_channel, zoom) in [ChannelId::live_feed(), ChannelId::mentions()]
            .into_iter()
            .flat_map(|channel| [0.75, 1.0, 1.25, 1.75].into_iter()
                .map(move |zoom| (channel.clone(), zoom)))
        {
            let (mut app, _commands) = test_app();
            app.ui_font_size = zoom;
            let ctx = egui::Context::default();
            let room = ChannelId::new("room");
            app.state.join_channel(room.clone());
            app.split_panes.add_pane(virtual_channel.clone(), None);
            app.split_panes.add_pane(room.clone(), None);
            app.state.active_channel = Some(room.clone());
            app.split_panes.focused = 1;
            app.split_panes.panes[0].input_buf = "retained pane draft".to_owned();
            let pane_id = app.split_panes.panes[0].id;
            let response_id = egui::Id::new("split_pane_bg").with(pane_id);
            let mut output = egui::FullOutput::default();
            let mut pane = None;
            for _ in 0..3 {
                (output, pane) = production_ui_frame_with_response(
                    &ctx, &mut app, Vec::new(), Some(response_id));
            }
            let pane = pane.expect("virtual pane is visible in the current pass").rect;
            let menu = output.shapes.iter().find_map(|shape| {
                let egui::epaint::Shape::Text(text) = &shape.shape else { return None };
                let rect = text.galley.rect.translate(text.pos.to_vec2());
                (text.galley.text() == "..." && pane.contains_rect(rect)).then_some(rect.center())
            }).unwrap_or_else(|| {
                let menus: Vec<_> = output.shapes.iter().filter_map(|shape| {
                    let egui::epaint::Shape::Text(text) = &shape.shape else { return None };
                    (text.galley.text() == "...")
                        .then_some(text.galley.rect.translate(text.pos.to_vec2()))
                }).collect();
                panic!("virtual split has no menu at zoom {zoom}; channel={virtual_channel:?}, pane={pane:?}, menus={menus:?}, screen={:?}, pixels_per_point={}",
                    ctx.screen_rect(), ctx.pixels_per_point())
            });
            for pressed in [true, false] {
                production_ui_frame(&ctx, &mut app, vec![
                    egui::Event::PointerMoved(menu),
                    egui::Event::PointerButton {
                        pos: menu,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            output = production_ui_frame(&ctx, &mut app, Vec::new());
            assert!(visible_text_rect(&output, "Show search").is_none());
            assert!(visible_text_rect(&output, "Hide search").is_none());
            let keep = visible_text_rect(&output, "Close other splits")
                .unwrap_or_else(|| {
                    let labels: Vec<_> = output.shapes.iter().filter_map(|shape| {
                        if let egui::epaint::Shape::Text(text) = &shape.shape {
                            Some(text.galley.text())
                        } else {
                            None
                        }
                    }).collect();
                    panic!("virtual split offers Close other splits at zoom {zoom}; pane={pane:?}, menu={menu:?}, rendered labels: {labels:?}")
                }).center();
            for pressed in [true, false] {
                production_ui_frame(&ctx, &mut app, vec![
                    egui::Event::PointerMoved(keep),
                    egui::Event::PointerButton {
                        pos: keep,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            assert!(app.split_panes.panes.is_empty(),
                "Close other splits did not respond at zoom {zoom}; channel={virtual_channel:?}, pane={pane:?}, menu={menu:?}, keep={keep:?}");
            assert_eq!(app.state.active_channel.as_ref(), Some(&virtual_channel));
            assert_eq!(app.chat_input_buf, "retained pane draft");
            assert!(app.state.channels.contains_key(&room));
            assert!(!app.message_search.contains_key(&virtual_channel));
        }
    }

    #[test]
    fn closing_a_redirected_channel_removes_all_its_panes_and_keeps_the_other_draft() {
        for focused in 0..3 {
            let (mut app, mut commands) = test_app();
            let ctx = egui::Context::default();
            let old = ChannelId::new("old");
            let redirected = ChannelId::new("redirected");
            let other = ChannelId::new("other");
            for channel in [&old, &redirected, &other] {
                app.state.join_channel(channel.clone());
                app.split_panes.add_pane(channel.clone(), None);
                app.split_panes.panes.last_mut().unwrap().input_buf = format!("{} draft", channel.display_name());
            }
            app.apply_event(crust_core::AppEvent::ChannelRedirected {
                old_channel: old, new_channel: redirected.clone(),
            }, &ctx);
            assert_eq!(app.split_panes.panes.iter().filter(|pane| pane.channel == redirected).count(), 2);
            app.split_panes.focused = focused;
            let _ = ctx.run(Default::default(), |ctx| {
                app.sync_split_focus(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    for pane in &mut app.split_panes.panes {
                        ui.add(egui::TextEdit::singleline(&mut pane.input_buf)
                            .id(super::chat_composer_id(Some(pane.id))));
                    }
                });
                app.close_channel_tab(redirected.clone(), ctx);
            });
            assert!(app.split_panes.panes.is_empty());
            assert_eq!(app.state.active_channel, Some(other.clone()));
            assert!(!app.state.channels.contains_key(&redirected));
            assert!(app.state.channels.contains_key(&other));
            assert_eq!(app.chat_input_buf, "other draft");
            let input = egui::RawInput {
                events: vec![egui::Event::Text(" continued".to_owned())],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut app.chat_input_buf)
                        .id(super::chat_composer_id(None)));
                });
            });
            assert_eq!(app.chat_input_buf, "other draft continued");
            assert!(ctx.memory(|memory| memory.has_focus(super::chat_composer_id(None))));
            let mut leave_count = 0;
            while let Ok(command) = commands.try_recv() {
                if matches!(command, AppCommand::LeaveChannel { channel } if channel == redirected) {
                    leave_count += 1;
                }
            }
            assert_eq!(leave_count, 1);
        }
    }

    #[test]
    fn shifted_moderation_shortcuts_keep_their_distinct_actions() {
        let ctx = egui::Context::default();
        let (mut app, mut commands) = moderation_test_app();
        activate_moderation_window(&ctx, &mut app);
        for (key, modifiers, expected, action) in [
            (egui::Key::A, egui::Modifiers::NONE, 1, "ALLOW"),
            (egui::Key::A, egui::Modifiers::SHIFT, 2, "ALLOW"),
            (egui::Key::D, egui::Modifiers::SHIFT, 2, "DENY"),
        ] {
            production_ui_frame(&ctx, &mut app, shortcut_input(key, modifiers).events);
            let resolved = moderation_commands(&mut commands);
            assert_eq!(resolved.len(), expected);
            assert!(resolved.iter().all(|(_, actual)| actual == action));
        }
        production_ui_frame(&ctx, &mut app,
            shortcut_input(egui::Key::Tab, egui::Modifiers::SHIFT).events);
        assert_eq!(app.mod_tools_active_tab, super::ModToolsTab::UnbanRequests,
            "focus {:?}, top {:?}", ctx.memory(|memory| memory.focused()), ctx.top_layer_id());
        production_ui_frame(&ctx, &mut app, Vec::new());
        assert!(ctx.memory(|memory| memory.focused().is_none()));
        production_ui_frame(&ctx, &mut app,
            shortcut_input(egui::Key::Tab, egui::Modifiers::NONE).events);
        assert_eq!(app.mod_tools_active_tab, super::ModToolsTab::AutoMod);
        production_ui_frame(&ctx, &mut app, Vec::new());
        let filter = ctx.read_response(egui::Id::new("moderation_filter")).unwrap().rect;
        click_production_ui(&ctx, &mut app, filter.center());
        production_ui_frame(&ctx, &mut app, vec![
            shortcut_input(egui::Key::A, egui::Modifiers::NONE).events.remove(0),
            egui::Event::Text("a".to_owned()),
        ]);
        assert_eq!(app.mod_tools_filter, "a");
        assert!(moderation_commands(&mut commands).is_empty());
        production_ui_frame(&ctx, &mut app,
            shortcut_input(egui::Key::Tab, egui::Modifiers::SHIFT).events);
        assert_eq!(app.mod_tools_active_tab, super::ModToolsTab::AutoMod,
            "Tab from a real editor keeps normal focus traversal");
    }

    fn moderation_test_app() -> (super::CrustApp, tokio::sync::mpsc::Receiver<AppCommand>) {
        let (mut app, commands) = test_app();
        let channel = ChannelId::new("channel");
        app.state.join_channel(channel.clone());
        app.state.channels.get_mut(&channel).unwrap().is_mod = true;
        app.mod_tools_open = true;
        app.automod_queue.insert(channel, (0..2).map(|index| super::AutoModQueueItem {
            message_id: format!("held-{index}"),
            sender_user_id: format!("user-{index}"),
            sender_login: format!("chatter{index}"),
            text: "Held message".to_owned(),
            reason: None,
        }).collect());
        (app, commands)
    }

    fn moderation_commands(commands: &mut tokio::sync::mpsc::Receiver<AppCommand>) -> Vec<(String, String)> {
        std::iter::from_fn(|| commands.try_recv().ok()).filter_map(|command| match command {
            AppCommand::ResolveAutoModMessage { message_id, action, .. } => Some((message_id, action)),
            _ => None,
        }).collect()
    }

    fn click_production_ui(ctx: &egui::Context, app: &mut super::CrustApp, pos: egui::Pos2) {
        for pressed in [true, false] {
            production_ui_frame(ctx, app, vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos, button: egui::PointerButton::Primary, pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
    }

    fn activate_moderation_window(ctx: &egui::Context, app: &mut super::CrustApp) {
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = production_ui_frame(ctx, app, Vec::new());
        }
        let channel_label = visible_text_rect(&output, "Channel: #channel")
            .unwrap_or_else(|| panic!("moderation channel must be visible (open {}, expanded {:?}, whispers {})",
                app.mod_tools_open, egui::collapsing_header::CollapsingState::load(
                    ctx, egui::Id::new("Moderation Tools").with("collapsing"),
                ).map(|state| state.is_open()), app.whispers_visible));
        click_production_ui(ctx, app, channel_label.center());
        assert!(ctx.memory(|memory| memory.focused().is_none()));
    }

    #[test]
    fn moderation_bulk_shortcuts_ignore_collapsed_covered_and_inactive_windows() {
        let ctx = egui::Context::default();
        let (mut app, mut commands) = moderation_test_app();
        activate_moderation_window(&ctx, &mut app);
        moderation_commands(&mut commands);

        let window_id = egui::Id::new("Moderation Tools");
        let rect = ctx.memory(|memory| memory.area_rect(window_id)).unwrap();
        let output = production_ui_frame(&ctx, &mut app, Vec::new());
        let title = visible_text_rect(&output, "Moderation Tools").unwrap();
        let collapse = egui::pos2(rect.left() + 13.0, title.center().y);
        click_production_ui(&ctx, &mut app, collapse);
        for _ in 0..3 {
            production_ui_frame(&ctx, &mut app, Vec::new());
        }
        assert!(!egui::collapsing_header::CollapsingState::load(
            &ctx, window_id.with("collapsing"),
        ).unwrap().is_open());
        for key in [egui::Key::A, egui::Key::D] {
            production_ui_frame(&ctx, &mut app, shortcut_input(key, egui::Modifiers::SHIFT).events);
            assert!(moderation_commands(&mut commands).is_empty(), "collapsed moderation must not act");
        }

        let output = production_ui_frame(&ctx, &mut app, Vec::new());
        let rect = ctx.memory(|memory| memory.area_rect(window_id)).unwrap();
        let title = visible_text_rect(&output, "Moderation Tools").unwrap();
        click_production_ui(&ctx, &mut app, egui::pos2(rect.left() + 13.0, title.center().y));
        activate_moderation_window(&ctx, &mut app);
        app.whispers_visible = true;
        for _ in 0..3 {
            production_ui_frame(&ctx, &mut app, Vec::new());
        }
        assert!(ctx.memory(|memory| memory.focused().is_none()));
        for key in [egui::Key::A, egui::Key::D] {
            production_ui_frame(&ctx, &mut app, shortcut_input(key, egui::Modifiers::SHIFT).events);
            assert!(moderation_commands(&mut commands).is_empty(), "a covered moderation window must not act");
        }

        app.whispers_visible = false;
        activate_moderation_window(&ctx, &mut app);
        let outside = egui::pos2(1090.0, 650.0);
        assert_eq!(ctx.layer_id_at(outside).unwrap().order, egui::Order::Background);
        click_production_ui(&ctx, &mut app, outside);
        production_ui_frame(&ctx, &mut app,
            shortcut_input(egui::Key::A, egui::Modifiers::SHIFT).events);
        assert!(moderation_commands(&mut commands).is_empty(), "clicking chat relinquishes moderation keys");

        activate_moderation_window(&ctx, &mut app);
        production_ui_frame(&ctx, &mut app,
            shortcut_input(egui::Key::A, egui::Modifiers::SHIFT).events);
        assert_eq!(moderation_commands(&mut commands).len(), 2,
            "activating the expanded window restores its shortcuts");
    }

    #[test]
    fn moderation_navigation_and_approval_stay_with_filtered_entries() {
        for (tab, expected) in [
            (super::ModToolsTab::AutoMod, "held-1"),
            (super::ModToolsTab::UnbanRequests, "request-1"),
            (super::ModToolsTab::LowTrust, "chatter1"),
        ] {
            let ctx = egui::Context::default();
            let (mut app, mut commands) = moderation_test_app();
            let channel = ChannelId::new("channel");
            app.unban_requests.insert(channel.clone(), (0..2).map(|index| super::UnbanRequestItem {
                request_id: format!("request-{index}"),
                user_id: format!("user-{index}"),
                user_login: format!("chatter{index}"),
                text: Some("Please review".to_owned()),
                created_at: None,
                status: None,
            }).collect());
            for index in 0..2 {
                app.state.channels.get_mut(&channel).unwrap().low_trust_users.insert(
                    format!("chatter{index}"), crust_core::model::LowTrustEntry {
                        status: crust_core::model::LowTrustStatus::Monitored,
                        user_id: format!("user-{index}"),
                        display_name: format!("Chatter{index}"),
                    },
                );
            }
            app.mod_tools_active_tab = tab;
            app.mod_tools_filter = " ChAtTeR1 ".to_owned();
            app.mod_tools_focused_index = 9;
            activate_moderation_window(&ctx, &mut app);
            moderation_commands(&mut commands);
            for navigation in [egui::Key::J, egui::Key::K, egui::Key::J, egui::Key::K] {
                production_ui_frame(&ctx, &mut app,
                    shortcut_input(navigation, egui::Modifiers::NONE).events);
                production_ui_frame(&ctx, &mut app,
                    shortcut_input(egui::Key::A, egui::Modifiers::NONE).events);
                let targets: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok())
                    .filter_map(|command| match command {
                        AppCommand::ResolveAutoModMessage { message_id, action, .. } if action == "ALLOW" => Some(message_id),
                        AppCommand::ResolveUnbanRequest { request_id, approve: true, .. } => Some(request_id),
                        AppCommand::ClearSuspiciousUser { login, .. } => Some(login),
                        _ => None,
                    }).collect();
                assert_eq!(targets, [expected], "{tab:?} navigation must keep a visible target");
                assert_eq!(app.mod_tools_focused_index, 0);
            }
        }
    }

    #[test]
    fn exact_shortcuts_preserve_platform_command_aliases() {
        for modifiers in [
            egui::Modifiers::CTRL | egui::Modifiers::COMMAND,
            egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND,
        ] {
            let ctx = egui::Context::default();
            let mut binding = crust_core::KeyBinding::new("F");
            binding.command = true;
            let _ = ctx.run(shortcut_input(egui::Key::F, modifiers), |ctx| {
                assert!(ctx.input_mut(|input| super::consume_binding(input, &binding)));
            });
            let _ = ctx.run(shortcut_input(egui::Key::F, modifiers | egui::Modifiers::SHIFT), |ctx| {
                assert!(!ctx.input_mut(|input| super::consume_binding(input, &binding)));
                binding.shift = true;
                assert!(ctx.input_mut(|input| super::consume_binding(input, &binding)));
            });
        }
    }

    #[test]
    fn search_jump_focuses_the_existing_split_and_preserves_drafts() {
        let (mut app, _commands) = test_app();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        for channel in [&first, &second] {
            app.state.join_channel(channel.clone());
            app.split_panes.add_pane(channel.clone(), None);
        }
        app.state.channels.get_mut(&second).unwrap().push_message(search_message(second.clone()));
        app.split_panes.panes[0].input_buf = "first draft".to_owned();
        app.split_panes.panes[1].input_buf = "second draft".to_owned();
        app.jump_to_message(second.clone(), crust_core::MessageId(1));
        assert_eq!(app.state.active_channel, Some(second.clone()));
        assert_eq!(app.split_panes.focused, 1);
        assert_eq!(app.split_panes.panes[0].channel, first);
        assert_eq!(app.split_panes.panes[0].input_buf, "first draft");
        assert_eq!(app.split_panes.panes[1].input_buf, "second draft");
        assert_eq!(app.pending_scroll_to_message.get(&second), Some(&crust_core::MessageId(1)));
    }

    #[test]
    fn deleted_message_events_refresh_an_open_search() {
        let (mut app, _commands) = test_app();
        let channel = ChannelId::new("first");
        app.state.join_channel(channel.clone());
        app.state.channels.get_mut(&channel).unwrap().push_message(search_message(channel.clone()));
        app.global_search.input = "is:deleted".to_owned();
        super::refresh_if_stale(&mut app.global_search, &app.state.channels);
        assert!(app.global_search.results.is_empty());
        app.apply_event(crust_core::events::AppEvent::MessageDeleted {
            channel,
            server_id: "search-server-id".to_owned(),
        }, &egui::Context::default());
        assert!(super::refresh_if_stale(&mut app.global_search, &app.state.channels));
        assert_eq!(app.global_search.results.len(), 1);
    }

    #[test]
    fn inactive_search_does_not_consume_escape_from_other_controls() {
        let (mut app, _commands) = test_app();
        app.state.join_channel(ChannelId::new("first"));
        let ctx = egui::Context::default();
        let _ = ctx.run(shortcut_input(egui::Key::Escape, egui::Modifiers::NONE), |ctx| {
            app.handle_search_shortcuts(ctx);
            assert!(ctx.input_mut(|input| super::consume_exact_key(input, egui::Modifiers::NONE, egui::Key::Escape)));
        });
    }

    #[test]
    fn nickname_updates_restore_chat_mentions_and_search_names() {
        let (mut app, _commands) = test_app();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        for channel in [&first, &second] {
            app.state.join_channel(channel.clone());
            app.state.channels.get_mut(channel).unwrap().push_message(search_message(channel.clone()));
        }
        app.state.mentions.push_back(search_message(first.clone()));
        let ctx = egui::Context::default();
        app.apply_event(crust_core::events::AppEvent::NicknamesUpdated {
            nicknames: vec![crust_core::model::Nickname::new("alice", "Custom name")],
        }, &ctx);
        assert_eq!(app.state.channels[&first].messages[0].sender.display_name, "Custom name");
        assert_eq!(app.state.mentions[0].sender.display_name, "Custom name");
        app.global_search.input = "from:\"Custom name\"".to_owned();
        super::refresh_if_stale(&mut app.global_search, &app.state.channels);
        assert_eq!(app.global_search.results.len(), 2);

        let mut scoped = crust_core::model::Nickname::new("alice", "Scoped name");
        scoped.channel = Some("first".to_owned());
        app.apply_event(crust_core::events::AppEvent::NicknamesUpdated { nicknames: vec![scoped] }, &ctx);
        assert_eq!(app.state.channels[&first].messages[0].sender.display_name, "Scoped name");
        assert_eq!(app.state.channels[&second].messages[0].sender.display_name, "Alice");
        app.apply_event(crust_core::events::AppEvent::NicknamesUpdated { nicknames: Vec::new() }, &ctx);
        for channel in [&first, &second] {
            assert_eq!(app.state.channels[channel].messages[0].sender.display_name, "Alice");
            assert!(app.state.channels[channel].messages[0].sender.original_display_name.is_none());
        }
        assert_eq!(app.state.mentions[0].sender.display_name, "Alice");
        assert!(super::refresh_if_stale(&mut app.global_search, &app.state.channels));
        assert!(app.global_search.results.is_empty());
    }

    #[test]
    fn loaded_history_restores_stored_aliases_when_the_rule_was_removed() {
        let (mut app, _commands) = test_app();
        let channel = ChannelId::new("first");
        app.state.join_channel(channel.clone());
        let mut message = search_message(channel.clone());
        crust_core::model::apply_sender_nickname(
            &[crust_core::model::Nickname::new("alice", "Previous alias")],
            channel.display_name(),
            &mut message.sender,
        );
        message.flags.is_mention = true;
        let ctx = egui::Context::default();
        app.apply_event(crust_core::events::AppEvent::HistoryLoaded {
            channel: channel.clone(),
            messages: vec![message.clone()],
            local: None,
        }, &ctx);
        app.apply_event(crust_core::events::AppEvent::MentionsLoaded { messages: vec![message] }, &ctx);
        assert_eq!(app.state.channels[&channel].messages[0].sender.display_name, "Alice");
        assert_eq!(app.state.mentions[0].sender.display_name, "Alice");
    }

    #[test]
    fn local_history_keeps_its_row_cursor_and_ignores_repeated_page_results() {
        use crust_core::model::{HistoryCursor, LocalHistoryPageInfo};
        let (mut app, mut commands) = test_app();
        let channel = ChannelId::new("first");
        app.state.join_channel(channel.clone());
        let ctx = egui::Context::default();
        let first = HistoryCursor { timestamp_ms: 1000, row_id: 20 };
        let next = HistoryCursor { timestamp_ms: 1000, row_id: 10 };
        app.apply_event(crust_core::events::AppEvent::HistoryLoaded {
            channel: channel.clone(), messages: vec![],
            local: Some(LocalHistoryPageInfo { before: None, next: Some(first), has_more: true, identities: Vec::new() }),
        }, &ctx);
        app.request_older_local_history(&channel, 2000);
        assert!(matches!(commands.try_recv().unwrap(), AppCommand::LoadOlderLocalHistory {
            before_ts_ms: 1000, before_row_id: Some(20), ..
        }));
        let mut message = search_message(channel.clone());
        message.server_id = None;
        let page = crust_core::events::AppEvent::HistoryLoaded {
            channel: channel.clone(), messages: vec![message],
            local: Some(LocalHistoryPageInfo { before: Some(first), next: Some(next), has_more: false, identities: Vec::new() }),
        };
        app.apply_event(page.clone(), &ctx);
        app.apply_event(page, &ctx);
        let state = &app.state.channels[&channel];
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.local_history_cursor, Some(next));
        assert!(state.local_history_exhausted);
        app.request_older_local_history(&channel, 1000);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn newly_matched_live_highlights_are_persisted_with_the_original_message_identity() {
        let (mut app, mut commands) = test_app();
        let channel = ChannelId::new("first");
        app.state.join_channel(channel.clone());
        let message = search_message(channel.clone());
        let ctx = egui::Context::default();
        app.apply_event(crust_core::events::AppEvent::HighlightRulesUpdated {
            rules: vec![crust_core::highlight::HighlightRule::new(message.raw_text.clone())],
        }, &ctx);
        app.apply_event(crust_core::events::AppEvent::MessageReceived {
            channel: channel.clone(), message: message.clone(),
        }, &ctx);
        match commands.try_recv().unwrap() {
            AppCommand::PersistMessageHighlight { channel: actual, server_id, message_id, timestamp_ms } => {
                assert_eq!(actual, channel);
                assert_eq!(server_id, message.server_id);
                assert_eq!(message_id, message.id);
                assert_eq!(timestamp_ms, message.timestamp.timestamp_millis());
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(app.state.channels[&channel].messages[0].flags.is_highlighted);
        let mut history = message;
        history.id = crust_core::MessageId(999);
        history.server_id = Some("older-highlight".to_owned());
        history.flags.is_history = true;
        app.apply_event(crust_core::events::AppEvent::MessageReceived { channel, message: history }, &ctx);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn redirecting_splits_preserves_both_drafts_and_focused_pane() {
        let old = ChannelId::irc("irc.example", 6697, true, "old");
        let new = ChannelId::irc("irc.example", 6697, true, "new");
        let other = ChannelId::new("other");
        let mut splits = SplitPanes::default();
        splits.add_pane(old.clone(), None);
        splits.add_pane(other.clone(), None);
        splits.panes[0].input_buf = "source draft".to_owned();
        splits.focused = 0;
        splits.redirect_channel(&old, &new);
        assert_eq!(splits.panes[0].channel, new);
        assert_eq!(splits.panes[0].input_buf, "source draft");
        assert_eq!(splits.panes[1].channel, other);
        assert_eq!(splits.focused, 0);

        splits.panes[0].channel = old.clone();
        splits.panes[1].channel = new.clone();
        splits.panes[1].input_buf = "destination draft".to_owned();
        splits.redirect_channel(&old, &new);
        assert_eq!(splits.panes.len(), 2);
        assert!(splits.panes.iter().all(|pane| pane.channel == new));
        assert_eq!(splits.panes[0].input_buf, "source draft");
        assert_eq!(splits.panes[1].input_buf, "destination draft");
        assert_eq!(splits.focused, 0);
        splits.redirect_channel(&new, &new);
        assert_eq!(splits.panes[1].input_buf, "destination draft");
    }

    #[test]
    fn reactivating_visible_split_preserves_panes_and_drafts() {
        let alice = ChannelId::new("alice");
        let bob = ChannelId::new("bob");
        let mut splits = SplitPanes::default();
        splits.add_pane(alice.clone(), None);
        splits.add_pane(bob.clone(), None);
        splits.panes[0].input_buf = "draft for alice".to_owned();
        splits.panes[1].input_buf = "draft for bob".to_owned();

        splits.activate_channel(&alice);
        assert_eq!(splits.focused, 0);
        assert_eq!(splits.panes[0].input_buf, "draft for alice");
        splits.activate_channel(&bob);
        assert_eq!(splits.focused, 1);
        assert_eq!(splits.panes.len(), 2);
        assert_eq!(splits.panes[0].channel, alice);
        assert_eq!(splits.panes[1].channel, bob);
        assert_eq!(splits.panes[0].input_buf, "draft for alice");
        assert_eq!(splits.panes[1].input_buf, "draft for bob");
    }

    #[test]
    fn event_toasts_suppress_history_mirrors_and_background_channels() {
        use crust_core::model::{ChatMessage, MessageFlags, MessageId, MsgKind, Sender, UserId};

        let mut message = ChatMessage {
            id: MessageId(1),
            server_id: None,
            timestamp: chrono::Utc::now(),
            channel: ChannelId::new("example"),
            sender: Sender {
                user_id: UserId("1".to_owned()),
                login: "sender".to_owned(),
                display_name: "Sender".to_owned(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: String::new(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Bits { amount: 100 },
            shared: None,
        };
        let live = EventToast::from_message(true, &message).expect("live cheer toast");
        assert!(live.text.contains("Sender cheered 100 bits"));
        assert!(!live.confetti);
        assert!(EventToast::from_message(false, &message).is_none());
        message.flags.is_history = true;
        assert!(EventToast::from_message(true, &message).is_none());
        message.flags.is_history = false;
        message.flags.suppress_notification = true;
        assert!(EventToast::from_message(true, &message).is_none());
        message.flags.suppress_notification = false;
        message.msg_kind = MsgKind::Bits { amount: 99 };
        assert!(EventToast::from_message(true, &message).is_none());
    }

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
    fn responsive_layout_controls_match_visible_selection_and_click_actions() {
        fn frame(ctx: &egui::Context, app: &mut super::CrustApp, narrow: bool,
            events: Vec<egui::Event>) -> (egui::Response, egui::Response) {
            let mut controls = None;
            let _ = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO,
                    egui::vec2(if narrow { 480.0 } else { 1000.0 }, 200.0))),
                events, ..Default::default()
            }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        controls = Some(app.show_channel_layout_controls(ui, narrow, false));
                    });
                });
            });
            controls.unwrap()
        }
        fn click(ctx: &egui::Context, app: &mut super::CrustApp, narrow: bool, pos: egui::Pos2) {
            for pressed in [true, false] {
                let _ = frame(ctx, app, narrow, vec![egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary,
                        pressed, modifiers: egui::Modifiers::NONE }]);
            }
        }
        let (mut app, mut commands) = test_app();
        let ctx = egui::Context::default();
        app.channel_layout = super::ChannelLayout::Sidebar;
        app.sidebar_visible = true;
        let state = super::ChannelLayoutControls::new(app.channel_layout, app.sidebar_visible, true);
        assert!(!state.sidebar_selected);
        assert!(state.tabs_selected);
        let (sidebar, tabs) = frame(&ctx, &mut app, true, Vec::new());
        assert!(!sidebar.enabled());
        click(&ctx, &mut app, true, sidebar.rect.center());
        assert_eq!(app.channel_layout, super::ChannelLayout::Sidebar);
        assert!(app.sidebar_visible);
        click(&ctx, &mut app, true, tabs.rect.center());
        assert_eq!(app.channel_layout, super::ChannelLayout::TopTabs);
        assert!(std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, AppCommand::SetAppearanceSettings { channel_layout, .. }
                if channel_layout == "top_tabs")));

        let (sidebar, _) = frame(&ctx, &mut app, false, Vec::new());
        assert!(sidebar.enabled());
        click(&ctx, &mut app, false, sidebar.rect.center());
        assert_eq!(app.channel_layout, super::ChannelLayout::Sidebar);
        assert!(app.sidebar_visible);
        let state = super::ChannelLayoutControls::new(app.channel_layout, app.sidebar_visible, false);
        assert!(state.sidebar_selected);
        assert!(!state.tabs_selected);
        click(&ctx, &mut app, false, sidebar.rect.center());
        assert!(!app.sidebar_visible);
        let (_, tabs) = frame(&ctx, &mut app, false, Vec::new());
        click(&ctx, &mut app, false, tabs.rect.center());
        assert_eq!(app.channel_layout, super::ChannelLayout::TopTabs);
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
    fn slash_poll_long_flags_map_to_create_poll() {
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
