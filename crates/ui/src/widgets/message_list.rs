use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use egui::{
    Color32, Id, Label, LayerId, Order, RichText, ScrollArea, Ui, Vec2,
};
use tokio::sync::mpsc;

use crust_core::{
    events::AppCommand,
    model::{ChannelId, ChatMessage, MessageFlags, Span},
};

const EMOTE_SIZE: f32 = 22.0;
const TOOLTIP_EMOTE_SIZE: f32 = 112.0;
const BADGE_SIZE: f32 = 16.0;
const TOOLTIP_BADGE_SIZE: f32 = 64.0;
/// Row left/right padding (px)
const ROW_PAD_X: f32 = 6.0;
/// Row top/bottom padding (px)
const ROW_PAD_Y: f32 = 2.0;

/// Scrollable, bottom-anchored list of chat messages with inline emote images.
pub struct MessageList<'a> {
    messages: &'a VecDeque<ChatMessage>,
    /// Raw image bytes keyed by CDN URL: CDN url → (width, height, raw_bytes)
    emote_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
    /// For sending on-demand image-fetch requests (e.g. HD emote on hover).
    cmd_tx: &'a mpsc::Sender<AppCommand>,
    /// Channel identifier — used for per-channel scroll state.
    channel: &'a ChannelId,
}

impl<'a> MessageList<'a> {
    pub fn new(
        messages: &'a VecDeque<ChatMessage>,
        emote_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
        cmd_tx: &'a mpsc::Sender<AppCommand>,
        channel: &'a ChannelId,
    ) -> Self {
        Self { messages, emote_bytes, cmd_tx, channel }
    }

