use std::collections::HashMap;
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{AppCommand, AppEvent, ConnectionState, LinkPreview},
    model::{ChannelId, EmoteCatalogEntry, MsgKind, ReplyInfo},
    AppState,
};

use crate::perf::PerfOverlay;
use crate::theme as t;
use crate::widgets::{
    analytics::AnalyticsPanel,
    channel_list::ChannelList,
    chat_input::ChatInput,
    emote_picker::EmotePicker,
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
    is_live:  bool,
    title:    Option<String>,
    game:     Option<String>,
    viewers:  Option<u64>,
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
    /// Startup loading overlay (shown until initial emotes + history are ready).
    loading_screen: LoadingScreen,
    /// Cached stream status per channel (key = channel login, lowercase).
    stream_statuses: HashMap<String, StreamStatusInfo>,
    /// When each channel's stream status was last fetched.
    stream_status_fetched: HashMap<String, std::time::Instant>,
    /// Short-lived pop-in banners for Sub / Raid / Bits events (cap 5).
    event_toasts: Vec<EventToast>,
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

        vis.widgets.hovered.weak_bg_fill = t::BG_RAISED;
        vis.widgets.hovered.bg_fill = t::BG_RAISED;
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
            loading_screen: LoadingScreen::default(),
            stream_statuses: HashMap::new(),
            stream_status_fetched: HashMap::new(),
            event_toasts: Vec::new(),
        }
    }

    fn drain_events(&mut self, ctx: &Context) -> u32 {
        let mut count = 0u32;
        while let Ok(evt) = self.event_rx.try_recv() {
            self.apply_event(evt, ctx);
            count += 1;
        }
        count
    }

    fn apply_event(&mut self, evt: AppEvent, ctx: &Context) {
        // Feed the loading screen before the main state update.
        match &evt {
            AppEvent::ConnectionStateChanged { state } => {
                use crust_core::events::ConnectionState;
                match state {
                    ConnectionState::Connecting | ConnectionState::Reconnecting { .. } =>
                        self.loading_screen.on_event(LoadEvent::Connecting),
                    ConnectionState::Connected =>
                        self.loading_screen.on_event(LoadEvent::Connected),
                    _ => {}
                }
            }
            AppEvent::Authenticated { username, .. } =>
                self.loading_screen.on_event(LoadEvent::Authenticated { username: username.clone() }),
            AppEvent::ChannelJoined { channel } =>
                self.loading_screen.on_event(LoadEvent::ChannelJoined { channel: channel.as_str().to_owned() }),
            AppEvent::EmoteCatalogUpdated { emotes } =>
                self.loading_screen.on_event(LoadEvent::CatalogLoaded { count: emotes.len() }),
            AppEvent::HistoryLoaded { channel, messages } =>
                self.loading_screen.on_event(LoadEvent::HistoryLoaded {
                    channel: channel.as_str().to_owned(),
                    count: messages.len(),
                }),
            AppEvent::ChannelEmotesLoaded { channel, count } =>
                self.loading_screen.on_event(LoadEvent::ChannelEmotesLoaded {
                    channel: channel.as_str().to_owned(),
                    count: *count,
                }),
            AppEvent::ImagePrefetchQueued { count } =>
                self.loading_screen.on_event(LoadEvent::ImagePrefetchQueued { count: *count }),
            AppEvent::EmoteImageReady { .. } =>
                self.loading_screen.on_event(LoadEvent::EmoteImageReady),
            _ => {}
        }

        match evt {
            AppEvent::ConnectionStateChanged { state } => {
                self.state.connection = state;
            }
            AppEvent::ChannelJoined { channel } => {
                self.state.join_channel(channel.clone());
                // Kick off an immediate stream-status fetch for the new channel.
                // Stamp now so the periodic refresh knows to wait 60 s before
                // retrying — but if the fetch silently fails the retry still fires.
                let login = channel.as_str().to_lowercase();
                self.stream_status_fetched.insert(login.clone(), std::time::Instant::now());
                self.send_cmd(AppCommand::FetchUserProfile { login });
            }
            AppEvent::ChannelParted { channel } => {
                self.state.leave_channel(&channel);
            }
            AppEvent::MessageReceived { channel, message } => {
                let is_active = self.state.active_channel.as_ref() == Some(&channel);

                // Generate a short-lived event toast for high-visibility events.
                if self.event_toasts.len() < 5 {
                    // Only pop banners for the channel the user is watching.
                    let maybe_toast: Option<EventToast> = if !is_active { None } else { match &message.msg_kind {
                        MsgKind::Sub { display_name, months, is_gift, plan, .. } => {
                            let text = if *is_gift {
                                format!("🎁  {} received a gifted {} sub!", display_name, plan)
                            } else if *months <= 1 {
                                format!("⭐  {} just subscribed with {}!", display_name, plan)
                            } else {
                                format!("⭐  {} resubscribed x{}!", display_name, months)
                            };
                            Some(EventToast {
                                text,
                                hue: Color32::from_rgb(255, 215, 0),
                                born: std::time::Instant::now(),
                            })
                        }
                        MsgKind::Raid { display_name, viewer_count } => Some(EventToast {
                            text: format!("🚀  {} is raiding with {} viewers!", display_name, viewer_count),
                            hue: Color32::from_rgb(100, 200, 255),
                            born: std::time::Instant::now(),
                        }),
                        MsgKind::Bits { amount } if *amount >= 100 => Some(EventToast {
                            text: format!("💎  {} cheered {} bits!", message.sender.display_name, amount),
                            hue: Color32::from_rgb(255, 160, 50),
                            born: std::time::Instant::now(),
                        }),
                        _ => None,
                    } };
                    if let Some(toast) = maybe_toast {
                        if self.event_toasts.len() >= 5 { self.event_toasts.remove(0); }
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
            AppEvent::EmoteImageReady { uri, width, height, raw_bytes } => {
                // Stub events (empty bytes) are emitted by failed fetches just
                // to advance the loading-screen image counter; skip actual insert.
                if !raw_bytes.is_empty() {
                    let byte_len = raw_bytes.len();
                    self.emote_bytes
                        .entry(uri)
                        .or_insert_with(|| {
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
                        ch.messages.front()
                            .and_then(|m| m.server_id.clone())
                            .or_else(|| messages.last().and_then(|m| m.server_id.clone()))
                    } else {
                        None
                    };

                    ch.prepend_history(messages);

                    if let Some(sid) = seam_id {
                        let scroll_key = egui::Id::new("ml_scroll_to")
                            .with(channel.as_str());
                        ctx.data_mut(|d| d.insert_temp(scroll_key, sid));
                    }
                }
            }
            AppEvent::UserProfileLoaded { profile } => {
                // Cache stream status.
                let login = profile.login.to_lowercase();
                self.stream_statuses.insert(login.clone(), StreamStatusInfo {
                    is_live:  profile.is_live,
                    title:    profile.stream_title.clone(),
                    game:     profile.stream_game.clone(),
                    viewers:  profile.stream_viewers,
                });
                self.stream_status_fetched.insert(login, std::time::Instant::now());
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
            AppEvent::UserMessagesCleared { channel, login } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.delete_messages_from(&login);
                }
            }
            AppEvent::UserStateUpdated { channel, is_mod, .. } => {
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
            AppEvent::LinkPreviewReady { url, title, description, thumbnail_url } => {
                self.link_previews.insert(url, LinkPreview {
                    title,
                    description,
                    thumbnail_url,
                    fetched: true,
                });
            }
            AppEvent::AccountListUpdated { accounts, active, default } => {
                self.state.accounts = accounts.clone();
                self.login_dialog.update_accounts(accounts, active, default);
            }
            AppEvent::ChannelEmotesLoaded { .. } => {
                // Handled in the loading-screen pre-pass above; nothing else to do.
            }
            AppEvent::ImagePrefetchQueued { .. } => {
                // Handled in the loading-screen pre-pass above; nothing else to do.
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
        let has_live_channel = self.state.active_channel.as_ref()
            .and_then(|ch| self.stream_statuses.get(&ch.as_str().to_lowercase()))
            .map(|s| s.is_live)
            .unwrap_or(false);
        let has_live_sidebar = self.stream_statuses.values().any(|s| s.is_live);
        let repaint_ms = if has_live_channel || has_live_sidebar || !self.event_toasts.is_empty() {
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
        if let Some(ref ch) = self.state.active_channel.clone() {
            if let Some(ch_state) = self.state.channels.get(ch) {
                self.analytics_panel.tick(ch_state);
            }
        }

        // Periodic stream-status refresh: re-fetch every 60 s per channel.
        const STREAM_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);
        let stale_channels: Vec<String> = self.state.channel_order.iter()
            .map(|ch| ch.as_str().to_lowercase())
            .filter(|login| {
                self.stream_status_fetched
                    .get(login)
                    .map(|t| t.elapsed() >= STREAM_REFRESH)
                    .unwrap_or(true) // no entry at all → fetch immediately
            })
            .collect();
        for login in stale_channels {
            self.stream_status_fetched.insert(login.clone(), std::time::Instant::now());
            self.send_cmd(AppCommand::FetchUserProfile { login });
        }

        // Render profile popup and dispatch any moderation action.
        if let Some(action) = self.user_profile_popup.show(ctx, &self.emote_bytes) {
            match action {
                PopupAction::Timeout { channel, login, user_id, seconds, reason } => {
                    self.send_cmd(AppCommand::TimeoutUser { channel, login, user_id, seconds, reason });
                }
                PopupAction::Ban { channel, login, user_id, reason } => {
                    self.send_cmd(AppCommand::BanUser { channel, login, user_id, reason });
                }
                PopupAction::Unban { channel, login, user_id } => {
                    self.send_cmd(AppCommand::UnbanUser { channel, login, user_id });
                }
            }
        }

        // -- Dialogs -----------------------------------------------------------
        if let Some(ch) = self.join_dialog.show(ctx) {
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

        // -- Top bar -----------------------------------------------------------
        TopBottomPanel::top("status_bar")
            .exact_height(36.0)
            .frame(
                Frame::new()
                    .fill(t::BG_SURFACE)
                    .inner_margin(t::BAR_MARGIN)
                    .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
            )
            .show(ctx, |ui| {
                let bar_width = ui.available_width();
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                    // App logotype
                    ui.label(
                        RichText::new("crust")
                            .font(egui::FontId::proportional(15.0))
                            .strong()
                            .color(t::ACCENT),
                    );

                    ui.separator();

                    // Connection indicator
                    let dot_r = 4.5_f32;
                    let (dot_rect, _) = ui.allocate_exact_size(
                        egui::vec2(dot_r * 2.0 + 4.0, dot_r * 2.0),
                        egui::Sense::hover(),
                    );
                    let (dot_color, conn_label) = connection_indicator(
                        &self.state.connection,
                        self.state.auth.logged_in,
                    );
                    ui.painter().circle_filled(dot_rect.center(), dot_r, dot_color);
                    // Hide connection label text at narrow widths; dot is sufficient
                    if bar_width > 500.0 {
                        ui.label(
                            RichText::new(conn_label)
                                .font(t::small())
                                .color(t::TEXT_SECONDARY),
                        );
                    }

                    ui.separator();

                    // Join channel button
                    if ui
                        .add_sized(
                            [72.0, t::BAR_H],
                            egui::Button::new(RichText::new("+ Join").font(t::small())),
                        )
                        .on_hover_text("Join a Twitch channel")
                        .clicked()
                    {
                        self.join_dialog.toggle();
                    }

                    ui.separator();

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

                    // Right-side items
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                            // Perf overlay toggle - hide at narrow widths
                            if bar_width > 650.0 {
                                let perf_label =
                                    if self.perf.visible { "Perf: on" } else { "Perf: off" };
                                if ui
                                    .add_sized(
                                        [66.0, t::BAR_H],
                                        egui::Button::new(
                                            RichText::new(perf_label).font(t::small()),
                                        ),
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
                                        egui::Button::new(
                                            RichText::new(stats_label).font(t::small()),
                                        ),
                                    )
                                    .on_hover_text("Toggle chatter analytics")
                                    .clicked()
                                {
                                    self.analytics_visible = !self.analytics_visible;
                                }

                                ui.separator();
                            }

                            // Emote count - hide at narrow widths
                            if bar_width > 550.0 {
                                ui.label(
                                    RichText::new(format!(
                                        "{} emotes",
                                        self.emote_bytes.len()
                                    ))
                                    .font(t::small())
                                    .color(t::TEXT_MUTED),
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
                                    .unwrap_or("User");
                                let initial = name
                                    .chars()
                                    .next()
                                    .unwrap_or('?')
                                    .to_uppercase()
                                    .next()
                                    .unwrap_or('?');

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
                                    let bg = if resp.hovered() { t::BG_RAISED } else { t::BG_SURFACE };
                                    let border = if resp.hovered() { t::BORDER_ACCENT } else { t::BORDER_SUBTLE };
                                    ui.painter().rect(rect, t::RADIUS, bg, egui::Stroke::new(1.0, border), egui::StrokeKind::Outside);

                                    // Avatar circle
                                    let avatar_r = btn_h * 0.34;
                                    let avatar_c = egui::pos2(rect.left() + btn_h * 0.5, rect.center().y);

                                    // Try to render the real avatar image; fall back to initial letter.
                                    let avatar_bytes = self.state.auth.avatar_url.as_deref()
                                        .and_then(|url| self.emote_bytes.get(url).map(|(_, _, raw)| (url, raw.clone())));

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
                                        ui.painter().circle_filled(avatar_c, avatar_r, t::ACCENT_DIM);
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
                            } else if ui
                                .add_sized(
                                    [68.0, t::BAR_H],
                                    egui::Button::new(
                                        RichText::new(
                                            if self.state.accounts.is_empty() { "Log in" } else { "Accounts" }
                                        ).font(t::small()),
                                    ),
                                )
                                .on_hover_text("Log in with a Twitch OAuth token")
                                .clicked()
                            {
                                self.login_dialog.toggle();
                            }
                        },
                    );
                });
            });

        // -- Stream info bar --------------------------------------------------
        // Shows live/offline status, viewer count and stream title for the
        // currently active channel. Hidden when no channel is active.
        if let Some(active_ch) = self.state.active_channel.as_ref() {
            let login = active_ch.as_str().to_lowercase();
            let status = self.stream_statuses.get(&login).cloned();
            // Subtle red tint on the bar background when the channel is live.
            let bar_is_live = status.as_ref().map(|s| s.is_live).unwrap_or(false);
            let bar_fill = if bar_is_live { Color32::from_rgb(24, 14, 14) } else { t::BG_SURFACE };
            TopBottomPanel::top("stream_info_bar")
                .exact_height(22.0)
                .frame(
                    Frame::new()
                        .fill(bar_fill)
                        .inner_margin(egui::Margin::symmetric(10, 2))
                        .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
                )
                .show(ctx, |ui| {
                    // Thin accent stripe on the very left edge when live.
                    if bar_is_live {
                        let br = ui.max_rect();
                        let strip = egui::Rect::from_min_size(br.left_top(), egui::vec2(3.0, br.height()));
                        ui.painter().rect_filled(strip, 0.0, t::RED);
                    }
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        match status {
                            None => {
                                // Not fetched yet – show the channel name only.
                                ui.label(
                                    RichText::new(format!("#{}", active_ch.as_str()))
                                        .font(t::small())
                                        .color(t::TEXT_SECONDARY),
                                );
                            }
                            Some(s) => {
                                let t_anim = ui.input(|i| i.time) as f32;
                                let status_text = if s.is_live { "LIVE" } else { "OFFLINE" };
                                let label_col = if s.is_live { t::RED } else { t::TEXT_MUTED };

                                // Animated dot: outer glow + inner solid circle when live.
                                if s.is_live {
                                    let pulse = (t_anim * 2.5).sin() * 0.5 + 0.5;
                                    let base_r = 3.5_f32;
                                    let inner_r = base_r - 0.5 + pulse * 0.8;
                                    let glow_r  = base_r + 1.5 + pulse * 1.5;
                                    let alloc   = (glow_r + 1.0) * 2.0;
                                    let (dot_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(alloc, alloc),
                                        egui::Sense::hover(),
                                    );
                                    let c = dot_rect.center();
                                    let glow_a = (28.0 + pulse * 55.0) as u8;
                                    ui.painter().circle_filled(c, glow_r, Color32::from_rgba_unmultiplied(220, 65, 65, glow_a));
                                    ui.painter().circle_filled(c, inner_r, t::RED);
                                } else {
                                    let dot_r = 3.5_f32;
                                    let (dot_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(dot_r * 2.0, dot_r * 2.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().circle_filled(dot_rect.center(), dot_r, t::TEXT_MUTED);
                                }

                                // Status label
                                ui.label(
                                    RichText::new(status_text)
                                        .font(t::small())
                                        .color(label_col)
                                        .strong(),
                                );

                                ui.separator();

                                // Channel name
                                ui.label(
                                    RichText::new(format!("#{}", active_ch.as_str()))
                                        .font(t::small())
                                        .color(t::TEXT_PRIMARY),
                                );

                                // Viewer count (live only)
                                if s.is_live {
                                    if let Some(viewers) = s.viewers {
                                        ui.separator();
                                        ui.label(
                                            RichText::new(format!("👁 {}", fmt_viewers(viewers)))
                                                .font(t::small())
                                                .color(t::TEXT_SECONDARY),
                                        );
                                    }

                                    // Game
                                    if let Some(ref game) = s.game {
                                        if !game.is_empty() {
                                            ui.separator();
                                            ui.label(
                                                RichText::new(game.as_str())
                                                    .font(t::small())
                                                    .color(t::TEXT_SECONDARY),
                                            );
                                        }
                                    }

                                    // Stream title – right side, truncated
                                    if let Some(ref title) = s.title {
                                        if !title.is_empty() {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    let max_w = ui.available_width();
                                                    let galley = ui.painter().layout(
                                                        title.clone(),
                                                        t::small(),
                                                        t::TEXT_MUTED,
                                                        max_w,
                                                    );
                                                    let size = galley.size();
                                                    let (r, _) = ui.allocate_exact_size(
                                                        egui::vec2(max_w.min(size.x), size.y),
                                                        egui::Sense::hover(),
                                                    );
                                                    let clip = egui::Rect::from_min_size(
                                                        r.min,
                                                        egui::vec2(max_w, size.y),
                                                    );
                                                    ui.painter().with_clip_rect(clip).galley(
                                                        r.min,
                                                        galley,
                                                        t::TEXT_MUTED,
                                                    );
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    });
                });
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
                                        let label = if mentions > 0 {
                                            format!("{} ●{}", ch.as_str(), mentions)
                                        } else if unread > 0 {
                                            format!("{} {}", ch.as_str(), unread)
                                        } else {
                                            ch.as_str().to_owned()
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
                                                RichText::new(&label)
                                                    .font(t::small())
                                                    .color(fg),
                                            )
                                            .fill(bg),
                                        );

                                        if resp.clicked() {
                                            ch_selected = Some(ch.clone());
                                        }

                                        // Context menu: close channel
                                        resp.context_menu(|ui| {
                                            if ui
                                                .button(
                                                    RichText::new("Close channel")
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
                // at least 350 px - prevents chat from being hidden on narrow windows.
                let sidebar_max = (ctx.screen_rect().width() - 350.0)
                    .clamp(t::SIDEBAR_MIN_W, t::SIDEBAR_MAX_W);

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

                        let live_map: HashMap<String, bool> = self.stream_statuses
                            .iter()
                            .map(|(k, v)| (k.clone(), v.is_live))
                            .collect();
                        let mut list = ChannelList {
                            channels: &self.state.channel_order,
                            active: self.state.active_channel.as_ref(),
                            channel_states: &self.state.channels,
                            live_channels: Some(&live_map),
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
            self.send_cmd(AppCommand::LeaveChannel { channel: ch.clone() });
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
            .frame(Frame::new().fill(t::BG_BASE).inner_margin(Margin { left: 6, right: 0, top: 0, bottom: 0 }))
            .show(ctx, |ui| {
                if let Some(active_ch) = self.state.active_channel.clone() {
                    // Input tray pinned to bottom
                    TopBottomPanel::bottom("chat_input_panel")
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
                                let reply_to_msg_id = self.pending_reply
                                    .as_ref()
                                    .map(|r| r.parent_msg_id.clone());
                                self.pending_reply = None;
                                let is_mod = self.state.channels
                                    .get(&active_ch).map(|c| c.is_mod).unwrap_or(false);
                                // Broadcaster has full mod powers in their own channel.
                                let is_broadcaster = self.state.auth.username.as_deref()
                                    .map(|u| u.eq_ignore_ascii_case(active_ch.as_str()))
                                    .unwrap_or(false);
                                let can_moderate = is_mod || is_broadcaster;
                                let chatters_count = self.state.channels
                                    .get(&active_ch).map(|c| c.chatters.len()).unwrap_or(0);
                                if let Some(cmd) = parse_slash_command(
                                    &text, &active_ch, reply_to_msg_id.clone(),
                                    can_moderate, chatters_count,
                                ) {
                                    // Some slash commands manipulate the popup directly.
                                    if let AppCommand::ShowUserCard { ref login, ref channel } = cmd {
                                        self.user_profile_popup.set_loading(login, vec![], Some(channel.clone()), can_moderate);
                                    }
                                    self.send_cmd(cmd);
                                } else {
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
                        if !self.chat_input_buf.is_empty()
                            && !self.chat_input_buf.ends_with(' ')
                        {
                            self.chat_input_buf.push(' ');
                        }
                        self.chat_input_buf.push_str(&code);
                        self.chat_input_buf.push(' ');
                    }

                    // Messages above the input
                    if let Some(state) = self.state.channels.get(&active_ch) {
                        let is_broadcaster = self.state.auth.username.as_deref()
                            .map(|u| u.eq_ignore_ascii_case(active_ch.as_str()))
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
        self.event_toasts.retain(|t| t.born.elapsed().as_secs_f32() < 5.0);
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
            let slide_x = if age < 0.25 { (1.0 - age / 0.25) * 28.0 } else { 0.0 };
            egui::Area::new(egui::Id::new("event_toast").with(i))
                .anchor(
                    egui::Align2::RIGHT_TOP,
                    egui::vec2(-14.0 - slide_x, 58.0 + i as f32 * 50.0),
                )
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    let border_col = Color32::from_rgba_unmultiplied(
                        toast.hue.r(), toast.hue.g(), toast.hue.b(),
                        (160.0 * opacity) as u8,
                    );
                    let fill_col = Color32::from_rgba_unmultiplied(18, 16, 26, (225.0 * opacity) as u8);
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
    }
}

// Helper functions

fn connection_indicator(
    state: &ConnectionState,
    logged_in: bool,
) -> (Color32, &'static str) {
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

fn install_system_fallback_fonts(ctx: &Context) {
    // Ordered by Unicode coverage breadth. We load ALL that exist and push
    // them as fallbacks so glyphs missing in one font are found in the next.
    const CANDIDATES: &[(&str, &str)] = &[
        // DejaVu - good Latin/Greek/Cyrillic/symbols coverage
        ("dejavu", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        ("dejavu", "/usr/share/fonts/TTF/DejaVuSans.ttf"),
        // Noto Sans - broad multilingual coverage
        ("noto", "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf"),
        ("noto", "/usr/share/fonts/noto/NotoSans-Regular.ttf"),
        ("noto", "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
        ("noto", "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc"),
        // Noto Emoji - colour emoji fallback
        ("noto_emoji", "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf"),
        ("noto_emoji", "/usr/share/fonts/noto/NotoColorEmoji.ttf"),
        ("noto_emoji", "/usr/share/fonts/noto/NotoEmoji-Regular.ttf"),
        // GNU Unifont - near-complete BMP coverage as last resort
        ("unifont", "/usr/share/fonts/truetype/unifont/unifont.ttf"),
        ("unifont", "/usr/share/fonts/unifont/unifont.ttf"),
        ("unifont", "/usr/share/fonts/misc/unifont.ttf"),
        // GNU FreeFont
        ("freesans", "/usr/share/fonts/gnu-free/FreeSans.ttf"),
        // macOS
        ("arial_unicode", "/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
        // Windows
        ("seguisym", "C:\\Windows\\Fonts\\seguisym.ttf"),
        ("arial",    "C:\\Windows\\Fonts\\arial.ttf"),
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
            fonts.font_data.insert(key.clone(), egui::FontData::from_owned(bytes).into());
            fonts.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(key.clone());
            fonts.families
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
            let msg = "\
Crust built-in commands\n\
  /help               show this message\n\
  /clearmessages      clear chat view (local only)\n\
  /chatters           show connected chatter count\n\
  /fakemsg <text>     inject a local-only message\n\
  /openurl <url>      open a URL in the system browser\n\
  /popout [channel]   open Twitch popout chat in browser\n\
  /user <user>        open twitch.tv/<user> in browser\n\
  /usercard <user>    open in-app user profile\n\
  /streamlink [ch]    open stream in streamlink\n\
  /banid <id>         ban a user by Twitch user ID\n\
  /w <user> <msg>     send a Twitch whisper\n\
Any other /command is forwarded directly to Twitch.".to_owned();
            Some(AppCommand::InjectLocalMessage { channel: channel.clone(), text: msg })
        }

        "clearmessages" => {
            Some(AppCommand::ClearLocalMessages { channel: channel.clone() })
        }

        "chatters" => {
            let msg = format!("There are {} chatters currently connected.", chatters_count);
            Some(AppCommand::InjectLocalMessage { channel: channel.clone(), text: msg })
        }

        "fakemsg" if !rest.is_empty() => {
            // Inject the raw text as a local system notice (no IRC parsing).
            Some(AppCommand::InjectLocalMessage {
                channel: channel.clone(),
                text: rest.to_owned(),
            })
        }

        "openurl" if !rest.is_empty() => {
            Some(AppCommand::OpenUrl { url: rest.to_owned() })
        }

        // /popout [channel]  - opens Twitch's popout chat in the browser.
        "popout" => {
            let target = if rest.is_empty() { channel.as_str() } else { rest };
            let url = format!("https://www.twitch.tv/popout/{target}/chat?popout=");
            Some(AppCommand::OpenUrl { url })
        }

        // /user <user> [channel]  - open twitch.tv/<user> in browser.
        "user" => {
            let login = rest.split_whitespace().next().unwrap_or(channel.as_str());
            let url = format!("https://twitch.tv/{login}");
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
            let target = if rest.is_empty() { channel.as_str() } else { rest };
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
