use std::collections::HashMap;
use std::sync::Arc;

use egui::{
    Color32, Id, Key, LayerId, Order, RichText, Ui, Vec2,
};
use image::DynamicImage;

use crust_core::model::{ChannelId, EmoteCatalogEntry, ReplyInfo};
use crate::theme as t;

const AUTOCOMPLETE_MAX: usize = 10;
const AUTOCOMPLETE_EMOTE_SIZE: f32 = 20.0;
/// Maximum number of Tab-completion matches to cycle through.
const TAB_COMPLETE_MAX: usize = 50;

// Persistent state stored in egui temp data

/// Tab-completion state for bare-word emote completion (no `:` prefix).
#[derive(Clone, Default)]
struct TabState {
    /// Text before the word being completed.
    prefix: String,
    /// Matching emote codes.
    matches: Vec<String>,
    /// Current index in `matches`.
    index: usize,
    /// The buffer content we last set (to detect external changes).
    expected_buf: String,
}

/// Message-history recall state (Up / Down arrows).
#[derive(Clone, Default)]
struct HistState {
    /// -1 = normal input, 0 = most recent sent msg, 1 = second-most-recent, …
    idx: i32,
    /// The original unsent input saved when history navigation started.
    saved_input: String,
    /// What we set the buffer to (to detect user edits → reset).
    expected_buf: String,
}

/// Chat input bar shown at the bottom of the message area.
pub struct ChatInput<'a> {
    /// The active channel to send messages to.
    pub channel: &'a ChannelId,
    /// Whether the user is authenticated (can send).
    pub logged_in: bool,
    /// The current username (for display).
    pub username: Option<&'a str>,
    /// Full emote catalog for autocomplete.
    pub emote_catalog: &'a [EmoteCatalogEntry],
    /// Loaded emote image bytes for rendering previews.
    pub emote_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
    /// If set, show a dismissable "Replying to @name" banner above the input.
    pub pending_reply: Option<&'a ReplyInfo>,
    /// Previously-sent messages for Up/Down recall.
    pub message_history: &'a [String],
}

/// Result from showing the chat input.
pub struct ChatInputResult {
    /// The message text to send, if any.
    pub send: Option<String>,
    /// Whether the emote picker button was clicked.
    pub toggle_emote_picker: bool,
    /// User clicked ✕ to dismiss the pending reply.
    pub dismiss_reply: bool,
}

