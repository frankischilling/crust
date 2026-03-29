use std::collections::HashMap;
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use image::DynamicImage;
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{AppCommand, AppEvent, ConnectionState, LinkPreview},
    model::{
        ChannelId, ChannelState, EmoteCatalogEntry, MsgKind, ReplyInfo, IRC_SERVER_CONTROL_CHANNEL,
    },
    AppState,
};

use crate::commands::render_help_message;
use crate::perf::{ChatPerfStats, PerfOverlay};
use crate::theme as t;
use crate::widgets::{
    analytics::AnalyticsPanel,
    bytes_uri,
    channel_list::ChannelList,
    chat_input::ChatInput,
    emote_picker::EmotePicker,
    irc_status::IrcStatusPanel,
    join_dialog::JoinDialog,
    loading_screen::{LoadEvent, LoadingScreen},
    login_dialog::{LoginAction, LoginDialog},
    message_list::MessageList,
    message_search::{
        should_use_search_window, show_message_search_inline, show_message_search_window,
        MessageSearchState,
    },
    user_profile_popup::{PopupAction, UserProfilePopup},
};

// Channel layout mode

const REPAINT_ANIM_MS: u64 = 33;
const REPAINT_HOUSEKEEPING_MS: u64 = 2_000;
const STREAM_REFRESH_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Stream status snapshot for one channel, populated via FetchUserProfile.
#[derive(Clone)]
struct StreamStatusInfo {
    is_live: bool,
    title: Option<String>,
    game: Option<String>,
    viewers: Option<u64>,
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

    /// Returns true if channel already has a pane open.
    fn contains_channel(&self, ch: &ChannelId) -> bool {
        self.panes.iter().any(|p| &p.channel == ch)
    }
}