    /// Render the message list with auto-scroll behaviour.
    ///
    /// * Auto-scrolls to the bottom when new messages arrive.
    /// * Pauses auto-scroll when the user scrolls up.
    /// * Shows a floating "↓ Resume scrolling" button while paused.
    pub fn show(&self, ui: &mut Ui) {
        // We need the available rect before the scroll area consumes it
        let panel_rect = ui.available_rect_before_wrap();
        let n = self.messages.len();

        // Use a per-channel scroll area ID so offset doesn't leak between channels.
        let scroll_id = egui::Id::new("message_list").with(self.channel.as_str());

        // ── Reset stale scroll state on first render of a channel ────
        // egui persists scroll offsets across sessions.  When we
        // (re-)enter a channel whose old state carried a large offset,
        // the first frame would render content at the wrong position.
        // Detect "first render" via a temp flag and force offset to 0.
        let init_key = egui::Id::new("msg_list_init").with(self.channel.as_str());
        let first_render = !ui.ctx().data_mut(|d| {
            let seen: bool = d.get_temp(init_key).unwrap_or(false);
            if !seen {
                d.insert_temp(init_key, true);
            }
            seen
        });

        // Threshold: below this, render all rows directly (no virtual scrolling).
        // This avoids height-estimation and stale-offset edge cases that cause
        // layout glitches with very few messages.
        const VIRTUAL_THRESHOLD: usize = 100;

        // ── Height cache ────────────────────────────────────────────────
        // Keyed by MessageId (u64). Persisted in egui temp storage so that
        // off-screen rows are not re-measured every frame.  Shared between
        // the simple and virtual paths so the transition is seamless.
        let hc_id = egui::Id::new("msg_row_h").with(self.channel.as_str());
        let mut height_cache: std::collections::HashMap<u64, f32> =
            ui.ctx().data_mut(|d| d.get_temp(hc_id).unwrap_or_default());

        // Fallback height for rows we have never rendered before.
        const EST_H: f32 = 26.0;

        // Clear stale height cache when the channel has no messages
        // (e.g. after leaving and re-joining a channel).
        // Also reset the "first render" flag so re-entering triggers
        // a fresh scroll-offset reset.
        if n == 0 {
            height_cache.clear();
            ui.ctx().data_mut(|d| d.insert_temp::<bool>(init_key, false));
        }

        if n < VIRTUAL_THRESHOLD {
            // ── Simple path: render every message, let egui handle layout ─
            // We also measure row heights here so the cache is pre-populated
            // when the channel crosses VIRTUAL_THRESHOLD.
            let mut sa = ScrollArea::vertical()
                .id_salt(scroll_id)
                .auto_shrink([false; 2])
                .stick_to_bottom(true);
            if first_render {
                sa = sa.vertical_scroll_offset(0.0);
            }
            let output = sa.show(ui, |ui| {
                    let full_width = ui.available_width();
                    ui.set_min_width(full_width);
                    for msg in self.messages.iter() {
                        let top_y = ui.next_widget_position().y;
                        if msg.flags.is_deleted {
                            ui.add(
                                Label::new(
                                    RichText::new("<message deleted>")
                                        .italics()
                                        .color(Color32::DARK_GRAY),
                                )
                                .wrap(),
                            );
                        } else {
                            self.render_message(ui, msg);
                        }
                        let measured = ui.next_widget_position().y - top_y;
                        if measured > 0.0 {
                            height_cache.insert(msg.id.0, measured);
                        }
                    }
                });

            // Persist height cache for seamless transition to virtual scrolling.
            ui.ctx().data_mut(|d| d.insert_temp(hc_id, height_cache));
            self.show_resume_button(ui, &output, panel_rect);
            return;
        }

        // First frame in a channel can have unstable viewport metrics and no
        // row-height knowledge yet. Render everything once and measure actual
        // row heights so virtualization can take over with accurate data.
        if height_cache.is_empty() {
            let mut sa = ScrollArea::vertical()
                .id_salt(scroll_id)
                .auto_shrink([false; 2])
                .stick_to_bottom(true);
            if first_render {
                sa = sa.vertical_scroll_offset(0.0);
            }
            let output = sa.show(ui, |ui| {
                    let full_width = ui.available_width();
                    ui.set_min_width(full_width);
                    for msg in self.messages.iter() {
                        let top_y = ui.next_widget_position().y;
                        if msg.flags.is_deleted {
                            ui.add(
                                Label::new(
                                    RichText::new("<message deleted>")
                                        .italics()
                                        .color(Color32::DARK_GRAY),
                                )
                                .wrap(),
                            );
                        } else {
                            self.render_message(ui, msg);
                        }
                        let measured = ui.next_widget_position().y - top_y;
                        height_cache.insert(
                            msg.id.0,
                            if measured > 0.0 { measured } else { EST_H },
                        );
                    }
                });

            ui.ctx().data_mut(|d| d.insert_temp(hc_id, height_cache));
            self.show_resume_button(ui, &output, panel_rect);
            return;
        }

        // Build prefix-sum array.  prefix[i] = y-offset of the top of message i.
        let mut prefix = Vec::with_capacity(n + 1);
        prefix.push(0.0f32);
        for msg in self.messages.iter() {
            let h = height_cache.get(&msg.id.0).copied().unwrap_or(EST_H);
            prefix.push(prefix.last().unwrap() + h);
        }
        let total_h = *prefix.last().unwrap_or(&0.0);

        // ── Virtual-scrolling render pass ────────────────────────────────
        // show_viewport gives us the currently-visible rect in content-local
        // coordinates.  We allocate dead space for off-screen rows and only
        // call render_message for rows whose y-range overlaps the viewport.
        let mut sa = ScrollArea::vertical()
            .id_salt(scroll_id)
            .auto_shrink([false; 2])
            .stick_to_bottom(true);
        if first_render {
            sa = sa.vertical_scroll_offset(0.0);
        }
        let output = sa.show_viewport(ui, |ui, viewport| {
                let full_width = ui.available_width();
                ui.set_min_width(full_width);

                let vis_min = viewport.min.y;
                let vis_max = viewport.max.y;

                // Overscan and minimum-window safeguards prevent first-frame
                // under-rendering when viewport reports a tiny height.
                const OVERSCAN_PX: f32 = 260.0;
                const MIN_RENDER_ROWS: usize = 24;
                let scan_min = (vis_min - OVERSCAN_PX).max(0.0);
                let scan_max = vis_max + OVERSCAN_PX;

                // First row whose bottom edge is visible (top < vis_max).
                let mut first = if n == 0 {
                    0
                } else {
                    prefix.partition_point(|&p| p < scan_min).saturating_sub(1)
                };
                // One past the last visible row (top <= vis_max).
                let mut last = prefix.partition_point(|&p| p <= scan_max).min(n);
                let min_last = (first + MIN_RENDER_ROWS).min(n);
                if last < min_last {
                    last = min_last;
                }
                let min_rows = MIN_RENDER_ROWS.min(n);
                if last.saturating_sub(first) < min_rows {
                    if last == n {
                        first = n.saturating_sub(min_rows);
                    } else {
                        last = (first + min_rows).min(n);
                    }
                }

                // Dead space above the visible window.
                if first > 0 && prefix[first] > 0.0 {
                    ui.allocate_exact_size(
                        egui::Vec2::new(full_width, prefix[first]),
                        egui::Sense::hover(),
                    );
                }

                // Render only visible rows; measure heights for future frames.
                for i in first..last {
                    let msg = &self.messages[i];
                    let top_y = ui.next_widget_position().y;

                    if msg.flags.is_deleted {
                        ui.add(
                            Label::new(
                                RichText::new("<message deleted>")
                                    .italics()
                                    .color(Color32::DARK_GRAY),
                            )
                            .wrap(),
                        );
                    } else {
                        self.render_message(ui, msg);
                    }

                    let measured = ui.next_widget_position().y - top_y;
                    if measured > 0.0 {
                        height_cache.insert(msg.id.0, measured);
                    }
                }

                // Dead space below the visible window.
                let tail = total_h - prefix[last];
                if tail > 0.0 {
                    ui.allocate_exact_size(
                        egui::Vec2::new(full_width, tail),
                        egui::Sense::hover(),
                    );
                }
            });

        // Persist height cache for next frame.
        ui.ctx().data_mut(|d| d.insert_temp(hc_id, height_cache));

        // ── Sanitize corrupt scroll offset ──────────────────────────────
        // Guard against f32::MAX (or NaN) that may linger in persisted state
        // from a previous session.
        if !output.state.offset.y.is_finite() || output.state.offset.y > 1_000_000.0 {
            let mut state = output.state.clone();
            state.offset.y = 0.0;
            state.store(ui.ctx(), output.id);
        }

        self.show_resume_button(ui, &output, panel_rect);
    }