impl<'a> ChatInput<'a> {
    /// Show the chat input. The `buf` is stored externally so it persists across frames.
    pub fn show(&self, ui: &mut Ui, buf: &mut String) -> ChatInputResult {
        let mut result = ChatInputResult {
            send: None,
            toggle_emote_picker: false,
            dismiss_reply: false,
        };

        // Persistent autocomplete state via egui temp storage
        let ac_id = Id::new("emote_autocomplete");

        // Reply banner
        if let Some(rep) = self.pending_reply {
            egui::Frame::new()
                .fill(t::BG_RAISED)
                .inner_margin(egui::Margin::symmetric(12, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // Left accent stripe
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(2.0, 14.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 0.0, t::ACCENT);
                        ui.label(
                            RichText::new(format!("↩  Replying to @{}", rep.parent_display_name))
                                .font(t::small())
                                .color(t::ACCENT)
                                .strong(),
                        );
                        let body = if rep.parent_msg_body.len() > 60 {
                            format!("\"{}\u{2026}\"", &rep.parent_msg_body[..60])
                        } else {
                            format!("\"{}\"", rep.parent_msg_body)
                        };
                        ui.label(
                            RichText::new(body).font(t::small()).color(t::TEXT_MUTED),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("✕").on_hover_text("Dismiss reply").clicked() {
                                    result.dismiss_reply = true;
                                }
                            },
                        );
                    });
                });
        }

        egui::Frame::new()
            .fill(t::BG_SURFACE)
            .inner_margin(t::INPUT_MARGIN)
            .show(ui, |ui| {
                if !self.logged_in {
                    ui.horizontal_centered(|ui| {
                        ui.label(
                            RichText::new("Log in to send messages")
                                .font(t::body())
                                .color(t::TEXT_MUTED)
                                .italics(),
                        );
                    });
                    return;
                }

                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;
                    // Username label
                    if let Some(name) = self.username {
                        ui.label(
                            RichText::new(format!("{name}:"))
                                .color(t::ACCENT)
                                .strong()
                                .font(t::small()),
                        );
                    }

                    // Pre-check colon-autocomplete
                    let pre_matches = find_autocomplete_matches(buf, self.emote_catalog);
                    let colon_ac_active = !pre_matches.is_empty();

                    // Always consume Tab / Up / Down before TextEdit so they
                    // don't cause focus changes or unwanted behaviour.
                    // Enter is only consumed when the colon-popup needs it.
                    let mut consumed_tab = false;
                    let mut consumed_enter = false;
                    let mut consumed_up = false;
                    let mut consumed_down = false;

                    ui.input_mut(|i| {
                        consumed_tab = i.consume_key(egui::Modifiers::NONE, Key::Tab);
                        consumed_up = i.consume_key(egui::Modifiers::NONE, Key::ArrowUp);
                        consumed_down = i.consume_key(egui::Modifiers::NONE, Key::ArrowDown);
                        if colon_ac_active {
                            consumed_enter = i.consume_key(egui::Modifiers::NONE, Key::Enter);
                        }
                    });

                    // Text input - reserve space for emote button + Send button + 2 gaps
                    let reserve = t::BAR_H + 58.0 + t::TOOLBAR_SPACING.x * 2.0;
                    let text_width = (ui.available_width() - reserve).max(40.0);
                    let resp = ui.add_sized(
                        [text_width, t::BAR_H],
                        egui::TextEdit::singleline(buf)
                            .hint_text("Send a message...")
                            .text_color(t::TEXT_PRIMARY)
                            .margin(egui::Margin::symmetric(6, 6))
                            .frame(true)
                            // Prevent egui from cycling keyboard focus away on Tab;
                            // we handle Tab ourselves for autocomplete.
                            .lock_focus(true),
                    );
                    let text_edit_id = resp.id;

                    // Read autocomplete selection index
                    let mut ac_sel: i32 =
                        ui.ctx().data_mut(|d| d.get_temp(ac_id).unwrap_or(0i32));

                    // Recompute matches after TextEdit may have changed buf
                    let matches = find_autocomplete_matches(buf, self.emote_catalog);

                    let mut accepted_emote: Option<String> = None;

                    if !matches.is_empty() {
                        // ── Colon-autocomplete active ──
                        let n = matches.len() as i32;
                        ac_sel = ac_sel.clamp(0, n - 1);

                        if consumed_up {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_down {
                            ac_sel = (ac_sel + 1).rem_euclid(n);
                        }
                        if consumed_tab || consumed_enter {
                            accepted_emote = Some(matches[ac_sel as usize].code.clone());
                        }
                        // Keep focus on the text field while cycling through the AC list.
                        if consumed_tab || consumed_up || consumed_down {
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    } else {
                        ac_sel = 0;

                        // ── Bare-word Tab completion ──
                        let tab_id = Id::new("tab_complete_state");
                        if consumed_tab {
                            let mut ts: TabState =
                                ui.ctx().data_mut(|d| d.get_temp(tab_id).unwrap_or_default());

                            let continuing =
                                !ts.matches.is_empty() && ts.expected_buf == *buf;

                            if continuing {
                                // Cycle to next match
                                ts.index = (ts.index + 1) % ts.matches.len();
                            } else {
                                // Start new tab session
                                let (pfx, word) = extract_last_word(buf);
                                if !word.is_empty() {
                                    let wl = word.to_lowercase();
                                    let mut m: Vec<String> = self
                                        .emote_catalog
                                        .iter()
                                        .filter(|e| e.code.to_lowercase().starts_with(&wl))
                                        .map(|e| e.code.clone())
                                        .collect();
                                    m.sort_by(|a, b| {
                                        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                                    });
                                    m.truncate(TAB_COMPLETE_MAX);
                                    if !m.is_empty() {
                                        ts = TabState {
                                            prefix: pfx.to_owned(),
                                            matches: m,
                                            index: 0,
                                            expected_buf: String::new(),
                                        };
                                    } else {
                                        ts = TabState::default();
                                    }
                                } else {
                                    ts = TabState::default();
                                }
                            }

                            if !ts.matches.is_empty() {
                                let code = &ts.matches[ts.index];
                                *buf = format!("{}{} ", ts.prefix, code);
                                ts.expected_buf = buf.clone();
                                move_cursor_to_end(ui.ctx(), text_edit_id, buf.len());
                                ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                            }

                            ui.ctx().data_mut(|d| d.insert_temp(tab_id, ts));
                        } else {
                            // Any non-Tab keystroke invalidates the tab session
                            if consumed_up || consumed_down || resp.changed() {
                                ui.ctx().data_mut(|d| d.insert_temp(tab_id, TabState::default()));
                            }
                        }

                        // ── Message history (Up / Down) ──
                        let hist_id = Id::new("msg_history_state");
                        if (consumed_up || consumed_down) && !self.message_history.is_empty() {
                            let mut hs: HistState =
                                ui.ctx().data_mut(|d| d.get_temp(hist_id).unwrap_or_default());

                            // Detect user edits → reset history position
                            if hs.idx >= 0 && *buf != hs.expected_buf {
                                hs.idx = -1;
                            }

                            let hlen = self.message_history.len() as i32;

                            if consumed_up {
                                if hs.idx == -1 {
                                    hs.saved_input = buf.clone();
                                    hs.idx = 0;
                                } else if hs.idx < hlen - 1 {
                                    hs.idx += 1;
                                }
                                let i = (hlen - 1 - hs.idx) as usize;
                                *buf = self.message_history[i].clone();
                            }
                            if consumed_down {
                                if hs.idx > 0 {
                                    hs.idx -= 1;
                                    let i = (hlen - 1 - hs.idx) as usize;
                                    *buf = self.message_history[i].clone();
                                } else if hs.idx == 0 {
                                    hs.idx = -1;
                                    *buf = hs.saved_input.clone();
                                }
                            }

                            hs.expected_buf = buf.clone();
                            ui.ctx().data_mut(|d| d.insert_temp(hist_id, hs));
                            move_cursor_to_end(ui.ctx(), text_edit_id, buf.len());
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    }

                    // Replace the :query token with the accepted emote
                    if let Some(ref code) = accepted_emote {
                        replace_autocomplete_token(buf, code);
                        move_cursor_to_end(ui.ctx(), text_edit_id, buf.len());
                        ui.ctx()
                            .memory_mut(|m| m.request_focus(text_edit_id));
                        ac_sel = 0;
                    }

                    ui.ctx().data_mut(|d| d.insert_temp(ac_id, ac_sel));

                    // ── Send on Enter (only fires when we did NOT consume it) ──
                    let enter_pressed = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(Key::Enter));

                    if enter_pressed && !buf.trim().is_empty() {
                        result.send = Some(buf.trim().to_owned());
                        buf.clear();
                        // Reset history navigation on send
                        ui.ctx().data_mut(|d| d.insert_temp(Id::new("msg_history_state"), HistState::default()));
                        ui.ctx().data_mut(|d| d.insert_temp(Id::new("tab_complete_state"), TabState::default()));
                        resp.request_focus();
                    }

                    // Emote picker button
                    if ui
                        .add_sized(
                            [t::BAR_H, t::BAR_H],
                            egui::Button::new(
                                RichText::new(":)").font(t::small()),
                            ),
                        )
                        .on_hover_text("Emote picker")
                        .clicked()
                    {
                        result.toggle_emote_picker = true;
                    }

                    // Send button
                    if ui
                        .add_sized(
                            [58.0, t::BAR_H],
                            egui::Button::new(
                                RichText::new("Send").font(t::small()),
                            ),
                        )
                        .clicked()
                        && !buf.trim().is_empty()
                    {
                        result.send = Some(buf.trim().to_owned());
                        buf.clear();
                    }

                    // ── Draw autocomplete popup above input ──────────
                    let show_popup = !matches.is_empty()
                        && (resp.has_focus() || accepted_emote.is_some());
                    if show_popup {
                        if let Some(clicked) = self.show_autocomplete_popup(
                            ui, &resp, &matches, ac_sel,
                        ) {
                            replace_autocomplete_token(buf, &clicked);
                            move_cursor_to_end(ui.ctx(), text_edit_id, buf.len());
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                            ui.ctx().data_mut(|d| d.insert_temp(ac_id, 0i32));
                        }
                    }
                });
            });

        result
    }

    fn show_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[&EmoteCatalogEntry],
        selected: i32,
    ) -> Option<String> {
        let input_rect = text_resp.rect;
        let popup_width = input_rect.width().min(350.0);
        let row_h = 28.0;
        let popup_h =
            (matches.len() as f32 * row_h).min(AUTOCOMPLETE_MAX as f32 * row_h) + 8.0;
        let popup_rect = egui::Rect::from_min_size(
            egui::pos2(input_rect.left(), input_rect.top() - popup_h - 4.0),
            egui::vec2(popup_width, popup_h),
        );

        let layer_id =
            LayerId::new(Order::Foreground, Id::new("emote_autocomplete_popup"));
        let painter = ui.ctx().layer_painter(layer_id);

        // Background + border
        painter.rect_filled(popup_rect, 6.0, t::BG_RAISED);
        painter.rect_stroke(
            popup_rect,
            6.0,
            egui::Stroke::new(1.0, t::BORDER_SUBTLE),
            egui::epaint::StrokeKind::Outside,
        );

        // Interactive child UI on the foreground layer - force vertical layout
        let mut popup_ui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer_id)
                .max_rect(popup_rect.shrink(4.0))
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        popup_ui.set_clip_rect(popup_rect);

        let mut clicked_emote: Option<String> = None;
        let static_id = Id::new("ac_static_frames");
        let mut static_frames: HashMap<String, egui::TextureHandle> =
            ui.ctx().data_mut(|d| d.get_temp(static_id).unwrap_or_default());

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(&mut popup_ui, |ui| {
                for (i, entry) in matches.iter().enumerate() {
                    let is_selected = i as i32 == selected;
                    let row_id = Id::new("ac_row").with(i);

                    let row_bg = if is_selected {
                        t::ACTIVE_CHANNEL_BG
                    } else {
                        Color32::TRANSPARENT
                    };

                    let frame_resp = egui::Frame::new()
                        .fill(row_bg)
                        .corner_radius(3.0)
                        .inner_margin(egui::Margin::symmetric(4, 2))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;

                                // Emote image preview
                                if let Some(&(w, h, ref raw)) =
                                    self.emote_bytes.get(entry.url.as_str())
                                {
                                    let animated = is_likely_animated_url(&entry.url);
                                    let should_animate = is_selected;
                                    let (slot_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                        ),
                                        egui::Sense::hover(),
                                    );

                                    if animated && !should_animate {
                                        if !static_frames.contains_key(&entry.url) {
                                            if let Some(img) = decode_static_frame(raw) {
                                                let tex = ui.ctx().load_texture(
                                                    format!("ac-static://{}", entry.url),
                                                    img,
                                                    egui::TextureOptions::LINEAR,
                                                );
                                                static_frames.insert(entry.url.clone(), tex);
                                            }
                                        }

                                        if let Some(tex) = static_frames.get(&entry.url) {
                                            let size = fit_size(
                                                w,
                                                h,
                                                AUTOCOMPLETE_EMOTE_SIZE,
                                            );
                                            let image_rect =
                                                egui::Rect::from_center_size(
                                                    slot_rect.center(),
                                                    size,
                                                );
                                            ui.painter().image(
                                                tex.id(),
                                                image_rect,
                                                egui::Rect::from_min_max(
                                                    egui::pos2(0.0, 0.0),
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                Color32::WHITE,
                                            );
                                        }
                                    } else {
                                        let size = fit_size(w, h, AUTOCOMPLETE_EMOTE_SIZE);
                                        let image_rect = egui::Rect::from_center_size(
                                            slot_rect.center(),
                                            size,
                                        );
                                        let url_key = format!("bytes://{}", entry.url);
                                        ui.put(
                                            image_rect,
                                            egui::Image::from_bytes(
                                                url_key,
                                                egui::load::Bytes::Shared(raw.clone()),
                                            )
                                            .fit_to_exact_size(size),
                                        );
                                    }
                                } else {
                                    ui.allocate_exact_size(
                                        egui::vec2(
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                        ),
                                        egui::Sense::hover(),
                                    );
                                }

                                let code_color = if is_selected {
                                    t::TEXT_PRIMARY
                                } else {
                                    t::TEXT_SECONDARY
                                };
                                ui.label(RichText::new(&entry.code).color(code_color).font(t::small()));

                                // Provider tag (right-aligned)
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(provider_label(&entry.provider))
                                                .font(t::small())
                                                .color(t::TEXT_MUTED),
                                        );
                                    },
                                );
                            });
                        });

                    // Make the row clickable on the foreground layer
                    let row_rect = frame_resp.response.rect;
                    let click_resp =
                        ui.interact(row_rect, row_id, egui::Sense::click());

                    // If an animated emote row is hovered, render its animated preview.
                    if click_resp.hovered() {
                        if let Some(&(w, h, ref raw)) =
                            self.emote_bytes.get(entry.url.as_str())
                        {
                            if is_likely_animated_url(&entry.url) {
                                let size = fit_size(w, h, AUTOCOMPLETE_EMOTE_SIZE);
                                let preview_rect = egui::Rect::from_min_size(
                                    egui::pos2(row_rect.left() + 4.0, row_rect.top() + 2.0),
                                    egui::vec2(
                                        AUTOCOMPLETE_EMOTE_SIZE,
                                        AUTOCOMPLETE_EMOTE_SIZE,
                                    ),
                                );
                                let image_rect =
                                    egui::Rect::from_center_size(preview_rect.center(), size);
                                ui.put(
                                    image_rect,
                                    egui::Image::from_bytes(
                                        format!("bytes://{}", entry.url),
                                        egui::load::Bytes::Shared(raw.clone()),
                                    )
                                    .fit_to_exact_size(size),
                                );
                            }
                        }
                    }

                    if click_resp.clicked() {
                        clicked_emote = Some(entry.code.clone());
                    }
                }
            });

        ui.ctx().data_mut(|d| d.insert_temp(static_id, static_frames));

        clicked_emote
    }
}

