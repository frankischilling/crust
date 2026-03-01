use std::collections::HashMap;
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, SidePanel, TopBottomPanel};
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{AppCommand, AppEvent, ConnectionState},
    AppState,
};

use crate::perf::PerfOverlay;
use crate::widgets::{
    channel_list::ChannelList,
    chat_input::ChatInput,
    join_dialog::JoinDialog,
    login_dialog::{LoginAction, LoginDialog},
    message_list::MessageList,
};

// ─── CrustApp ────────────────────────────────────────────────────────────────

/// Top-level egui application — Twitch chat viewer with login support.
pub struct CrustApp {
    pub state: AppState,
    cmd_tx: mpsc::Sender<AppCommand>,
    event_rx: mpsc::Receiver<AppEvent>,

    // Raw image bytes: url → (width, height, raw_bytes)
    emote_bytes: HashMap<String, (u32, u32, Arc<[u8]>)>,

    // UI state
    join_dialog: JoinDialog,
    login_dialog: LoginDialog,
    /// Per-channel chat input buffer.
    chat_input_buf: String,

    // Performance overlay (debug)
    perf: PerfOverlay,
}

impl CrustApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd_tx: mpsc::Sender<AppCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Self {
        // Install image decoders (PNG, GIF, WebP, etc.)
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // Use dark visuals
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        // Tune global spacing/style for a tighter, chat-friendly layout
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(4.0, 3.0);
        style.spacing.button_padding = egui::vec2(6.0, 2.0);
        style.spacing.window_margin = egui::Margin::same(8);
        // Slightly softer corners
        style.visuals.window_corner_radius = egui::CornerRadius::same(6);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(6);
        cc.egui_ctx.set_style(style);

        // ── Load system fallback fonts ──────────────────────────────────
        // egui's built-in font lacks many Unicode blocks commonly used in
        // Twitch chat (Braille art, box-drawing, block elements, CJK, etc.).
        // We try well-known system fonts in priority order and add the first
        // one found as a fallback to both Proportional and Monospace families.
        install_system_fallback_fonts(&cc.egui_ctx);

        Self {
            state: AppState::default(),
            cmd_tx,
            event_rx,
            emote_bytes: HashMap::new(),
            join_dialog: JoinDialog::default(),
            login_dialog: LoginDialog::default(),
            chat_input_buf: String::new(),
            perf: PerfOverlay::default(),
        }
    }

    /// Process all pending events from the runtime.
    /// Returns the number of events handled (0 = nothing changed, skip repaint).
    fn drain_events(&mut self, ctx: &Context) -> u32 {
        let mut count = 0u32;
        while let Ok(evt) = self.event_rx.try_recv() {
            self.apply_event(evt, ctx);
            count += 1;
        }
        count
    }

    fn apply_event(&mut self, evt: AppEvent, _ctx: &Context) {
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
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.push_message(message);
                }
            }
            AppEvent::MessageDeleted { channel, server_id } => {
                if let Some(ch) = self.state.channels.get_mut(&channel) {
                    ch.delete_message(&server_id);
                }
            }
            AppEvent::SystemNotice(notice) => {
                tracing::debug!("[notice] {:?}", notice.text);
            }
            AppEvent::EmoteImageReady { uri, width, height, raw_bytes } => {
                // Store raw bytes; egui's loaders handle decoding + animation.
                self.emote_bytes
                    .entry(uri)
                    .or_insert_with(|| (width, height, Arc::from(raw_bytes.as_slice())));
            }
            AppEvent::Authenticated { username, user_id } => {
                self.state.auth.logged_in = true;
                self.state.auth.username = Some(username);
                self.state.auth.user_id = Some(user_id);
            }
            AppEvent::LoggedOut => {
                self.state.auth.logged_in = false;
                self.state.auth.username = None;
                self.state.auth.user_id = None;
            }
            AppEvent::Error { context, message } => {
                tracing::error!("[{context}] {message}");
            }
        }
    }

    fn send_cmd(&self, cmd: AppCommand) {
        if self.cmd_tx.try_send(cmd).is_err() {
            warn!("Command channel full/closed");
        }
    }
}