    /// Show the floating "Resume scrolling" button when the user has scrolled up.
    fn show_resume_button(
        &self,
        ui: &mut Ui,
        output: &egui::scroll_area::ScrollAreaOutput<()>,
        panel_rect: egui::Rect,
    ) {
        let viewport_h = output.inner_rect.height();
        let max_scroll = (output.content_size.y - viewport_h).max(0.0);
        let at_bottom = max_scroll < 1.0 || output.state.offset.y >= max_scroll - 20.0;

        if !at_bottom {
            // Paint a floating button on a foreground layer (no Area/Window needed)
            let btn_size = egui::vec2(170.0, 28.0);
            let btn_center = egui::pos2(
                panel_rect.center().x,
                panel_rect.bottom() - 36.0,
            );
            let btn_rect = egui::Rect::from_center_size(btn_center, btn_size);

            let fg_layer = LayerId::new(Order::Foreground, Id::new("resume_scroll_layer"));
            let painter = ui.ctx().layer_painter(fg_layer);

            // Button background
            painter.rect_filled(btn_rect, 8.0, Color32::from_rgb(88, 55, 175));
            // Button label
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                "↓ Resume scrolling",
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );

            // Detect click on the painted rect
            let btn_response = ui.interact(btn_rect, Id::new("resume_scroll_btn"), egui::Sense::click());
            if btn_response.clicked() {
                let id = output.id;
                let mut state = output.state;
                state.offset.y = max_scroll;
                state.store(ui.ctx(), id);
            }
        }
    }

    fn render_message(&self, ui: &mut Ui, msg: &ChatMessage) {
        // ── Message background ──────────────────────────────────────────
        let bg = if msg.flags.is_highlighted {
            Color32::from_rgba_unmultiplied(145, 70, 255, 20)
        } else if msg.flags.custom_reward_id.is_some() {
            Color32::from_rgba_unmultiplied(100, 65, 165, 16)
        } else if msg.flags.is_first_msg {
            Color32::from_rgba_unmultiplied(46, 120, 178, 16)
        } else {
            Color32::TRANSPARENT
        };

        egui::Frame::new()
            .fill(bg)
            .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
            .show(ui, |ui| {
                // ── Notification banner (first message / highlighted / channel points) ──
                // Rendered inside the Frame so the background fill covers
                // the banner as well, and the interaction rect is contiguous.
                if let Some((label, stripe_color)) = notification_label(&msg.flags) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // Colored left stripe
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(3.0, 14.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 1.0, stripe_color);
                        ui.add(Label::new(
                            RichText::new(label)
                                .small()
                                .color(stripe_color),
                        ));
                    });
                }

                // Center-align all items vertically so images don't sit above text baseline.
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                    |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 1.0);

                    // Timestamp
                        let ts = msg.timestamp.format("%H:%M").to_string();
                        ui.add(Label::new(
                            RichText::new(ts)
                                .color(Color32::from_rgb(90, 90, 90))
                                .small(),
                        ));

                        // Separator dot between timestamp and badges/name
                        ui.add(Label::new(
                            RichText::new("·")
                                .color(Color32::from_rgb(70, 70, 70))
                                .small(),
                        ));

                        // Badges: image if loaded, else text fallback
                        for badge in &msg.sender.badges {
                        let tooltip_label = pretty_badge_name(&badge.name, &badge.version);
                            if let Some(url) = &badge.url {
                                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                                    let size = fit_size(w, h, BADGE_SIZE);
                                    let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
                                    let tooltip_label = tooltip_label.clone();
                                    let raw_clone = raw.clone();
                                    let url_key = format!("bytes://{url}");
                                    self.show_image(ui, &url_key, raw, size)
                                        .on_hover_ui(|ui| {
                                            ui.set_max_width(200.0);
                                            ui.vertical_centered(|ui| {
                                                ui.add(
                                                    egui::Image::from_bytes(url_key.clone(), egui::load::Bytes::Shared(raw_clone))
                                                        .fit_to_exact_size(tooltip_size),
                                                );
                                                ui.add_space(4.0);
                                                ui.label(RichText::new(&tooltip_label).strong());
                                            });
                                        });
                                    continue;
                                }
                            }
                            ui.add(Label::new(
                                RichText::new(format!("[{}]", badge.name))
                                    .color(Color32::from_rgb(100, 100, 100))
                                    .small(),
                            ))
                            .on_hover_text(&tooltip_label);
                        }

                        // Sender name
                        let name_color = sender_color(&msg.sender.color);
                        let name = if msg.flags.is_action {
                            format!("* {}", msg.sender.display_name)
                        } else {
                            msg.sender.display_name.clone()
                        };
                        ui.add(Label::new(
                            RichText::new(name).color(name_color).strong(),
                        ));

                        // Colon separator after name (not shown for /me actions)
                        if !msg.flags.is_action {
                            ui.add(Label::new(
                                RichText::new(":").color(Color32::from_rgb(100, 100, 100)),
                            ));
                        }

                        // Message spans
                        for span in &msg.spans {
                            self.render_span(ui, span, msg.flags.is_action);
                        }
                    },
                );
            });
    }

    fn render_span(&self, ui: &mut Ui, span: &Span, is_action: bool) {
        let action_color = Color32::from_rgb(180, 180, 210);
        match span {
            Span::Text { text, .. } => {
                let rt = if is_action {
                    RichText::new(text).italics().color(action_color)
                } else {
                    RichText::new(text)
                };
                ui.add(Label::new(rt).wrap());
            }
            Span::Emote { url, url_hd, code, provider, .. } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let url_key = format!("bytes://{url}");

                    // Capture what the hover closure needs.
                    // References are Copy so these are just pointer copies.
                    let raw_1x      = raw.clone();
                    let url_key_1x  = url_key.clone();
                    let code        = code.clone();
                    let provider    = provider.clone();
                    let url_hd      = url_hd.clone();
                    let emote_bytes = self.emote_bytes;  // &HashMap — Copy
                    let cmd_tx      = self.cmd_tx;       // &Sender  — Copy

                    self.show_image(ui, &url_key, raw, size)
                        .on_hover_ui(move |ui| {
                            // Check HD availability at hover time, not every frame.
                            let hd_entry = url_hd.as_deref()
                                .and_then(|u| emote_bytes.get(u));

                            // Fire HD fetch once on first hover if not yet loaded.
                            if hd_entry.is_none() {
                                if let Some(hd_url) = url_hd.as_deref() {
                                    let _ = cmd_tx.try_send(
                                        AppCommand::FetchImage { url: hd_url.to_owned() }
                                    );
                                }
                            }

                            let (tt_key, tt_raw, tt_w, tt_h) = match hd_entry {
                                Some(&(hw, hh, ref href)) => (
                                    format!("bytes://{}", url_hd.as_deref().unwrap()),
                                    href.clone(), hw, hh,
                                ),
                                None => (url_key_1x.clone(), raw_1x.clone(), w, h),
                            };
                            let tt_size = fit_size(tt_w, tt_h, TOOLTIP_EMOTE_SIZE);

                            ui.set_max_width(280.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                    egui::Image::from_bytes(tt_key, egui::load::Bytes::Shared(tt_raw))
                                        .fit_to_exact_size(tt_size),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(&code).strong());
                                ui.label(
                                    RichText::new(provider_label(&provider))
                                        .small()
                                        .color(Color32::GRAY),
                                );
                            });
                        });
                } else {
                    // Image not yet loaded — show text code as placeholder
                    ui.add(Label::new(
                        RichText::new(code)
                            .italics()
                            .small()
                            .color(Color32::from_rgb(110, 150, 110)),
                    ));
                }
            }
            Span::Emoji { text, url } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let tooltip_size = fit_size(w, h, TOOLTIP_EMOTE_SIZE);
                    let text = text.clone();
                    let raw_clone = raw.clone();
                    let url_key = format!("bytes://{url}");
                    self.show_image(ui, &url_key, raw, size)
                        .on_hover_ui(|ui| {
                            ui.set_max_width(200.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                                    egui::Image::from_bytes(url_key.clone(), egui::load::Bytes::Shared(raw_clone))
                                        .fit_to_exact_size(tooltip_size),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(&text).strong());
                                ui.label(
                                    RichText::new("Twemoji")
                                        .small()
                                        .color(Color32::from_rgb(100, 100, 100)),
                                );
                            });
                        });
                } else {
                    ui.add(Label::new(RichText::new(text)));
                }
            }
            Span::Mention { login } => {
                ui.add(Label::new(
                    RichText::new(format!("@{login}"))
                        .color(Color32::from_rgb(100, 200, 255))
                        .strong(),
                ));
            }
            Span::Url { text, .. } => {
                ui.add(Label::new(
                    RichText::new(text)
                        .color(Color32::from_rgb(100, 180, 255))
                        .underline(),
                ));
            }
            Span::Badge { name, .. } => {
                ui.add(Label::new(
                    RichText::new(format!("[{name}]"))
                        .color(Color32::GRAY)
                        .small(),
                ));
            }
        }
    }

    /// Render an image from raw bytes using egui's image loaders (supports GIF animation).
    fn show_image(&self, ui: &mut Ui, uri: &str, raw: &Arc<[u8]>, size: Vec2) -> egui::Response {
        ui.add(
            egui::Image::from_bytes(uri.to_owned(), egui::load::Bytes::Shared(raw.clone()))
                .fit_to_exact_size(size),
        )
    }
}

