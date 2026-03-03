#!/usr/bin/env python3
"""Writes the new themed app.rs."""
import os

DEST = os.path.join(os.path.dirname(__file__), "../crates/ui/src/app.rs")

CONTENT = """\
use std::collections::HashMap;
use std::sync::Arc;

use egui::{CentralPanel, Color32, Context, Frame, Margin, RichText, SidePanel, TopBottomPanel};
use tokio::sync::mpsc;
use tracing::warn;

use crust_core::{
    events::{AppCommand, AppEvent, ConnectionState},
    model::EmoteCatalogEntry,
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
};

// \u2500\u2500\u2500 CrustApp \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

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
}

impl CrustApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cmd_tx: mpsc::Sender<AppCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // \u2500\u2500 Visuals \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
        let mut vis = egui::Visuals::dark();
        vis.override_text_color = Some(t::TEXT_PRIMARY);
        vis.panel_fill = t::BG_BASE;
        vis.window_fill = t::BG_DIALOG;
        vis.extreme_bg_color = t::BG_BASE;

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
                self.emote_bytes
                    .entry(uri)
                    .or_insert_with(|| (width, height, Arc::from(raw_bytes.as_slice())));
            }
            AppEvent::EmoteCatalogUpdated { mut emotes } => {
                emotes.sort_by(|a, b| a.code.to_lowercase().cmp(&b.code.to_lowercase()));
                self.emote_catalog = emotes;
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

// \u2500\u2500\u2500 eframe::App \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

impl eframe::App for CrustApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let events = self.drain_events(ctx);
        let had_events = events > 0;
        if had_events {
            ctx.request_repaint();
        }

        self.perf.emote_count = self.emote_bytes.len();
        self.perf.emote_ram_kb = self
            .emote_bytes
            .values()
            .map(|(_, _, raw)| raw.len())
            .sum::<usize>()
            / 1024;
        self.perf.record_frame(events, had_events);
        self.perf.show(ctx);

        // \u2500\u2500 Dialogs \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
        if let Some(ch) = self.join_dialog.show(ctx) {
            self.send_cmd(AppCommand::JoinChannel { channel: ch });
        }
        if let Some(action) = self.login_dialog.show(
            ctx,
            self.state.auth.logged_in,
            self.state.auth.username.as_deref(),
        ) {
            match action {
                LoginAction::Login(token) => self.send_cmd(AppCommand::Login { token }),
                LoginAction::Logout => self.send_cmd(AppCommand::Logout),
            }
        }

        // \u2500\u2500 Top bar \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
        TopBottomPanel::top("status_bar")
            .exact_height(36.0)
            .frame(
                Frame::new()
                    .fill(t::BG_SURFACE)
                    .inner_margin(t::BAR_MARGIN)
                    .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                    ui.label(
                        RichText::new("crust")
                            .font(egui::FontId::proportional(15.0))
                            .strong()
                            .color(t::ACCENT),
                    );

                    ui.separator();

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
                    ui.label(
                        RichText::new(conn_label)
                            .font(t::small())
                            .color(t::TEXT_SECONDARY),
                    );

                    ui.separator();

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

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;

                            let perf_label =
                                if self.perf.visible { "\u26a1 on" } else { "\u26a1 off" };
                            if ui
                                .add_sized(
                                    [58.0, t::BAR_H],
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

                            ui.label(
                                RichText::new(format!(
                                    "{} emotes",
                                    self.emote_bytes.len()
                                ))
                                .font(t::small())
                                .color(t::TEXT_MUTED),
                            );

                            ui.separator();

                            if self.state.auth.logged_in {
                                let name = self
                                    .state
                                    .auth
                                    .username
                                    .as_deref()
                                    .unwrap_or("User");
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(format!("\ud83d\udc64 {name}"))
                                                .font(t::small()),
                                        )
                                        .min_size(egui::vec2(90.0, t::BAR_H)),
                                    )
                                    .on_hover_text("Account settings")
                                    .clicked()
                                {
                                    self.login_dialog.toggle();
                                }
                            } else if ui
                                .add_sized(
                                    [80.0, t::BAR_H],
                                    egui::Button::new(
                                        RichText::new("\ud83d\udd11 Login").font(t::small()),
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

        // \u2500\u2500 Left sidebar \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
        SidePanel::left("channel_list")
            .resizable(true)
            .default_width(t::SIDEBAR_W)
            .min_width(t::SIDEBAR_MIN_W)
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
                    self.state.active_channel = Some(ch);
                }
                if let Some(ch) = res.closed {
                    self.send_cmd(AppCommand::LeaveChannel { channel: ch.clone() });
                    self.state.leave_channel(&ch);
                }
            });

        // \u2500\u2500 Central panel \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
        CentralPanel::default()
            .frame(Frame::new().fill(t::BG_BASE).inner_margin(Margin::ZERO))
            .show(ctx, |ui| {
                if let Some(active_ch) = self.state.active_channel.clone() {
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
                            };
                            let result = chat.show(ui, &mut self.chat_input_buf);
                            if let Some(text) = result.send {
                                self.send_cmd(AppCommand::SendMessage {
                                    channel: active_ch.clone(),
                                    text,
                                });
                            }
                            if result.toggle_emote_picker {
                                self.emote_picker.toggle();
                            }
                        });

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

                    if let Some(state) = self.state.channels.get(&active_ch) {
                        MessageList::new(
                            &state.messages,
                            &self.emote_bytes,
                            &self.cmd_tx,
                            &active_ch,
                        )
                        .show(ui);
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(
                                "Click \\\"+ Join\\\" to open a Twitch channel.",
                            )
                            .color(t::TEXT_MUTED)
                            .font(t::body()),
                        );
                    });
                }
            });
    }
}

// \u2500\u2500\u2500 Helpers \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

fn connection_indicator(
    state: &ConnectionState,
    logged_in: bool,
) -> (Color32, &'static str) {
    match state {
        ConnectionState::Connected if logged_in => (t::GREEN, "Connected"),
        ConnectionState::Connected => (t::GREEN, "Connected (anon)"),
        ConnectionState::Connecting => (t::YELLOW, "Connecting\u2026"),
        ConnectionState::Reconnecting { .. } => (t::YELLOW, "Reconnecting\u2026"),
        ConnectionState::Disconnected => (t::RED, "Disconnected"),
        ConnectionState::Error(_) => (t::RED, "Error"),
    }
}

fn install_system_fallback_fonts(ctx: &Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/unifont/unifont.ttf",
        "/usr/share/fonts/gnu-free/FreeSans.ttf",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "C:\\\\Windows\\\\Fonts\\\\seguisym.ttf",
        "C:\\\\Windows\\\\Fonts\\\\arial.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            tracing::info!("Loaded fallback font: {path}");
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "fallback".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
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
    tracing::warn!("No system fallback font found; some Unicode glyphs may render as \u25a1");
}
"""

with open(DEST, "w", encoding="utf-8") as f:
    f.write(CONTENT)
print(f"Wrote {len(CONTENT)} chars to {DEST}")