/// Find emotes matching a `:partial` token at the end of the input buffer.
fn find_autocomplete_matches<'a>(
    buf: &str,
    catalog: &'a [EmoteCatalogEntry],
) -> Vec<&'a EmoteCatalogEntry> {
    let query = match extract_colon_query(buf) {
        Some(q) if !q.is_empty() => q,
        _ => return Vec::new(),
    };

    let query_lower = query.to_lowercase();
    let mut matches: Vec<&EmoteCatalogEntry> = catalog
        .iter()
        .filter(|e| e.code.to_lowercase().contains(&query_lower))
        .collect();

    // Prefix matches first, then shorter codes first
    matches.sort_by(|a, b| {
        let a_prefix = a.code.to_lowercase().starts_with(&query_lower);
        let b_prefix = b.code.to_lowercase().starts_with(&query_lower);
        b_prefix
            .cmp(&a_prefix)
            .then_with(|| a.code.len().cmp(&b.code.len()))
            .then_with(|| a.code.cmp(&b.code))
    });

    matches.truncate(AUTOCOMPLETE_MAX);
    matches
}

/// Extract the partial query after the last `:` in the buffer.
fn extract_colon_query(buf: &str) -> Option<&str> {
    let trimmed = buf.trim_end();
    let bytes = trimmed.as_bytes();

    for i in (0..bytes.len()).rev() {
        if bytes[i] == b':' {
            if i == 0 || bytes[i - 1] == b' ' {
                let after = &trimmed[i + 1..];
                if !after.contains(' ') && !after.is_empty() {
                    return Some(after);
                }
            }
        }
    }
    None
}

