use std::collections::HashMap;
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{AppCommand, AppEvent, ConnectionState, LinkPreview},
    model::{
        ChannelId, ChannelState, EmoteCatalogEntry, MsgKind, ReplyInfo,
        IRC_SERVER_CONTROL_CHANNEL,
    },
    AppState,
};

use crate::commands::render_help_message;
use crate::perf::PerfOverlay;
use crate::theme as t;
use crate::widgets::{
    analytics::AnalyticsPanel,
    channel_list::ChannelList,
    chat_input::ChatInput,
    emote_picker::EmotePicker,
    irc_status::IrcStatusPanel,
    join_dialog::JoinDialog,
    loading_screen::{LoadEvent, LoadingScreen},
    login_dialog::{LoginAction, LoginDialog},
    message_list::MessageList,
    user_profile_popup::{PopupAction, UserProfilePopup},
};

// Channel layout mode

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
    /// Wall-clock moment the toast was created.
    born: std::time::Instant,
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
    pending_reply: Option<ReplyInfo>,
    /// User profile card (Chatterino-style, shown on username click).
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
}

impl CrustApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd_tx: mpsc::Sender<AppCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // -- Visuals -----------------------------------------------------------
        let mut vis = egui::Visuals::dark();
        vis.override_text_color = Some(t::TEXT_PRIMARY);
        vis.panel_fill = t::BG_BASE;
        vis.window_fill = t::BG_DIALOG;
        vis.extreme_bg_color = t::BG_RAISED; // TextEdit / ComboBox fill

        vis.widgets.inactive.weak_bg_fill = t::BG_SURFACE;
        vis.widgets.inactive.bg_fill = t::BG_SURFACE;
        vis.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, t::TEXT_SECONDARY);
        vis.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, t::BORDER_SUBTLE);
        vis.widgets.inactive.corner_radius = t::RADIUS;

        vis.widgets.hovered.weak_bg_fill = t::HOVER_BG;
        vis.widgets.hovered.bg_fill = t::HOVER_BG;
        vis.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, t::TEXT_PRIMARY);
        vis.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, t::BORDER_ACCENT);
        vis.widgets.hovered.corner_radius = t::RADIUS;

        vis.widgets.active.weak_bg_fill = t::ACCENT_DIM;
        vis.widgets.active.bg_fill = t::ACCENT_DIM;
        vis.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::WHITE);
        vis.widgets.active.bg_stroke = egui::Stroke::new(1.0, t::ACCENT);
        vis.widgets.active.corner_radius = t::RADIUS;

        vis.widgets.open.weak_bg_fill = t::BG_RAISED;
        vis.widgets.open.bg_fill = t::BG_RAISED;

        vis.selection.bg_fill = t::ACCENT_DIM;
        vis.selection.stroke = egui::Stroke::new(1.0, t::ACCENT);

        vis.window_corner_radius = t::RADIUS;
        vis.window_stroke = t::STROKE_SUBTLE;
        vis.menu_corner_radius = t::RADIUS;
        vis.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, t::BORDER_SUBTLE);

        let mut style = egui::Style {
            visuals: vis,
            ..(*cc.egui_ctx.style()).clone()
        };
        style.spacing.item_spacing = t::ITEM_SPACING;
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin = Margin::same(10);
        style.interaction.tooltip_delay = 0.0;
        style.interaction.tooltip_grace_time = 0.5;
        cc.egui_ctx.set_style(style);

        install_system_fallback_fonts(&cc.egui_ctx);

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
            live_map_cache: HashMap::new(),
            event_toasts: Vec::new(),
            settings_open: false,
            kick_beta_enabled: false,
            irc_beta_enabled: false,
            irc_nickserv_user: String::new(),
            irc_nickserv_pass: String::new(),
            always_on_top: false,
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
                // Kick off an immediate stream-status fetch for the new channel (Twitch only).
                if channel.is_twitch() {
                    let login = channel.display_name().to_lowercase();
                    if is_valid_twitch_login(&login) {
                        self.stream_status_fetched
                            .insert(login.clone(), std::time::Instant::now());
                        self.send_cmd(AppCommand::FetchUserProfile { login });
                    }
                }
            }
            AppEvent::ChannelParted { channel } => {
                self.state.leave_channel(&channel);
            }
            AppEvent::ChannelRedirected {
                old_channel,
                new_channel,
            } => {
                self.state.redirect_channel(&old_channel, &new_channel);
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
                                let text = if *is_gift {
                                    format!("🎁  {} received a gifted {} sub!", display_name, plan)
                                } else if *months <= 1 {
                                    format!("⭐  {} just subscribed with {}!", display_name, plan)
                                } else {
                                    format!("⭐  {} resubscribed x{}!", display_name, months)
                                };
                                Some(EventToast {
                                    text,
                                    hue: t::GOLD,
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
                                hue: t::RAID_CYAN,
                                born: std::time::Instant::now(),
                            }),
                            MsgKind::Bits { amount } if *amount >= 100 => Some(EventToast {
                                text: format!(
                                    "💎  {} cheered {} bits!",
                                    message.sender.display_name, amount
                                ),
                                hue: t::BITS_ORANGE,
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

                if let Some(ch) = self.state.channels.get_mut(&channel) {
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
                            if message.flags.is_mention || message.flags.is_highlighted {
                                ch.unread_mentions += 1;
                            }
                        }
                        ch.push_message(message);
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
                    self.user_profile_popup.set_profile(profile);
                }
            }
            AppEvent::UserProfileUnavailable { login } => {
                if self.user_profile_popup.accepts_profile(&login) {
                    self.user_profile_popup.set_unavailable(&login);
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
            } => {
                self.link_previews.insert(
                    url,
                    LinkPreview {
                        title,
                        description,
                        thumbnail_url,
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
            AppEvent::ChannelEmotesLoaded { .. } => {
                // Handled in the loading-screen pre-pass above; nothing else to do.
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
        }
    }

    fn send_cmd(&self, cmd: AppCommand) {
        if self.cmd_tx.try_send(cmd).is_err() {
            warn!("Command channel full/closed");
        }
    }
}

// eframe::App implementation

impl eframe::App for CrustApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let events = self.drain_events(ctx);
        let had_events = events > 0;

        // Smart repaint: repaint immediately when events arrive so back-to-
        // back messages drain quickly.  Otherwise poll at a relaxed 100 ms
        // interval - user interactions (mouse, keyboard, scroll) already
        // trigger repaints via egui, and GIF animation is driven by the
        // image loaders internally.
        if had_events {
            ctx.request_repaint(); // drain the next batch ASAP
        }
        // Bump to 33 ms (~30 fps) when live-dot animations or toast slide-ins
        // are active so they look smooth.
        // PERF: avoid per-frame to_lowercase() allocation - check live status
        // directly against stream_statuses values.
        let has_live_sidebar = self.stream_statuses.values().any(|s| s.is_live);
        let repaint_ms = if has_live_sidebar || !self.event_toasts.is_empty() {
            33
        } else {
            100
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
        self.perf.show(ctx);

        // Keep analytics cache warm even when the panel is hidden, so data
        // is ready the moment the user opens it.
        if let Some(ref ch) = self.state.active_channel {
            if let Some(ch_state) = self.state.channels.get(ch) {
                self.analytics_panel.tick(ch_state);
            }
        }

        // Periodic stream-status refresh: re-fetch every 60 s per channel.
        // PERF: iterate without collecting into a Vec; avoid per-channel
        // to_lowercase() allocations when all timestamps are fresh.
        const STREAM_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);
        {
            let mut stale: Vec<String> = Vec::new();
            for ch in &self.state.channel_order {
                if !ch.is_twitch() {
                    continue;
                }
                let login = ch.display_name().to_lowercase();
                if !is_valid_twitch_login(&login) {
                    continue;
                }
                let is_stale = self
                    .stream_status_fetched
                    .get(&login)
                    .map(|t| t.elapsed() >= STREAM_REFRESH)
                    .unwrap_or(true);
                if is_stale {
                    stale.push(login);
                }
            }
            for login in stale {
                self.stream_status_fetched
                    .insert(login.clone(), std::time::Instant::now());
                self.send_cmd(AppCommand::FetchUserProfile { login });
            }
        }

        // Render profile popup and dispatch any moderation action.
        if let Some(action) = self.user_profile_popup.show(ctx, &self.emote_bytes) {
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
            }
        }

        // -- Dialogs -----------------------------------------------------------
        if let Some(ch) = self
            .join_dialog
            .show(ctx, self.kick_beta_enabled, self.irc_beta_enabled)
        {
            self.send_cmd(AppCommand::JoinChannel { channel: ch });
        }
        if let Some(action) = self.login_dialog.show(
            ctx,
            self.state.auth.logged_in,
            self.state.auth.username.as_deref(),
            self.state.auth.avatar_url.as_deref(),
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

            egui::Window::new("Settings")
                .open(&mut settings_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let settings_w = (ctx.screen_rect().width() - 24.0).clamp(200.0, 400.0);
                    ui.set_min_width(settings_w);
                    ui.set_max_width(settings_w);

                    ui.label(
                        RichText::new("Window")
                            .font(t::body())
                            .strong()
                            .color(t::TEXT_PRIMARY),
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
                            .color(t::TEXT_PRIMARY),
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
                            .color(t::TEXT_PRIMARY),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Automatically identify with NickServ when connecting to IRC servers.",
                        )
                        .font(t::small())
                        .color(t::TEXT_MUTED),
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
                        .color(t::TEXT_MUTED),
                    );
                });

            self.settings_open = settings_open;
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
            .exact_height(if window_width < VERY_NARROW_THRESHOLD { 28.0 } else { 36.0 })
            .frame(
                Frame::new()
                    .fill(t::BG_SURFACE)
                    .inner_margin(t::BAR_MARGIN)
                    .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
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
                        let logo_w = ui
                            .fonts(|f| {
                                f.layout_no_wrap("crust".to_owned(), logo_font.clone(), t::ACCENT)
                                    .rect
                                    .width()
                            })
                            + 4.0;
                        ui.allocate_ui_with_layout(
                            egui::vec2(logo_w, t::BAR_H),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new("crust")
                                        .font(logo_font)
                                        .strong()
                                        .color(t::ACCENT),
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
                        let conn_w = ui
                            .fonts(|f| {
                                f.layout_no_wrap(
                                    conn_label.to_owned(),
                                    conn_font.clone(),
                                    t::TEXT_SECONDARY,
                                )
                                .rect
                                .width()
                            })
                            + 4.0;
                        ui.allocate_ui_with_layout(
                            egui::vec2(conn_w, t::BAR_H),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(conn_label)
                                        .font(conn_font)
                                        .color(t::TEXT_SECONDARY),
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
                                    egui::Button::new(RichText::new(settings_label).font(t::small())),
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
                                    egui::Button::new(RichText::new(irc_status_label).font(t::small())),
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
                                    .color(t::TEXT_MUTED),
                            );

                            ui.separator();
                        }

                        // Login / Account button
                        if self.state.auth.logged_in {
                            let name = self.state.auth.username.as_deref().unwrap_or("User");
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
                                    name.to_owned(),
                                    t::small(),
                                    t::TEXT_PRIMARY,
                                );
                                let pill_w = btn_h + 6.0 + name_galley.size().x + 10.0;
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(pill_w, btn_h),
                                    egui::Sense::click(),
                                );
                                resp.clone().on_hover_text("Account");

                                if ui.is_rect_visible(rect) {
                                    let bg = if resp.hovered() {
                                        t::BG_RAISED
                                    } else {
                                        t::BG_SURFACE
                                    };
                                    let border = if resp.hovered() {
                                        t::BORDER_ACCENT
                                    } else {
                                        t::BORDER_SUBTLE
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
                                    let avatar_bytes =
                                        self.state.auth.avatar_url.as_deref().and_then(|url| {
                                            self.emote_bytes
                                                .get(url)
                                                .map(|(_, _, raw)| (url, raw.clone()))
                                        });

                                    if let Some((logo, raw)) = avatar_bytes {
                                        let uri = format!("bytes://{logo}");
                                        let av_size = avatar_r * 2.0;
                                        let av_rect = egui::Rect::from_center_size(
                                            avatar_c,
                                            egui::vec2(av_size, av_size),
                                        );
                                        ui.painter().circle_filled(avatar_c, avatar_r, t::BG_RAISED);
                                        ui.put(
                                            av_rect,
                                            egui::Image::from_bytes(
                                                uri,
                                                egui::load::Bytes::Shared(raw),
                                            )
                                            .fit_to_exact_size(egui::vec2(av_size, av_size))
                                            .corner_radius(egui::CornerRadius::same(avatar_r as u8)),
                                        );
                                    } else {
                                        ui.painter()
                                            .circle_filled(avatar_c, avatar_r, t::ACCENT_DIM);
                                        ui.painter().text(
                                            avatar_c,
                                            egui::Align2::CENTER_CENTER,
                                            initial.to_string(),
                                            egui::FontId::proportional(avatar_r * 1.15),
                                            t::TEXT_PRIMARY,
                                        );
                                    }

                                    // Username
                                    ui.painter().text(
                                        egui::pos2(avatar_c.x + btn_h * 0.5 + 4.0, rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        name,
                                        t::small(),
                                        t::TEXT_PRIMARY,
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
                            .fill(t::BG_SURFACE)
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
                                        .color(t::TEXT_PRIMARY),
                                )
                                .truncate(),
                            );
                            if bar_w > 120.0 {
                                ui.label(
                                    RichText::new(platform)
                                        .font(t::small())
                                        .color(t::TEXT_MUTED),
                                );
                            }
                            if !topic.is_empty() && bar_w > 200.0 {
                                ui.label(
                                    RichText::new("-")
                                        .font(t::small())
                                        .color(t::TEXT_MUTED),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(topic)
                                            .font(t::small())
                                            .color(t::TEXT_SECONDARY),
                                    )
                                    .truncate(),
                                );
                            }
                        });
                    });
            } else {
                let login = active_ch.display_name().to_lowercase();
                let status = self.stream_statuses.get(&login);
                // Subtle red tint on the bar background when the channel is live.
                let bar_is_live = status.map(|s| s.is_live).unwrap_or(false);
                let bar_fill = if bar_is_live {
                    Color32::from_rgb(24, 14, 14)
                } else {
                    t::BG_SURFACE
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
                        let show_title = bar_w >= 500.0;

                        // Thin accent stripe on the very left edge when live.
                        if bar_is_live {
                            let br = ui.max_rect();
                            let strip = egui::Rect::from_min_size(
                                br.left_top(),
                                egui::vec2(3.0, br.height()),
                            );
                            ui.painter().rect_filled(strip, 0.0, t::RED);
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
                                        .color(t::TEXT_PRIMARY),
                                    );
                                    if !ultra_compact {
                                        ui.label(
                                            RichText::new("Fetching stream status…")
                                                .font(t::small())
                                                .color(t::TEXT_MUTED),
                                        );
                                    }
                                }
                                Some(s) => {
                                    let status_text = if s.is_live { "LIVE" } else { "OFFLINE" };
                                    let status_col = if s.is_live { t::RED } else { t::TEXT_MUTED };
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
                                        .color(t::TEXT_PRIMARY),
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
                                                    .color(t::TEXT_SECONDARY),
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
                                                            .color(t::TEXT_SECONDARY),
                                                    );
                                                }
                                            }
                                        }

                                        // Stream title uses any remaining horizontal space.
                                        if show_title {
                                            if let Some(ref title) = s.title {
                                                if !title.is_empty() {
                                                    let rem = ui.available_width();
                                                    if rem > if compact { 80.0 } else { 180.0 } {
                                                        let resp = ui.add_sized(
                                                            [rem, 16.0],
                                                            egui::Label::new(
                                                                RichText::new(title.as_str())
                                                                    .font(t::small())
                                                                    .color(t::TEXT_MUTED),
                                                            )
                                                            .truncate(),
                                                        );
                                                        resp.on_hover_text(title.as_str());
                                                    }
                                                }
                                            }
                                        }
                                    } else if !compact {
                                        if let Some(ref title) = s.title {
                                            if !title.is_empty() && show_title {
                                                let rem = ui.available_width();
                                                if rem > 80.0 {
                                                    let resp = ui.add_sized(
                                                        [rem, 16.0],
                                                        egui::Label::new(
                                                            RichText::new(title.as_str())
                                                                .font(t::small())
                                                                .color(t::TEXT_MUTED),
                                                        )
                                                        .truncate(),
                                                    );
                                                    resp.on_hover_text(title.as_str());
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
                let room = self
                    .state
                    .channels
                    .get(active_ch)
                    .map(|ch| &ch.room_state);
                let has_active_modes = room
                    .map(|rs| {
                        rs.emote_only
                            || rs.subscribers_only
                            || rs.r9k
                            || rs.followers_only.map(|v| v >= 0).unwrap_or(false)
                            || rs.slow_mode.map(|v| v > 0).unwrap_or(false)
                    })
                    .unwrap_or(false);
                if has_active_modes {
                    TopBottomPanel::top("room_state_bar")
                        .exact_height(20.0)
                        .frame(
                            Frame::new()
                                .fill(t::BG_BASE)
                                .inner_margin(egui::Margin::symmetric(8, 2))
                                .stroke(egui::Stroke::NONE),
                        )
                        .show(ctx, |ui| {
                            ui.horizontal_centered(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if let Some(rs) = room {
                                    if rs.emote_only {
                                        room_state_pill(ui, "Emote Only", t::ACCENT);
                                    }
                                    if rs.subscribers_only {
                                        room_state_pill(ui, "Sub Only", t::GOLD);
                                    }
                                    if let Some(slow) = rs.slow_mode {
                                        if slow > 0 {
                                            room_state_pill(ui, &format!("Slow {slow}s"), t::YELLOW);
                                        }
                                    }
                                    if let Some(fol) = rs.followers_only {
                                        if fol >= 0 {
                                            let label = if fol == 0 {
                                                "Followers".to_owned()
                                            } else {
                                                format!("Followers {fol}m")
                                            };
                                            room_state_pill(ui, &label, t::TEXT_SECONDARY);
                                        }
                                    }
                                    if rs.r9k {
                                        room_state_pill(ui, "R9K", t::TEXT_MUTED);
                                    }
                                }
                            });
                        });
                }
            }
        }

        // -- Channel list: left sidebar OR top tab strip ----------------------
        // Accumulate actions outside the panel closure so we can call &mut self
        // methods after the panel is done drawing.
        let mut ch_selected: Option<ChannelId> = None;
        let mut ch_closed: Option<ChannelId> = None;
        let mut ch_reordered: Option<Vec<ChannelId>> = None;

        match self.channel_layout {
            // ── Top-tab strip ────────────────────────────────────────────────
            ChannelLayout::TopTabs => {
                TopBottomPanel::top("channel_tabs")
                    .exact_height(32.0)
                    .frame(
                        Frame::new()
                            .fill(t::BG_SURFACE)
                            .inner_margin(egui::Margin::symmetric(6, 0))
                            .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
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
                                            (t::TEXT_PRIMARY, t::ACCENT_DIM)
                                        } else if mentions > 0 {
                                            (t::ACCENT, t::BG_SURFACE)
                                        } else if unread > 0 {
                                            (t::TEXT_PRIMARY, t::BG_SURFACE)
                                        } else {
                                            (t::TEXT_SECONDARY, t::BG_SURFACE)
                                        };

                                        let resp = ui.add(
                                            egui::Button::new(
                                                RichText::new(&label).font(t::small()).color(fg),
                                            )
                                            .fill(bg),
                                        );

                                        if resp.clicked() {
                                            ch_selected = Some(ch.clone());
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
                let min_central = if window_width < NARROW_THRESHOLD { 140.0 } else { 250.0 };
                let sidebar_max =
                    (ctx.screen_rect().width() - min_central).clamp(t::SIDEBAR_MIN_W, t::SIDEBAR_MAX_W);

                SidePanel::left("channel_list")
                    .resizable(true)
                    .default_width(t::SIDEBAR_W)
                    .min_width(t::SIDEBAR_MIN_W)
                    .max_width(sidebar_max)
                    .frame(
                        Frame::new()
                            .fill(t::BG_SURFACE)
                            .inner_margin(t::SIDEBAR_MARGIN)
                            .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
                    )
                    .show(ctx, |ui| {
                        ui.label(
                            RichText::new("CHANNELS")
                                .font(t::heading())
                                .strong()
                                .color(t::TEXT_MUTED),
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
            self.state.active_channel = Some(ch);
        }
        if let Some(ch) = ch_closed {
            self.send_cmd(AppCommand::LeaveChannel {
                channel: ch.clone(),
            });
            self.state.leave_channel(&ch);
        }
        if let Some(new_order) = ch_reordered {
            self.state.channel_order = new_order;
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
                                .fill(t::BG_SURFACE)
                                .inner_margin(t::SIDEBAR_MARGIN)
                                .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
                        )
                        .show(ctx, |ui| {
                            self.analytics_panel.show(ui, ch_state);
                        });
                }
            }
        }

        // -- Central area: messages + input ------------------------------------
        CentralPanel::default()
            .frame(Frame::new().fill(t::BG_BASE).inner_margin(Margin {
                left: 6,
                right: 0,
                top: 0,
                bottom: 0,
            }))
            .show(ctx, |ui| {
                if let Some(active_ch) = self.state.active_channel.clone() {
                    // Input tray pinned to bottom
                    let input_panel_h = if self.pending_reply.is_some() {
                        64.0
                    } else {
                        t::BAR_H + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom) as f32
                    };
                    TopBottomPanel::bottom("chat_input_panel")
                        .resizable(false)
                        .exact_height(input_panel_h)
                        .frame(
                            Frame::new()
                                .fill(t::BG_SURFACE)
                                .inner_margin(Margin::ZERO)
                                .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
                        )
                        .show_inside(ui, |ui| {
                            let chat = ChatInput {
                                channel: &active_ch,
                                logged_in: self.state.auth.logged_in,
                                username: self.state.auth.username.as_deref(),
                                emote_catalog: &self.emote_catalog,
                                emote_bytes: &self.emote_bytes,
                                pending_reply: self.pending_reply.as_ref(),
                                message_history: &self.message_history,
                                known_channels: &self.state.channel_order,
                            };
                            let result = chat.show(ui, &mut self.chat_input_buf);
                            if result.dismiss_reply {
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
                                    self.pending_reply.as_ref().map(|r| r.parent_msg_id.clone());
                                self.pending_reply = None;
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
                    ) {
                        if !self.chat_input_buf.is_empty() && !self.chat_input_buf.ends_with(' ') {
                            self.chat_input_buf.push(' ');
                        }
                        self.chat_input_buf.push_str(&code);
                        self.chat_input_buf.push(' ');
                    }

                    // Messages above the input
                    if let Some(state) = self.state.channels.get(&active_ch) {
                        let is_broadcaster = self
                            .state
                            .auth
                            .username
                            .as_deref()
                            .map(|u| u.eq_ignore_ascii_case(active_ch.display_name()))
                            .unwrap_or(false);
                        let is_mod = state.is_mod || is_broadcaster;
                        let ml_result = MessageList::new(
                            &state.messages,
                            &self.emote_bytes,
                            &self.cmd_tx,
                            &active_ch,
                            &self.link_previews,
                        )
                        .show(ui);
                        if let Some(r) = ml_result.reply {
                            self.pending_reply = Some(r);
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
                                .color(t::TEXT_MUTED)
                                .font(t::body()),
                        );
                    });
                }
            });

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
                    let fill_col =
                        Color32::from_rgba_unmultiplied(18, 16, 26, (225.0 * opacity) as u8);
                    egui::Frame::new()
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
                });
        }
        // Keep animating while toasts are live.
        if !self.event_toasts.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }

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
        .fill(Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 20))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.4)))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(5, 0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(t::tiny())
                    .color(color)
                    .strong(),
            );
        });
}

fn connection_indicator(state: &ConnectionState, logged_in: bool) -> (Color32, &'static str) {
    match state {
        ConnectionState::Connected if logged_in => (t::GREEN, "Connected"),
        ConnectionState::Connected => (t::GREEN, "Connected (anon)"),
        ConnectionState::Connecting => (t::YELLOW, "Connecting..."),
        ConnectionState::Reconnecting { .. } => (t::YELLOW, "Reconnecting..."),
        ConnectionState::Disconnected => (t::RED, "Disconnected"),
        ConnectionState::Error(_) => (t::RED, "Error"),
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
        // Noto Sans - broad multilingual coverage
        (
            "noto",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        ),
        ("noto", "/usr/share/fonts/noto/NotoSans-Regular.ttf"),
        ("noto", "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
        ("noto", "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc"),
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
        // Windows
        ("seguisym", "C:\\Windows\\Fonts\\seguisym.ttf"),
        ("arial", "C:\\Windows\\Fonts\\arial.ttf"),
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
            })
        }

        // /w <user> <message>  - Twitch whisper (pass straight through).
        "w" | "whisper" => Some(AppCommand::SendMessage {
            channel: channel.clone(),
            text: text.to_owned(),
            reply_to_msg_id,
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
