use std::collections::HashMap;
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{AppCommand, AppEvent, ConnectionState, LinkPreview},
    model::{ChannelId, EmoteCatalogEntry, ReplyInfo},
    AppState,
};

use crate::perf::PerfOverlay;
use crate::theme as t;
use crate::widgets::{
    channel_list::ChannelList,
    chat_input::ChatInput,
    emote_picker::EmotePicker,
    join_dialog::JoinDialog,
    login_dialog::{LoginAction, LoginDialog},
    message_list::MessageList,
    user_profile_popup::{PopupAction, UserProfilePopup},
};

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
    /// Running total of raw emote bytes — updated incrementally on EmoteImageReady
    /// so we don't iterate the entire map every frame.
    emote_ram_bytes: usize,
    /// Chat message history for Up/Down arrow recall.
    message_history: Vec<String>,
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
        match evt {
            AppEvent::ConnectionStateChanged { state } => {
                self.state.connection = state;
            }
            AppEvent::ChannelJoined { channel } => {
                self.state.join_channel(channel);
            }
            AppEvent::ChannelParted { channel } => {
                self.state.leave_channel(&channel);
            }
            AppEvent::MessageReceived { channel, message } => {
                let is_active = self.state.active_channel.as_ref() == Some(&channel);
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
                let byte_len = raw_bytes.len();
                self.emote_bytes
                    .entry(uri)
                    .or_insert_with(|| {
                        self.emote_ram_bytes += byte_len;
                        (width, height, Arc::from(raw_bytes.as_slice()))
                    });
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
        // interval — user interactions (mouse, keyboard, scroll) already
        // trigger repaints via egui, and GIF animation is driven by the
        // image loaders internally.
        if had_events {
            ctx.request_repaint(); // drain the next batch ASAP
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        self.perf.emote_count = self.emote_bytes.len();
        self.perf.emote_ram_kb = self.emote_ram_bytes / 1024;
        self.perf.record_frame(events, had_events);
        self.perf.show(ctx);

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

                    // Right-side items
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                            // Perf overlay toggle — hide at narrow widths
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
                            }

                            // Emote count — hide at narrow widths
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

        // -- Left sidebar ------------------------------------------------------
        // Dynamically cap sidebar width so the central panel always gets
        // at least 350 px — prevents chat from being hidden on narrow windows.
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

                let mut list = ChannelList {
                    channels: &self.state.channel_order,
                    active: self.state.active_channel.as_ref(),
                    channel_states: &self.state.channels,
                };
                let res = list.show(ui);
                if let Some(ch) = res.selected {
                    // Clear unread counters when the user opens the channel.
                    if let Some(state) = self.state.channels.get_mut(&ch) {
                        state.mark_read();
                    }
                    self.state.active_channel = Some(ch);
                }
                if let Some(ch) = res.closed {
                    self.send_cmd(AppCommand::LeaveChannel { channel: ch.clone() });
                    self.state.leave_channel(&ch);
                }
                if let Some(new_order) = res.reordered {
                    self.state.channel_order = new_order;
                }
            });

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

fn install_system_fallback_fonts(ctx: &Context) {
    // Ordered by Unicode coverage breadth. We load ALL that exist and push
    // them as fallbacks so glyphs missing in one font are found in the next.
    const CANDIDATES: &[(&str, &str)] = &[
        // DejaVu — good Latin/Greek/Cyrillic/symbols coverage
        ("dejavu", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        ("dejavu", "/usr/share/fonts/TTF/DejaVuSans.ttf"),
        // Noto Sans — broad multilingual coverage
        ("noto", "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf"),
        ("noto", "/usr/share/fonts/noto/NotoSans-Regular.ttf"),
        ("noto", "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
        ("noto", "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc"),
        // Noto Emoji — colour emoji fallback
        ("noto_emoji", "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf"),
        ("noto_emoji", "/usr/share/fonts/noto/NotoColorEmoji.ttf"),
        ("noto_emoji", "/usr/share/fonts/noto/NotoEmoji-Regular.ttf"),
        // GNU Unifont — near-complete BMP coverage as last resort
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

        // /popout [channel]  — opens Twitch's popout chat in the browser.
        "popout" => {
            let target = if rest.is_empty() { channel.as_str() } else { rest };
            let url = format!("https://www.twitch.tv/popout/{target}/chat?popout=");
            Some(AppCommand::OpenUrl { url })
        }

        // /user <user> [channel]  — open twitch.tv/<user> in browser.
        "user" => {
            let login = rest.split_whitespace().next().unwrap_or(channel.as_str());
            let url = format!("https://twitch.tv/{login}");
            Some(AppCommand::OpenUrl { url })
        }

        // /usercard <user> [channel]  — show our profile popup.
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

        // /streamlink [channel]  — open stream in streamlink via URL scheme.
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

        // /w <user> <message>  — Twitch whisper (pass straight through).
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