/// Controls where the channel list is rendered.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLayout {
    /// Classic left sidebar (default).
    #[default]
    Sidebar,
    /// Compact horizontal tab strip pinned to the top of the window.
    TopTabs,
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
    /// Controls whether the left channel sidebar is visible (Sidebar mode only).
    sidebar_visible: bool,
    /// Where channel tabs are rendered: left sidebar or top strip.
    channel_layout: ChannelLayout,
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
    /// Last time we scanned channels to schedule stale stream-status refreshes.
    last_stream_refresh_scan: std::time::Instant,
    /// Cached live-status map derived from `stream_statuses`; rebuilt only on
    /// change rather than every frame.
    live_map_cache: HashMap<String, bool>,
    /// Short-lived pop-in banners for Sub / Raid / Bits events (cap 5).
    event_toasts: Vec<EventToast>,
    /// Settings dialog visibility.
    settings_open: bool,
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
            sidebar_visible: true,
            channel_layout: ChannelLayout::default(),
            analytics_panel: AnalyticsPanel::default(),
            analytics_visible: false,
            irc_status_panel: IrcStatusPanel::default(),
            irc_status_visible: false,
            loading_screen: LoadingScreen::default(),
            stream_statuses: HashMap::new(),
            stream_status_fetched: HashMap::new(),
            last_stream_refresh_scan: std::time::Instant::now(),
            live_map_cache: HashMap::new(),
            event_toasts: Vec::new(),
            settings_open: false,
            kick_beta_enabled: false,
            irc_beta_enabled: false,
            irc_nickserv_user: String::new(),
            irc_nickserv_pass: String::new(),
            always_on_top: false,
            prevent_overlong_twitch_messages: true,
            collapse_long_messages: true,
            collapse_long_message_lines: 8,
            animations_when_focused: true,
            stv_avatars: HashMap::new(),
            static_avatar_frames: HashMap::new(),
            split_panes: SplitPanes::default(),
            message_search: HashMap::new(),
            sorted_chatters: HashMap::new(),
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
                    let login = channel.display_name().to_owned();
                    if is_valid_twitch_login(&login) {
                        self.stream_status_fetched
                            .insert(login.clone(), std::time::Instant::now());
                        self.send_cmd(AppCommand::FetchUserProfile { login });
                    }
                }
            }
            AppEvent::ChannelParted { channel } => {
                self.state.leave_channel(&channel);
                self.sorted_chatters.remove(&channel);
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
            AppEvent::MessageReceived { channel, message } => {
                if channel.is_irc() && !self.state.channels.contains_key(&channel) {
                    // IRC can deliver messages on targets we haven't opened yet
                    // (e.g. direct messages or status-targeted channel forms).
                    // Create the tab first so inbound messages are never dropped.
                    self.state.join_channel(channel.clone());
                    self.sorted_chatters.entry(channel.clone()).or_default();
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

                // Generate a short-lived event toast for high-visibility events.
                if self.event_toasts.len() < 5 {
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
                        if self.event_toasts.len() >= 5 {
                            self.event_toasts.remove(0);
                        }
                        self.event_toasts.push(toast);
                    }
                }

                let mut rebuilt_chatters: Option<Vec<String>> = None;
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
                            {
                                ch.unread_mentions += 1;
                            }
                        }
                        ch.push_message(message);
                    }
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
                // Clear the previous account's avatar so it doesn't flash
                // while the new one is fetched.
                self.state.auth.avatar_url = None;
                self.state.auth.logged_in = true;
                self.state.auth.username = Some(username);
                self.state.auth.user_id = Some(user_id);
            }
            AppEvent::LoggedOut => {
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
                self.stream_status_fetched
                    .insert(login, std::time::Instant::now());
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
            AppEvent::UserProfileUnavailable { login } => {
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
                            let exists = msg.sender.badges.iter().any(|x| {
                                x.url.as_deref() == b.url.as_deref()
                                    || x.name.eq_ignore_ascii_case("7tv")
                            });
                            if !exists {
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
}

// eframe::App implementation

impl eframe::App for CrustApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
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

        // Periodic stream-status refresh: re-fetch every 60 s per channel.
        // Throttle the stale-scan itself to avoid per-frame channel iteration.
        const STREAM_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);
        if self.last_stream_refresh_scan.elapsed() >= STREAM_REFRESH_SCAN_INTERVAL {
            self.last_stream_refresh_scan = std::time::Instant::now();
            let mut stale: Vec<String> = Vec::new();
            for ch in &self.state.channel_order {
                if !ch.is_twitch() {
                    continue;
                }
                let login = ch.display_name();
                if !is_valid_twitch_login(&login) {
                    continue;
                }
                let is_stale = self
                    .stream_status_fetched
                    .get(login)
                    .map(|t| t.elapsed() >= STREAM_REFRESH)
                    .unwrap_or(true);
                if is_stale {
                    stale.push(login.to_owned());
                }
            }
            for login in stale {
                self.stream_status_fetched
                    .insert(login.clone(), std::time::Instant::now());
                self.send_cmd(AppCommand::FetchUserProfile { login });
            }
        }

        // Render profile popup and dispatch any actions.
        for action in self
            .user_profile_popup
            .show(ctx, &self.emote_bytes, &self.stv_avatars)
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
            let mut kick = self.kick_beta_enabled;
            let mut irc = self.irc_beta_enabled;
            let mut ns_user = self.irc_nickserv_user.clone();
            let mut ns_pass = self.irc_nickserv_pass.clone();
            let mut on_top = self.always_on_top;
            let mut overflow_prevent = self.prevent_overlong_twitch_messages;
            let mut collapse_long_messages = self.collapse_long_messages;
            let mut collapse_long_message_lines = self.collapse_long_message_lines;
            let mut animations_when_focused = self.animations_when_focused;
            let mut light = t::is_light();

            let settings_default_pos = egui::pos2(
                (ctx.screen_rect().center().x - 200.0).max(8.0),
                (ctx.screen_rect().center().y - 180.0).max(8.0),
            );
            egui::Window::new("Settings")
                .open(&mut settings_open)
                .collapsible(false)
                .resizable(false)
                .default_pos(settings_default_pos)
                .show(ctx, |ui| {
                    let screen_w = ctx.screen_rect().width();
                    let settings_w = (screen_w - 64.0)
                        .clamp(140.0, 400.0)
                        .min((screen_w - 16.0).max(100.0));
                    ui.set_min_width(settings_w);
                    ui.set_max_width(settings_w);

                    ui.label(
                        RichText::new("Appearance")
                            .font(t::body())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Theme:");
                        ui.selectable_value(&mut light, false, "Dark");
                        ui.selectable_value(&mut light, true, "Light");
                    });
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label(
                        RichText::new("Window")
                            .font(t::body())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.add_space(4.0);
                    ui.checkbox(&mut on_top, "Always on top");
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label(
                        RichText::new("Experimental Transport Compatibility")
                            .font(t::body())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.add_space(6.0);

                    ui.checkbox(&mut kick, "Kick compatibility (beta)");
                    ui.checkbox(&mut irc, "IRC chat compatibility (beta)");

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label(
                        RichText::new("IRC NickServ Auto-Identify")
                            .font(t::body())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Automatically identify with NickServ when connecting to IRC servers.",
                        )
                        .font(t::small())
                        .color(t::text_muted()),
                    );
                    ui.add_space(6.0);

                    ui.horizontal(|ui| {
                        ui.label("Username:");
                        ui.text_edit_singleline(&mut ns_user);
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut ns_pass).password(true));
                    });

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Changes are saved immediately. Enabling beta transports may require restarting Crust.",
                        )
                        .font(t::small())
                        .color(t::text_muted()),
                    );

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.label(
                        RichText::new("Chat Input + Rendering")
                            .font(t::body())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.add_space(6.0);

                    ui.label(
                        RichText::new("Twitch message overflow")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.horizontal(|ui| {
                        ui.radio_value(
                            &mut overflow_prevent,
                            false,
                            "Highlight (allow typing over 500 chars)",
                        );
                        ui.radio_value(&mut overflow_prevent, true, "Prevent (hard cap at 500)");
                    });
                    ui.add_space(6.0);

                    ui.checkbox(&mut collapse_long_messages, "Collapse long chat messages");
                    ui.add_enabled_ui(collapse_long_messages, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Collapse after");
                            ui.add(
                                egui::Slider::new(&mut collapse_long_message_lines, 2..=24)
                                    .text("lines"),
                            );
                        });
                    });
                    ui.add_space(6.0);
                    ui.checkbox(
                        &mut animations_when_focused,
                        "Animate only while window is focused",
                    );
                });

            self.settings_open = settings_open;
            if light != t::is_light() {
                if light {
                    t::set_light();
                } else {
                    t::set_dark();
                }
                apply_theme_visuals(ctx);
                let theme = if light { "light" } else { "dark" };
                self.send_cmd(AppCommand::SetTheme {
                    theme: theme.to_owned(),
                });
            }
            if on_top != self.always_on_top {
                self.always_on_top = on_top;
                let level = if on_top {
                    egui::viewport::WindowLevel::AlwaysOnTop
                } else {
                    egui::viewport::WindowLevel::Normal
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                self.send_cmd(AppCommand::SetAlwaysOnTop { enabled: on_top });
            }
            if kick != self.kick_beta_enabled || irc != self.irc_beta_enabled {
                self.kick_beta_enabled = kick;
                self.irc_beta_enabled = irc;
                self.send_cmd(AppCommand::SetBetaFeatures {
                    kick_enabled: kick,
                    irc_enabled: irc,
                });
            }
            if ns_user != self.irc_nickserv_user || ns_pass != self.irc_nickserv_pass {
                self.irc_nickserv_user = ns_user.clone();
                self.irc_nickserv_pass = ns_pass.clone();
                self.send_cmd(AppCommand::SetIrcAuth {
                    nickserv_user: ns_user,
                    nickserv_pass: ns_pass,
                });
            }
            if overflow_prevent != self.prevent_overlong_twitch_messages
                || collapse_long_messages != self.collapse_long_messages
                || collapse_long_message_lines != self.collapse_long_message_lines
                || animations_when_focused != self.animations_when_focused
            {
                self.prevent_overlong_twitch_messages = overflow_prevent;
                self.collapse_long_messages = collapse_long_messages;
                self.collapse_long_message_lines = collapse_long_message_lines.max(1);
                self.animations_when_focused = animations_when_focused;
                self.send_cmd(AppCommand::SetChatUiBehavior {
                    prevent_overlong_twitch_messages: self.prevent_overlong_twitch_messages,
                    collapse_long_messages: self.collapse_long_messages,
                    collapse_long_message_lines: self.collapse_long_message_lines,
                    animations_when_focused: self.animations_when_focused,
                });
            }
        }

        // -- Top bar -----------------------------------------------------------
        // Auto-collapse sidebar into top tabs when window is very narrow so
        // the chat area always has usable space for a super-thin layout.
        let window_width = ctx.screen_rect().width();
        const NARROW_THRESHOLD: f32 = 400.0;
        const VERY_NARROW_THRESHOLD: f32 = 260.0;
        if window_width < NARROW_THRESHOLD {
            if self.channel_layout == ChannelLayout::Sidebar && self.sidebar_visible {
                self.channel_layout = ChannelLayout::TopTabs;
            }
        }

        TopBottomPanel::top("status_bar")
            .exact_height(if window_width < VERY_NARROW_THRESHOLD {
                28.0
            } else {
                36.0
            })
            .frame(
                Frame::new()
                    .fill(t::bg_surface())
                    .inner_margin(t::BAR_MARGIN)
                    .stroke(egui::Stroke::new(1.0, t::border_subtle())),
            )
            .show(ctx, |ui| {
                let bar_width = ui.available_width();
                const COMPACT_CONTROLS_THRESHOLD: f32 = 700.0;
                const COMPACT_ACCOUNT_THRESHOLD: f32 = 700.0;
                const SHOW_LOGO_THRESHOLD: f32 = 250.0;
                const SHOW_CONN_TEXT_THRESHOLD: f32 = 620.0;
                const SHOW_JOIN_TEXT_THRESHOLD: f32 = 420.0;

                let compact_controls = bar_width < COMPACT_CONTROLS_THRESHOLD;
                let compact_account = bar_width < COMPACT_ACCOUNT_THRESHOLD;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                    // App logotype
                    if bar_width > SHOW_LOGO_THRESHOLD {
                        let logo_font = egui::FontId::proportional(15.0);
                        let logo_w = ui.fonts(|f| {
                            f.layout_no_wrap("crust".to_owned(), logo_font.clone(), t::accent())
                                .rect
                                .width()
                        }) + 4.0;
                        ui.allocate_ui_with_layout(
                            egui::vec2(logo_w, t::BAR_H),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new("crust")
                                        .font(logo_font)
                                        .strong()
                                        .color(t::accent()),
                                );
                            },
                        );
                        ui.separator();
                    }

                    // Connection indicator
                    let dot_r = 4.5_f32;
                    let (dot_rect, _) = ui.allocate_exact_size(
                        egui::vec2(dot_r * 2.0 + 4.0, dot_r * 2.0),
                        egui::Sense::hover(),
                    );
                    let (dot_color, conn_label) =
                        connection_indicator(&self.state.connection, self.state.auth.logged_in);
                    ui.painter()
                        .circle_filled(dot_rect.center(), dot_r, dot_color);
                    // Hide connection label text at narrow widths; dot is sufficient
                    if bar_width > SHOW_CONN_TEXT_THRESHOLD {
                        let conn_font = t::small();
                        let conn_w = ui.fonts(|f| {
                            f.layout_no_wrap(
                                conn_label.to_owned(),
                                conn_font.clone(),
                                t::text_secondary(),
                            )
                            .rect
                            .width()
                        }) + 4.0;
                        ui.allocate_ui_with_layout(
                            egui::vec2(conn_w, t::BAR_H),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(conn_label)
                                        .font(conn_font)
                                        .color(t::text_secondary()),
                                );
                            },
                        );
                    }

                    ui.separator();

                    // Join channel button
                    let join_label = if bar_width < SHOW_JOIN_TEXT_THRESHOLD {
                        "+"
                    } else {
                        "+ Join"
                    };
                    let join_w = if bar_width < SHOW_JOIN_TEXT_THRESHOLD {
                        28.0
                    } else {
                        72.0
                    };
                    if ui
                        .add_sized(
                            [join_w, t::BAR_H],
                            egui::Button::new(RichText::new(join_label).font(t::small())),
                        )
                        .on_hover_text("Join a Twitch channel")
                        .clicked()
                    {
                        self.join_dialog.toggle();
                    }

                    ui.separator();

                    // Sidebar / layout toggles - hidden at very narrow widths
                    // In compact mode these actions move to the overflow menu.
                    if !compact_controls && bar_width > 520.0 {
                        // Sidebar visibility toggle
                        let sidebar_open =
                            self.channel_layout == ChannelLayout::Sidebar && self.sidebar_visible;
                        let vis_icon = if sidebar_open { "[|" } else { "|]" };
                        let vis_tip = if sidebar_open {
                            "Hide channel sidebar"
                        } else {
                            "Show channel sidebar"
                        };
                        if ui
                            .add_sized(
                                [26.0, t::BAR_H],
                                egui::Button::new(RichText::new(vis_icon).font(t::small())),
                            )
                            .on_hover_text(vis_tip)
                            .clicked()
                        {
                            match self.channel_layout {
                                ChannelLayout::TopTabs => {
                                    // Return to sidebar
                                    self.channel_layout = ChannelLayout::Sidebar;
                                    self.sidebar_visible = true;
                                }
                                ChannelLayout::Sidebar => {
                                    self.sidebar_visible = !self.sidebar_visible;
                                }
                            }
                        }

                        // Layout mode toggle (Sidebar <-> Top tabs)
                        let mode_icon = if self.channel_layout == ChannelLayout::Sidebar {
                            "Tabs"
                        } else {
                            "Side"
                        };
                        let mode_tip = if self.channel_layout == ChannelLayout::Sidebar {
                            "Move channels to top bar"
                        } else {
                            "Move channels to sidebar"
                        };
                        if ui
                            .add_sized(
                                [38.0, t::BAR_H],
                                egui::Button::new(RichText::new(mode_icon).font(t::small())),
                            )
                            .on_hover_text(mode_tip)
                            .clicked()
                        {
                            if self.channel_layout == ChannelLayout::Sidebar {
                                self.channel_layout = ChannelLayout::TopTabs;
                            } else {
                                self.channel_layout = ChannelLayout::Sidebar;
                                self.sidebar_visible = true;
                            }
                        }
                    } // end bar_width > 350 guard

                    // Right-side items
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                        if compact_controls {
                            ui.menu_button(RichText::new("⋯").font(t::small()), |ui| {
                                if ui
                                    .button(RichText::new("Settings").font(t::small()))
                                    .clicked()
                                {
                                    self.settings_open = true;
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

                                let perf_label = if self.perf.visible {
                                    "Perf: on"
                                } else {
                                    "Perf: off"
                                };
                                if ui
                                    .button(RichText::new(perf_label).font(t::small()))
                                    .clicked()
                                {
                                    self.perf.visible = !self.perf.visible;
                                    ui.close_menu();
                                }

                                let stats_label = if self.analytics_visible {
                                    "Stats: on"
                                } else {
                                    "Stats: off"
                                };
                                if ui
                                    .button(RichText::new(stats_label).font(t::small()))
                                    .clicked()
                                {
                                    self.analytics_visible = !self.analytics_visible;
                                    ui.close_menu();
                                }

                                let irc_label = if self.irc_status_visible {
                                    "IRC: on"
                                } else {
                                    "IRC: off"
                                };
                                if ui
                                    .button(RichText::new(irc_label).font(t::small()))
                                    .clicked()
                                {
                                    self.irc_status_visible = !self.irc_status_visible;
                                    ui.close_menu();
                                }
                            });
                            ui.separator();
                        }

                        // Settings button stays visible only outside compact mode
                        // (in compact mode it's available in the overflow menu).
                        if !compact_controls {
                            let settings_label = if bar_width < 860.0 { "⚙" } else { "Settings" };
                            let settings_w = if bar_width < 860.0 { 28.0 } else { 72.0 };
                            if ui
                                .add_sized(
                                    [settings_w, t::BAR_H],
                                    egui::Button::new(
                                        RichText::new(settings_label).font(t::small()),
                                    ),
                                )
                                .on_hover_text("Open application settings")
                                .clicked()
                            {
                                self.settings_open = true;
                            }

                            ui.separator();
                        }

                        // Perf overlay toggle - hide at narrow widths
                        if !compact_controls && bar_width > 930.0 {
                            let perf_label = if self.perf.visible {
                                "Perf: on"
                            } else {
                                "Perf: off"
                            };
                            if ui
                                .add_sized(
                                    [66.0, t::BAR_H],
                                    egui::Button::new(RichText::new(perf_label).font(t::small())),
                                )
                                .on_hover_text("Toggle performance overlay")
                                .clicked()
                            {
                                self.perf.visible = !self.perf.visible;
                            }

                            ui.separator();

                            // Analytics panel toggle
                            let stats_label = if self.analytics_visible {
                                "Stats: on"
                            } else {
                                "Stats: off"
                            };
                            if ui
                                .add_sized(
                                    [66.0, t::BAR_H],
                                    egui::Button::new(RichText::new(stats_label).font(t::small())),
                                )
                                .on_hover_text("Toggle chatter analytics")
                                .clicked()
                            {
                                self.analytics_visible = !self.analytics_visible;
                            }

                            ui.separator();
                        }

                        // IRC status toggle - hide at narrow widths
                        if !compact_controls && bar_width > 760.0 {
                            let irc_status_label = if self.irc_status_visible {
                                "IRC: on"
                            } else {
                                "IRC: off"
                            };
                            if ui
                                .add_sized(
                                    [62.0, t::BAR_H],
                                    egui::Button::new(
                                        RichText::new(irc_status_label).font(t::small()),
                                    ),
                                )
                                .on_hover_text("Toggle IRC status window")
                                .clicked()
                            {
                                self.irc_status_visible = !self.irc_status_visible;
                            }

                            ui.separator();
                        }

                        // Emote count - hide at narrow widths
                        if !compact_controls && bar_width > 900.0 {
                            ui.label(
                                RichText::new(format!("{} emotes", self.emote_bytes.len()))
                                    .font(t::small())
                                    .color(t::text_muted()),
                            );

                            ui.separator();
                        }

                        // Login / Account button
                        if self.state.auth.logged_in {
                            let name = self
                                .state
                                .auth
                                .username
                                .as_deref()
                                .unwrap_or("User")
                                .to_owned();
                            let initial = name
                                .chars()
                                .next()
                                .unwrap_or('?')
                                .to_uppercase()
                                .next()
                                .unwrap_or('?');

                            if compact_account {
                                if ui
                                    .add_sized(
                                        [t::BAR_H, t::BAR_H],
                                        egui::Button::new(
                                            RichText::new(initial.to_string())
                                                .font(t::small())
                                                .strong(),
                                        ),
                                    )
                                    .on_hover_text("Account")
                                    .clicked()
                                {
                                    self.login_dialog.toggle();
                                }
                            } else {
                                let btn_h = t::BAR_H;
                                let name_galley = ui.painter().layout_no_wrap(
                                    name.clone(),
                                    t::small(),
                                    t::text_primary(),
                                );
                                let pill_w = btn_h + 6.0 + name_galley.size().x + 10.0;
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(pill_w, btn_h),
                                    egui::Sense::click(),
                                );
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
                                        egui::StrokeKind::Outside,
                                    );

                                    // Avatar circle
                                    let avatar_r = btn_h * 0.34;
                                    let avatar_c =
                                        egui::pos2(rect.left() + btn_h * 0.5, rect.center().y);

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
                                                self.emote_bytes.get(url).map(|(_, _, raw)| {
                                                    (url.to_owned(), raw.clone())
                                                })
                                            })
                                        });

                                    if let Some((logo, raw)) = avatar_bytes {
                                        let av_size = avatar_r * 2.0;
                                        let av_rect = egui::Rect::from_center_size(
                                            avatar_c,
                                            egui::vec2(av_size, av_size),
                                        );
                                        ui.painter().circle_filled(
                                            avatar_c,
                                            avatar_r,
                                            t::bg_raised(),
                                        );
                                        if let Some(tex) =
                                            self.static_avatar_texture_for(ui, &logo, &raw)
                                        {
                                            ui.put(
                                                av_rect,
                                                egui::Image::new((
                                                    tex.id(),
                                                    egui::vec2(av_size, av_size),
                                                ))
                                                .corner_radius(egui::CornerRadius::same(
                                                    avatar_r as u8,
                                                )),
                                            );
                                        } else {
                                            let uri = bytes_uri(&logo, raw.as_ref());
                                            ui.put(
                                                av_rect,
                                                egui::Image::from_bytes(
                                                    uri,
                                                    egui::load::Bytes::Shared(raw),
                                                )
                                                .fit_to_exact_size(egui::vec2(av_size, av_size))
                                                .corner_radius(egui::CornerRadius::same(
                                                    avatar_r as u8,
                                                )),
                                            );
                                        }
                                    } else {
                                        ui.painter().circle_filled(
                                            avatar_c,
                                            avatar_r,
                                            t::accent_dim(),
                                        );
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
                                        &name,
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
                                ("👤", t::BAR_H)
                            } else if self.state.accounts.is_empty() {
                                ("Log in", 68.0)
                            } else {
                                ("Accounts", 68.0)
                            };
                            if ui
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
                    });
                });
            });

        // -- Stream info bar --------------------------------------------------
        // Shows live/offline status, viewer count and stream title for the
        // currently active channel. Hidden when no channel is active.
        if let Some(active_ch) = self.state.active_channel.as_ref() {
            if !active_ch.is_twitch() {
                TopBottomPanel::top("stream_info_bar")
                    .exact_height(28.0)
                    .frame(
                        Frame::new()
                            .fill(t::bg_surface())
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .stroke(egui::Stroke::NONE),
                    )
                    .show(ctx, |ui| {
                        let prefix = if active_ch.is_kick() || active_ch.is_irc_server_tab() {
                            ""
                        } else {
                            "#"
                        };
                        let platform = if active_ch.is_kick() { "Kick" } else { "IRC" };
                        let topic = self
                            .state
                            .channels
                            .get(active_ch)
                            .and_then(|ch| ch.topic.as_deref())
                            .unwrap_or("");
                        let bar_w = ui.available_width();
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("{prefix}{}", active_ch.display_name()))
                                        .strong()
                                        .font(t::small())
                                        .color(t::text_primary()),
                                )
                                .truncate(),
                            );
                            if bar_w > 120.0 {
                                ui.label(
                                    RichText::new(platform)
                                        .font(t::small())
                                        .color(t::text_muted()),
                                );
                            }
                            if !topic.is_empty() && bar_w > 200.0 {
                                ui.label(
                                    RichText::new("-").font(t::small()).color(t::text_muted()),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(topic)
                                            .font(t::small())
                                            .color(t::text_secondary()),
                                    )
                                    .truncate(),
                                );
                            }
                        });
                    });
            } else {
                let login = active_ch.display_name();
                let status = self.stream_statuses.get(login);
                // Subtle red tint on the bar background when the channel is live.
                let bar_is_live = status.map(|s| s.is_live).unwrap_or(false);
                let bar_fill = if bar_is_live {
                    t::live_tint_bg()
                } else {
                    t::bg_surface()
                };
                TopBottomPanel::top("stream_info_bar")
                    .exact_height(28.0)
                    .frame(
                        Frame::new()
                            .fill(bar_fill)
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .stroke(egui::Stroke::NONE),
                    )
                    .show(ctx, |ui| {
                        let bar_w = ui.available_width();
                        let compact = bar_w < 640.0;
                        let ultra_compact = bar_w < 360.0;
                        let show_viewers = bar_w >= 420.0;
                        let show_game = bar_w >= 700.0;
                        let show_title = bar_w >= 260.0;

                        // Thin accent stripe on the very left edge when live.
                        if bar_is_live {
                            let br = ui.max_rect();
                            let strip = egui::Rect::from_min_size(
                                br.left_top(),
                                egui::vec2(3.0, br.height()),
                            );
                            ui.painter().rect_filled(strip, 0.0, t::red());
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = if ultra_compact { 4.0 } else { 8.0 };
                            match status {
                                None => {
                                    // Not fetched yet – show the channel name only.
                                    let ch_prefix =
                                        if active_ch.is_kick() || active_ch.is_irc_server_tab() {
                                            ""
                                        } else {
                                            "#"
                                        };
                                    ui.label(
                                        RichText::new(format!(
                                            "{ch_prefix}{}",
                                            active_ch.display_name()
                                        ))
                                        .strong()
                                        .font(t::small())
                                        .color(t::text_primary()),
                                    );
                                    if !ultra_compact {
                                        ui.label(
                                            RichText::new("Fetching stream status…")
                                                .font(t::small())
                                                .color(t::text_muted()),
                                        );
                                    }
                                }
                                Some(s) => {
                                    let status_text = if s.is_live { "LIVE" } else { "OFFLINE" };
                                    let status_col =
                                        if s.is_live { t::red() } else { t::text_muted() };
                                    let status_bg = if s.is_live {
                                        Color32::from_rgba_unmultiplied(200, 45, 45, 30)
                                    } else {
                                        Color32::from_rgba_unmultiplied(120, 120, 120, 20)
                                    };

                                    egui::Frame::new()
                                        .fill(status_bg)
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            status_col.gamma_multiply(0.5),
                                        ))
                                        .corner_radius(t::RADIUS_SM)
                                        .inner_margin(egui::Margin::symmetric(6, 1))
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing.x = 4.0;
                                                ui.label(
                                                    RichText::new("●")
                                                        .font(t::small())
                                                        .color(status_col),
                                                );
                                                if !ultra_compact {
                                                    ui.label(
                                                        RichText::new(status_text)
                                                            .font(t::small())
                                                            .color(status_col)
                                                            .strong(),
                                                    );
                                                }
                                            });
                                        });

                                    let ch_prefix2 =
                                        if active_ch.is_kick() || active_ch.is_irc_server_tab() {
                                            ""
                                        } else {
                                            "#"
                                        };
                                    ui.label(
                                        RichText::new(format!(
                                            "{ch_prefix2}{}",
                                            active_ch.display_name()
                                        ))
                                        .strong()
                                        .font(t::small())
                                        .color(t::text_primary()),
                                    );

                                    // Viewer count (live only)
                                    if s.is_live {
                                        if show_viewers {
                                            if let Some(viewers) = s.viewers {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} viewers",
                                                        fmt_viewers(viewers)
                                                    ))
                                                    .font(t::small())
                                                    .color(t::text_secondary()),
                                                );
                                            }
                                        }

                                        // Game
                                        if show_game {
                                            if let Some(ref game) = s.game {
                                                if !game.is_empty() {
                                                    ui.label(
                                                        RichText::new(game.as_str())
                                                            .font(t::small())
                                                            .color(t::text_secondary()),
                                                    );
                                                }
                                            }
                                        }

                                        // Stream title uses any remaining horizontal space.
                                        if show_title {
                                            if let Some(ref title) = s.title {
                                                if !title.is_empty() {
                                                    let rem = ui.available_width();
                                                    let min_title_w = if ultra_compact {
                                                        24.0
                                                    } else if compact {
                                                        56.0
                                                    } else {
                                                        140.0
                                                    };
                                                    if rem > min_title_w {
                                                        ui.add_sized(
                                                            [rem, 16.0],
                                                            egui::Label::new(
                                                                RichText::new(title.as_str())
                                                                    .font(t::small())
                                                                    .color(t::text_muted()),
                                                            )
                                                            .truncate(),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        if let Some(ref title) = s.title {
                                            if !title.is_empty() && show_title {
                                                let rem = ui.available_width();
                                                let min_title_w = if ultra_compact {
                                                    24.0
                                                } else if compact {
                                                    56.0
                                                } else {
                                                    80.0
                                                };
                                                if rem > min_title_w {
                                                    ui.add_sized(
                                                        [rem, 16.0],
                                                        egui::Label::new(
                                                            RichText::new(title.as_str())
                                                                .font(t::small())
                                                                .color(t::text_muted()),
                                                        )
                                                        .truncate(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    });

                // -- Room state pills (sub-only, slow, emote-only, etc.) ------
                // Shown as a thin strip below the stream info bar when any mode
                // is active - Twitch channels only.
                let room = self.state.channels.get(active_ch).map(|ch| &ch.room_state);
                let live_viewers = status.and_then(|s| if s.is_live { s.viewers } else { None });
                let has_active_modes = room
                    .map(|rs| {
                        rs.emote_only
                            || rs.subscribers_only
                            || rs.r9k
                            || rs.followers_only.map(|v| v >= 0).unwrap_or(false)
                            || rs.slow_mode.map(|v| v > 0).unwrap_or(false)
                    })
                    .unwrap_or(false)
                    || live_viewers.is_some();
                if has_active_modes {
                    TopBottomPanel::top("room_state_bar")
                        .exact_height(20.0)
                        .frame(
                            Frame::new()
                                .fill(t::bg_base())
                                .inner_margin(egui::Margin::symmetric(8, 2))
                                .stroke(egui::Stroke::NONE),
                        )
                        .show(ctx, |ui| {
                            ui.horizontal_centered(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if let Some(rs) = room {
                                    if rs.emote_only {
                                        room_state_pill(ui, "Emote Only", t::accent());
                                    }
                                    if rs.subscribers_only {
                                        room_state_pill(ui, "Sub Only", t::gold());
                                    }
                                    if let Some(slow) = rs.slow_mode {
                                        if slow > 0 {
                                            room_state_pill(
                                                ui,
                                                &format!("Slow {slow}s"),
                                                t::yellow(),
                                            );
                                        }
                                    }
                                    if let Some(fol) = rs.followers_only {
                                        if fol >= 0 {
                                            let label = format_followers_only_label(fol);
                                            room_state_pill(ui, &label, t::text_secondary());
                                        }
                                    }
                                    if rs.r9k {
                                        room_state_pill(ui, "R9K", t::text_muted());
                                    }
                                }
                                if let Some(viewers) = live_viewers {
                                    room_state_pill(
                                        ui,
                                        &format!("Viewers {}", fmt_viewers(viewers)),
                                        t::raid_cyan(),
                                    );
                                }
                            });
                        });
                }
            }

            // -- Pinned message strip (Twitch/Kick pinned/elevated messages) ---
            // Show the latest pinned message near the top of chat.
            let latest_pinned = self.state.channels.get(active_ch).and_then(|ch| {
                ch.messages
                    .iter()
                    .rev()
                    .find(|m| m.flags.is_pinned && !m.flags.is_deleted)
                    .map(|m| (m.sender.display_name.clone(), m.raw_text.clone()))
            });
            if let Some((sender, text)) = latest_pinned {
                TopBottomPanel::top("pinned_message_bar")
                    .exact_height(24.0)
                    .frame(
                        Frame::new()
                            .fill(Color32::from_rgba_unmultiplied(255, 215, 0, 18))
                            .inner_margin(egui::Margin::symmetric(8, 3))
                            .stroke(egui::Stroke::new(1.0, t::gold().gamma_multiply(0.45))),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(
                                RichText::new("📌 Pinned")
                                    .font(t::small())
                                    .strong()
                                    .color(t::gold()),
                            );
                            ui.label(RichText::new("·").font(t::small()).color(t::text_muted()));
                            ui.label(
                                RichText::new(format!("{sender}:"))
                                    .font(t::small())
                                    .strong()
                                    .color(t::text_primary()),
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text)
                                        .font(t::small())
                                        .color(t::text_secondary()),
                                )
                                .truncate(),
                            );
                        });
                    });
            }
        }

        // -- Channel list: left sidebar OR top tab strip ----------------------
        // Accumulate actions outside the panel closure so we can call &mut self
        // methods after the panel is done drawing.
        let mut ch_selected: Option<ChannelId> = None;
        let mut ch_closed: Option<ChannelId> = None;
        let mut ch_reordered: Option<Vec<ChannelId>> = None;
        let mut ch_drag_split: Option<ChannelId> = None;
        let mut show_split_drop_zone = false;

        match self.channel_layout {
            // ── Top-tab strip ────────────────────────────────────────────────
            ChannelLayout::TopTabs => {
                TopBottomPanel::top("channel_tabs")
                    .exact_height(32.0)
                    .frame(
                        Frame::new()
                            .fill(t::bg_surface())
                            .inner_margin(egui::Margin::symmetric(6, 0))
                            .stroke(egui::Stroke::new(1.0, t::border_subtle())),
                    )
                    .show(ctx, |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("channel_tabs_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.horizontal_centered(|ui| {
                                    ui.spacing_mut().item_spacing.x = 2.0;
                                    for ch in self.state.channel_order.iter() {
                                        let is_active =
                                            self.state.active_channel.as_ref() == Some(ch);
                                        let (unread, mentions) = self
                                            .state
                                            .channels
                                            .get(ch)
                                            .map(|s| (s.unread_count, s.unread_mentions))
                                            .unwrap_or((0, 0));

                                        // Build tab label with optional badge.
                                        let tab_name = ch.display_name();
                                        let label = if mentions > 0 {
                                            format!("{tab_name} ●{mentions}")
                                        } else if unread > 0 {
                                            format!("{tab_name} {unread}")
                                        } else {
                                            tab_name.to_owned()
                                        };

                                        let (fg, bg) = if is_active {
                                            (t::text_primary(), t::accent_dim())
                                        } else if mentions > 0 {
                                            (t::accent(), t::bg_surface())
                                        } else if unread > 0 {
                                            (t::text_primary(), t::bg_surface())
                                        } else {
                                            (t::text_secondary(), t::bg_surface())
                                        };

                                        let resp = ui.add(
                                            egui::Button::new(
                                                RichText::new(&label).font(t::small()).color(fg),
                                            )
                                            .fill(bg)
                                            .sense(egui::Sense::click_and_drag()),
                                        );

                                        if resp.clicked() {
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
                let min_central = if window_width < NARROW_THRESHOLD {
                    140.0
                } else {
                    250.0
                };
                let sidebar_max = (ctx.screen_rect().width() - min_central)
                    .clamp(t::SIDEBAR_MIN_W, t::SIDEBAR_MAX_W);

                SidePanel::left("channel_list")
                    .resizable(true)
                    .default_width(t::SIDEBAR_W)
                    .min_width(t::SIDEBAR_MIN_W)
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
            if let Some(state) = self.state.channels.get_mut(&ch) {
                state.mark_read();
            }
            // In split mode, switch the focused pane's channel.
            if !self.split_panes.panes.is_empty() {
                let f = self.split_panes.focused;
                if let Some(pane) = self.split_panes.panes.get_mut(f) {
                    pane.channel = ch.clone();
                    pane.input_buf.clear();
                }
            }
            self.state.active_channel = Some(ch);
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
            if !self.split_panes.contains_channel(&ch) {
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
                        .default_width(220.0)
                        .min_width(180.0)
                        .max_width(340.0)
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
                    let usable_w =
                        total.width() - sep_w * (n as f32 - 1.0);
                    let mut close_pane: Option<usize> = None;

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

                        // ── Pane header (manually painted, edge-to-edge) ─────
                        {
                            let hdr_rect = egui::Rect::from_min_size(
                                pane_rect.min,
                                egui::vec2(pane_w, 24.0),
                            );
                            let hdr_fill = if is_focused {
                                t::accent_dim()
                            } else {
                                t::bg_surface()
                            };
                            ui.painter().rect_filled(
                                hdr_rect,
                                egui::CornerRadius::ZERO,
                                hdr_fill,
                            );
                            ui.painter().hline(
                                hdr_rect.x_range(),
                                hdr_rect.bottom(),
                                egui::Stroke::new(1.0, t::border_subtle()),
                            );
                            let hdr_content = hdr_rect.shrink2(egui::vec2(6.0, 0.0));
                            let mut hdr_ui = pane_ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(hdr_content)
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            );
                            hdr_ui.set_clip_rect(hdr_rect);
                            hdr_ui.label(
                                RichText::new(format!(
                                    "# {}",
                                    ch.display_name()
                                ))
                                .font(t::small())
                                .strong()
                                .color(if is_focused {
                                    t::text_primary()
                                } else {
                                    t::text_secondary()
                                }),
                            );
                            hdr_ui.with_layout(
                                egui::Layout::right_to_left(
                                    egui::Align::Center,
                                ),
                                |ui| {
                                    let cb = ui.add(
                                        egui::Label::new(
                                            RichText::new("✕")
                                                .font(t::small())
                                                .color(t::text_muted()),
                                        )
                                        .sense(egui::Sense::click()),
                                    );
                                    if cb.clicked() {
                                        close_pane = Some(pi);
                                    }
                                    if cb.hovered() {
                                        ui.ctx().set_cursor_icon(
                                            egui::CursorIcon::PointingHand,
                                        );
                                    }
                                },
                            );
                            // Advance pane_ui cursor past the header.
                            pane_ui.allocate_space(egui::vec2(pane_w, 24.0));
                        }

                        // ── Pane chat input (bottom) ─────────────
                        let input_h = t::BAR_H
                            + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom)
                                as f32;
                        let input_rect = egui::Rect::from_min_size(
                            egui::pos2(
                                pane_rect.left(),
                                pane_rect.bottom() - input_h,
                            ),
                            egui::vec2(pane_w, input_h),
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
                                            egui::pos2(pane_rect.left() + 6.0, pane_rect.top() + 30.0),
                                            egui::pos2(pane_rect.right() - 6.0, input_rect.top()),
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
                                        ) + 8.0;
                                    }
                                }
                            }
                        }
                        let msg_rect = egui::Rect::from_min_max(
                            egui::pos2(pane_rect.left(), pane_rect.top() + 24.0 + search_h),
                            egui::pos2(pane_rect.right(), input_rect.top()),
                        );
                        if let Some(ch_state) =
                            self.state.channels.get(&ch)
                        {
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
                            let _is_mod = ch_state.is_mod || is_bc;
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

                    // Close-pane action
                    if let Some(idx) = close_pane {
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
        self.event_toasts
            .retain(|t| t.born.elapsed().as_secs_f32() < 5.0);
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
        if !self.event_toasts.is_empty() {
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
    }
}

// Helper functions

/// Render a tiny colored pill label (used for room-state modes in the stream bar).
fn room_state_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            20,
        ))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.4)))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(5, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).font(t::tiny()).color(color).strong());
        });
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

fn fmt_viewers(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_followers_only_label(minutes: i32) -> String {
    if minutes <= 0 {
        return "Followers-only".to_owned();
    }

    let total = minutes as i64;
    let days = total / 1_440;
    let hours = (total % 1_440) / 60;
    let mins = total % 60;

    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 && parts.len() < 2 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 && parts.len() < 2 {
        parts.push(format!("{mins}m"));
    }
    if parts.is_empty() {
        parts.push("0m".to_owned());
    }

    format!("Followers-only {}", parts.join(" "))
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
    _is_mod: bool,
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
        // /raid, /host, /commercial, /uniquechat, /marker, /block, /unblock,
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