impl eframe::App for CrustApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Drain events; only request an extra repaint when something actually
        // changed (new messages, images loaded). Animated emotes schedule
        // their own repaints through egui's image animation system.
        let events = self.drain_events(ctx);
        let had_events = events > 0;
        if had_events {
            ctx.request_repaint();
        }

        // Update perf stats.
        self.perf.emote_count = self.emote_bytes.len();
        self.perf.emote_ram_kb = self.emote_bytes.values()
            .map(|(_, _, raw)| raw.len())
            .sum::<usize>() / 1024;
        self.perf.record_frame(events, had_events);
        self.perf.show(ctx);

        // ── Join dialog (modal) ─────────────────────────────────────────
        if let Some(ch) = self.join_dialog.show(ctx) {
            self.send_cmd(AppCommand::JoinChannel { channel: ch });
        }

        // ── Login dialog (modal) ────────────────────────────────────────
        if let Some(action) = self.login_dialog.show(
            ctx,
            self.state.auth.logged_in,
            self.state.auth.username.as_deref(),
        ) {
            match action {
                LoginAction::Login(token) => {
                    self.send_cmd(AppCommand::Login { token });
                }
                LoginAction::Logout => {
                    self.send_cmd(AppCommand::Logout);
                }
            }
        }

        // ── Status bar ──────────────────────────────────────────────────
        TopBottomPanel::top("status_bar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                // Connection indicator dot — vertically centred with the text
                let dot_r = 5.0f32;
                let (dot_rect, _) = ui.allocate_exact_size(
                    egui::vec2(dot_r * 2.0 + 4.0, dot_r * 2.0),
                    egui::Sense::hover(),
                );
                let (dot_color, label) = match &self.state.connection {
                    ConnectionState::Connected if self.state.auth.logged_in => {
                        (Color32::from_rgb(80, 200, 100), "Connected")
                    }
                    ConnectionState::Connected    => (Color32::from_rgb(80, 200, 100), "Connected (anonymous)"),
                    ConnectionState::Connecting   => (Color32::from_rgb(240, 200, 60), "Connecting…"),
                    ConnectionState::Reconnecting { .. } => (Color32::from_rgb(240, 200, 60), "Reconnecting…"),
                    ConnectionState::Disconnected => (Color32::from_rgb(200, 70, 70), "Disconnected"),
                    ConnectionState::Error(_)     => (Color32::from_rgb(200, 70, 70), "Error"),
                };
                ui.painter().circle_filled(dot_rect.center(), dot_r, dot_color);
                ui.label(egui::RichText::new(label).small());

                ui.separator();

                if ui.button("+ Join").clicked() {
                    self.join_dialog.toggle();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let perf_label = if self.perf.visible { "⚡ Perf ✓" } else { "⚡ Perf" };
                    if ui.small_button(perf_label).clicked() {
                        self.perf.visible = !self.perf.visible;
                    }

                    ui.separator();

                    let emote_count = self.emote_bytes.len();
                    ui.label(
                        egui::RichText::new(format!("{emote_count} emotes"))
                            .small()
                            .color(Color32::from_rgb(100, 100, 100)),
                    );

                    ui.separator();

                    // Login / Account button
                    if self.state.auth.logged_in {
                        let name = self.state.auth.username.as_deref().unwrap_or("User");
                        if ui.small_button(format!("👤 {name}")).clicked() {
                            self.login_dialog.toggle();
                        }
                    } else if ui.small_button("🔑 Login").clicked() {
                        self.login_dialog.toggle();
                    }
                });
            });
        });

        // ── Left sidebar: channel tabs ──────────────────────────────────
        SidePanel::left("channel_list")
            .resizable(true)
            .default_width(150.0)
            .min_width(100.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(8, 8)))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new("CHANNELS")
                        .small()
                        .strong()
                        .color(Color32::from_rgb(120, 120, 120)),
                );
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                let mut list = ChannelList {
                    channels: &self.state.channel_order,
                    active: self.state.active_channel.as_ref(),
                    channel_states: &self.state.channels,
                };
                let result = list.show(ui);

                if let Some(ch) = result.selected {
                    self.state.active_channel = Some(ch);
                }
                if let Some(ch) = result.closed {
                    self.send_cmd(AppCommand::LeaveChannel {
                        channel: ch.clone(),
                    });
                    self.state.leave_channel(&ch);
                }
            });

        // ── Message list (center) ───────────────────────────────────────
        CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(0)))
            .show(ctx, |ui| {
            if let Some(active_ch) = self.state.active_channel.clone() {
                // Chat input at the bottom
                TopBottomPanel::bottom("chat_input_panel")
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                        let chat = ChatInput {
                            channel: &active_ch,
                            logged_in: self.state.auth.logged_in,
                            username: self.state.auth.username.as_deref(),
                        };
                        let result = chat.show(ui, &mut self.chat_input_buf);
                        if let Some(text) = result.send {
                            self.send_cmd(AppCommand::SendMessage {
                                channel: active_ch.clone(),
                                text,
                            });
                        }
                    });

                // Messages above the input
                if let Some(state) = self.state.channels.get(&active_ch) {
                    MessageList::new(&state.messages, &self.emote_bytes, &self.cmd_tx, &active_ch).show(ui);
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Click \"+ Join\" to open a Twitch channel.");
                });
            }
        });
    }
}

// ─── Font helpers ────────────────────────────────────────────────────────────

/// Try well-known system font paths and add the first found as a fallback font.
/// This gives us Braille patterns, box-drawing, block elements, CJK, and many
/// other Unicode blocks that egui's built-in font doesn't cover.
fn install_system_fallback_fonts(ctx: &Context) {
    // Candidate fonts in priority order (most to least preferred).
    // These cover Braille (U+2800–U+28FF), Box Drawing, Block Elements,
    // Misc Symbols, and many more.
    const CANDIDATES: &[&str] = &[
        // Linux
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/unifont/unifont.ttf",
        "/usr/share/fonts/gnu-free/FreeSans.ttf",
        // macOS
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        // Windows
        "C:\\Windows\\Fonts\\seguisym.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ];

    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            tracing::info!("Loaded fallback font: {path}");
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "fallback".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            // Append as fallback to both font families
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.push("fallback".to_owned());
            }
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                list.push("fallback".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
    tracing::warn!("No system fallback font found; some Unicode glyphs may render as □");
}