/// Scale image dimensions to a target height, preserving aspect ratio.
fn fit_size(w: u32, h: u32, target_h: f32) -> Vec2 {
    if h == 0 {
        return Vec2::new(target_h, target_h);
    }
    let scale = target_h / h as f32;
    Vec2::new(w as f32 * scale, target_h)
}

fn sender_color(color: &Option<String>) -> Color32 {
    color
        .as_deref()
        .and_then(parse_hex_color)
        .map(|(r, g, b)| {
            // Brighten dark colors for readability on dark backgrounds
            let lum = (r as u32 + g as u32 + b as u32) / 3;
            if lum < 60 {
                Color32::from_rgb(
                    (r as u16 + 80).min(255) as u8,
                    (g as u16 + 80).min(255) as u8,
                    (b as u16 + 80).min(255) as u8,
                )
            } else {
                Color32::from_rgb(r, g, b)
            }
        })
        .unwrap_or(Color32::from_rgb(150, 150, 200))
}

fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Build a human-readable badge tooltip from the set name and version.
fn pretty_badge_name(name: &str, version: &str) -> String {
    let label = name
        .split(|c: char| c == '-' || c == '_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => {
                    let mut s = c.to_uppercase().to_string();
                    s.push_str(chars.as_str());
                    s
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    // For subscriber badges the version usually indicates months
    if name == "subscriber" {
        if let Ok(months) = version.parse::<u32>() {
            if months == 0 || months == 1 {
                return format!("{label} (New)");
            }
            return format!("{label} ({months} months)");
        }
    }

    // For bits/sub-gifter the version is the tier/count
    if version != "1" && version != "0" {
        return format!("{label} ({version})");
    }

    label
}

/// Map short provider codes to human-readable labels.
fn provider_label(provider: &str) -> &'static str {
    match provider {
        "bttv" => "BetterTTV",
        "ffz" => "FrankerFaceZ",
        "7tv" => "7TV",
        "twitch" => "Twitch",
        _ => "Emote",
    }
}

/// Return `(label_text, stripe_color)` for messages with a chat notification.
/// Returns `None` for ordinary messages.
fn notification_label(flags: &MessageFlags) -> Option<(&'static str, Color32)> {
    if flags.is_first_msg {
        Some(("First message", Color32::from_rgb(46, 120, 178)))
    } else if flags.is_highlighted {
        Some(("Highlighted Message", Color32::from_rgb(145, 70, 255)))
    } else if flags.custom_reward_id.is_some() {
        Some(("Channel Points Reward", Color32::from_rgb(100, 65, 165)))
    } else {
        None
    }
}