/// Move the TextEdit cursor to `char_pos` (pass `buf.len()` for end-of-input).
fn move_cursor_to_end(ctx: &egui::Context, id: egui::Id, char_pos: usize) {
    use egui::text::{CCursor, CCursorRange};
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let cursor = CCursor::new(char_pos);
        state.set_ccursor_range(Some(CCursorRange::one(cursor)));
        egui::TextEdit::store_state(ctx, id, state);
    }
}

/// Replace the `:query` at the end of the buffer with the emote code.
fn replace_autocomplete_token(buf: &mut String, code: &str) {
    let trimmed_len = buf.trim_end().len();
    let bytes = buf.as_bytes();

    for i in (0..trimmed_len).rev() {
        if bytes[i] == b':' {
            if i == 0 || bytes[i - 1] == b' ' {
                buf.replace_range(i..trimmed_len, code);
                buf.push(' ');
                return;
            }
        }
    }
}

fn provider_label(provider: &str) -> &'static str {
    match provider {
        "bttv" => "BTTV",
        "ffz" => "FFZ",
        "7tv" => "7TV",
        "twitch" => "Twitch",
        _ => "Emote",
    }
}

fn fit_size(w: u32, h: u32, target_h: f32) -> Vec2 {
    if w == 0 || h == 0 {
        return Vec2::new(target_h, target_h);
    }
    let scale_x = target_h / w as f32;
    let scale_y = target_h / h as f32;
    let scale = scale_x.min(scale_y);
    Vec2::new(w as f32 * scale, h as f32 * scale)
}

fn is_likely_animated_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains(".gif") || lower.contains(".webp")
}

fn decode_static_frame(raw: &[u8]) -> Option<egui::ColorImage> {
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

/// Extract the last whitespace-delimited word from the buffer.
/// Returns `(prefix_before_word, word)`.
fn extract_last_word(buf: &str) -> (&str, &str) {
    let trimmed = buf.trim_end_matches(' ');
    if trimmed.is_empty() {
        return (buf, "");
    }
    if let Some(pos) = trimmed.rfind(' ') {
        (&buf[..pos + 1], &trimmed[pos + 1..])
    } else {
        ("", trimmed)
    }
}
