use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use egui::{
    Color32, Id, Label, LayerId, Order, RichText, ScrollArea, Ui, Vec2,
};
use tokio::sync::mpsc;

use crust_core::{
    events::{AppCommand, LinkPreview},
    model::{Badge, ChannelId, ChatMessage, MessageFlags, MsgKind, ReplyInfo, Span},
};

use crate::theme as t;

/// Returned from [`MessageList::show`].
pub struct MessageListResult {
    /// Set when the user right-clicked a message and chose "Reply".
    pub reply: Option<ReplyInfo>,
    /// Set when a username was clicked: (login, sender_badges).
    pub profile_request: Option<(String, Vec<Badge>)>,
}

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
    /// Channel identifier - used for per-channel scroll state.
    channel: &'a ChannelId,
    /// Cached link previews keyed by URL.
    link_previews: &'a HashMap<String, LinkPreview>,
}

impl<'a> MessageList<'a> {
    pub fn new(
        messages: &'a VecDeque<ChatMessage>,
        emote_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
        cmd_tx: &'a mpsc::Sender<AppCommand>,
        channel: &'a ChannelId,
        link_previews: &'a HashMap<String, LinkPreview>,
    ) -> Self {
        Self { messages, emote_bytes, cmd_tx, channel, link_previews }
    }

    /// Render the message list with auto-scroll behaviour.
    ///
    /// * Auto-scrolls to the bottom when new messages arrive.
    /// * Pauses auto-scroll when the user scrolls up.
    /// * Shows a floating "↓ Resume scrolling" button while paused.
    /// * Returns a [`MessageListResult`] that may contain a reply request.
    pub fn show(&self, ui: &mut Ui) -> MessageListResult {
        let reply_key = Id::new("ml_reply_req").with(self.channel.as_str());
        // We need the available rect before the scroll area consumes it
        let panel_rect = ui.available_rect_before_wrap();
        // Keep a small safety gap at the bottom so message pixels/emotes
        // don't bleed into the input panel border while scrolling.
        let mut clip = ui.clip_rect();
        clip.max.y = clip.max.y.min(panel_rect.max.y - 2.0);
        ui.set_clip_rect(clip);
        let n = self.messages.len();
        // Use a per-channel scroll area ID so offset doesn't leak
        let scroll_id = egui::Id::new("message_list").with(self.channel.as_str());

        // Reset stale scroll state on first render of a channel
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
        // layout glitches with very few messages.  Kept low so virtual
        // scrolling kicks in early and only visible rows are rendered.
        const VIRTUAL_THRESHOLD: usize = 40;

        // Height cache
        // Keyed by MessageId (u64). Persisted in egui temp storage so that
        // off-screen rows are not re-measured every frame.  Shared between
        // the simple and virtual paths so the transition is seamless.
        let hc_id = egui::Id::new("msg_row_h").with(self.channel.as_str());
        let mut height_cache: std::collections::HashMap<u64, f32> =
            ui.ctx().data_mut(|d| d.get_temp(hc_id).unwrap_or_default());

        // Fallback height for rows we have never rendered before.
        const EST_H: f32 = 26.0;

        // Invalidate height cache when available width changes significantly
        // (e.g. window resize or sidebar drag), since messages re-wrap to
        // different heights at different widths.
        let avail_width = ui.available_width();
        let width_key = egui::Id::new("msg_list_width").with(self.channel.as_str());
        let prev_width: f32 = ui.ctx().data_mut(|d| d.get_temp(width_key).unwrap_or(0.0));
        if (avail_width - prev_width).abs() > 2.0 {
            height_cache.clear();
            ui.ctx().data_mut(|d| d.insert_temp(width_key, avail_width));
        }

        // Clear stale height cache when the channel has no messages
        // (e.g. after leaving and re-joining a channel).
        // Also reset the "first render" flag so re-entering triggers
        // a fresh scroll-offset reset.
        if n == 0 {
            height_cache.clear();
            ui.ctx().data_mut(|d| d.insert_temp::<bool>(init_key, false));
        }

        // Scroll-to-reply target
        // Written by the reply-header click handler; read and cleared here so
        // it only fires once.
        let scroll_to_key = egui::Id::new("ml_scroll_to").with(self.channel.as_str());
        let forced_offset: Option<f32> = {
            let target: Option<String> =
                ui.ctx().data_mut(|d| {
                    let v: Option<String> = d.get_temp(scroll_to_key);
                    if v.is_some() {
                        d.remove::<String>(scroll_to_key);
                    }
                    v
                });
            target.and_then(|tgt_id| {
                let idx = self.messages.iter().position(|m| {
                    m.server_id.as_deref() == Some(tgt_id.as_str())
                })?;
                let offset: f32 = (0..idx)
                    .map(|i| height_cache.get(&self.messages[i].id.0).copied().unwrap_or(EST_H))
                    .sum();
                Some(offset)
            })
        };

        if n < VIRTUAL_THRESHOLD {
            // ── Simple path: render every message, let egui handle layout ─
            // We also measure row heights here so the cache is pre-populated
            // when the channel crosses VIRTUAL_THRESHOLD.
            let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
            let scroll_paused: bool =
                ui.ctx().data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
            let mut sa = ScrollArea::vertical()
                .id_salt(scroll_id)
                .auto_shrink([false; 2])
                .stick_to_bottom(!scroll_paused && forced_offset.is_none());
            if let Some(offset) = forced_offset {
                sa = sa.vertical_scroll_offset(offset);
            } else if first_render {
                sa = sa.vertical_scroll_offset(0.0);
            }
            let output = sa.show(ui, |ui| {
                    let full_width = ui.available_width();
                    ui.set_min_width(full_width);
                    for msg in self.messages.iter() {
                        let top_y = ui.next_widget_position().y;
                        self.render_message(ui, msg);
                        let measured = ui.next_widget_position().y - top_y;
                        if measured > 0.0 {
                            height_cache.insert(msg.id.0, measured);
                        }
                    }
                });

            // Persist height cache for seamless transition to virtual scrolling.
            ui.ctx().data_mut(|d| d.insert_temp(hc_id, height_cache));
            self.apply_snap(ui, &output);
            self.show_resume_button(ui, &output, panel_rect);
            return MessageListResult {
                reply: self.take_reply(ui, reply_key),
                profile_request: self.take_profile_request(ui),
            };
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
        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
        let scroll_paused: bool =
            ui.ctx().data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
        let mut sa = ScrollArea::vertical()
            .id_salt(scroll_id)
            .auto_shrink([false; 2])
            .stick_to_bottom(!scroll_paused && forced_offset.is_none());
        if let Some(offset) = forced_offset {
            sa = sa.vertical_scroll_offset(offset);
        } else if first_render {
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

                    self.render_message(ui, msg);

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

        self.apply_snap(ui, &output);
        self.show_resume_button(ui, &output, panel_rect);
        MessageListResult {
            reply: self.take_reply(ui, reply_key),
            profile_request: self.take_profile_request(ui),
        }
    }

    /// Read and clear the reply request stored by a context menu during this frame.
    fn take_reply(&self, ui: &Ui, key: Id) -> Option<ReplyInfo> {
        ui.ctx().data_mut(|d| {
            let v: Option<ReplyInfo> = d.get_temp(key);
            if v.is_some() { d.remove::<ReplyInfo>(key); }
            v
        })
    }

    /// Read and clear the profile-request stored by a username click this frame.
    fn take_profile_request(&self, ui: &Ui) -> Option<(String, Vec<Badge>)> {
        let key = Id::new("ml_profile_req").with(self.channel.as_str());
        ui.ctx().data_mut(|d| {
            let v: Option<(String, Vec<Badge>)> = d.get_temp(key);
            if v.is_some() { d.remove::<(String, Vec<Badge>)>(key); }
            v
        })
    }

    /// If the snap-to-bottom flag is active, force the scroll offset to the
    /// current real maximum every frame until `stick_to_bottom` takes over.
    fn apply_snap(&self, ui: &mut Ui, output: &egui::scroll_area::ScrollAreaOutput<()>) {
        let snap_key = Id::new("snap_to_bottom").with(self.channel.as_str());
        let snapping: bool = ui.ctx().data_mut(|d| d.get_temp(snap_key).unwrap_or(false));
        if !snapping { return; }

        let viewport_h = output.inner_rect.height();
        let max_scroll = (output.content_size.y - viewport_h).max(0.0);
        let at_bottom = max_scroll < 1.0 || output.state.offset.y >= max_scroll - 20.0;

        if at_bottom {
            // stick_to_bottom has taken over; clear the flag.
            ui.ctx().data_mut(|d| d.insert_temp(snap_key, false));
        } else {
            // Re-write the true max every frame so new messages don't stall us.
            let mut state = output.state;
            state.offset.y = max_scroll;
            state.store(ui.ctx(), output.id);
            ui.ctx().request_repaint();
        }
    }

    /// Show the floating "Resume scrolling" button when the user has scrolled up.
    /// Also updates the per-channel `scroll_paused` flag used to gate `stick_to_bottom`.
    fn show_resume_button(
        &self,
        ui: &mut Ui,
        output: &egui::scroll_area::ScrollAreaOutput<()>,
        panel_rect: egui::Rect,
    ) {
        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
        let viewport_h = output.inner_rect.height();
        let max_scroll = (output.content_size.y - viewport_h).max(0.0);
        let at_bottom = max_scroll < 1.0 || output.state.offset.y >= max_scroll - 20.0;

        // Keep the paused flag in sync with where the scroll actually is.
        ui.ctx().data_mut(|d| d.insert_temp(paused_key, !at_bottom));

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
                // Clear paused so stick_to_bottom re-engages next frame.
                ui.ctx().data_mut(|d| d.insert_temp(paused_key, false));
                // Immediately write the real max_scroll to the correct egui
                // scroll-state key (output.id, not the salt), and set the
                // snap flag so apply_snap keeps rewriting every frame until
                // stick_to_bottom confirms we are at the bottom.
                let mut state = output.state;
                state.offset.y = max_scroll;
                state.store(ui.ctx(), output.id);
                let snap_key = Id::new("snap_to_bottom").with(self.channel.as_str());
                ui.ctx().data_mut(|d| d.insert_temp(snap_key, true));
                ui.ctx().request_repaint();
            }
        }
    }

    fn render_message(&self, ui: &mut Ui, msg: &ChatMessage) {
        // Dispatch non-chat (and non-bits) events to the compact system-event renderer.
        match &msg.msg_kind {
            MsgKind::Chat | MsgKind::Bits { .. } => {}
            _ => { self.render_system_event(ui, msg); return; }
        }

        let reply_key = Id::new("ml_reply_req").with(self.channel.as_str());
        let scroll_to_key = egui::Id::new("ml_scroll_to").with(self.channel.as_str());
        // Stable ID per message for context menu state tracking.
        let ctx_id = egui::Id::new("msg_ctx").with(msg.id.0);

        // ── Message background ──────────────────────────────────────────
        let bg = if msg.flags.is_highlighted {
            Color32::from_rgba_unmultiplied(145, 70, 255, 20)
        } else if msg.flags.is_mention {
            Color32::from_rgba_unmultiplied(210, 140, 40, 22)
        } else if msg.flags.is_deleted {
            Color32::from_rgba_unmultiplied(180, 30, 30, 12)        } else if matches!(msg.msg_kind, MsgKind::Bits { .. }) {
            Color32::from_rgba_unmultiplied(255, 175, 30, 14)        } else if msg.flags.custom_reward_id.is_some() {
            Color32::from_rgba_unmultiplied(100, 65, 165, 16)
        } else {
            Color32::TRANSPARENT
        };

        let msg_frame = egui::Frame::new()
            .fill(bg)
            .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
            .show(ui, |ui| {
                // History messages are rendered at reduced opacity so they
                // read as older context while still being fully legible.
                if msg.flags.is_history {
                    ui.set_opacity(0.55);
                }
                // ── Context menu - registered FIRST so child widgets (images,
                // labels) have higher registration order and win egui's hover
                // hit-test, keeping emote/badge tooltips functional.
                // `ui.interact` does NOT move the cursor / allocate space.
                //
                // We capture `msg` by reference so strings are only cloned
                // when the user actually opens the menu (not every frame).
                ui.interact(ui.max_rect(), ctx_id, egui::Sense::click())
                    .context_menu(|ui| {
                        if let Some(ref id) = msg.server_id {
                            if !msg.flags.is_deleted {
                                if ui.button("↩  Reply").clicked() {
                                    let info = ReplyInfo {
                                        parent_msg_id: id.clone(),
                                        parent_user_login: msg.sender.login.clone(),
                                        parent_display_name: msg.sender.display_name.clone(),
                                        parent_msg_body: msg.raw_text.clone(),
                                    };
                                    ui.ctx().data_mut(|d| d.insert_temp(reply_key, info));
                                    ui.close_menu();
                                }
                                ui.separator();
                            }
                        }
                        if ui.button("📋  Copy message").clicked() {
                            ui.ctx().copy_text(msg.raw_text.clone());
                            ui.close_menu();
                        }
                    });
                if let Some(ref rep) = msg.reply {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        // Accent left stripe
                        let (stripe, _) = ui.allocate_exact_size(
                            egui::vec2(2.0, 12.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(stripe, 0.0, Color32::from_rgb(100, 65, 190));
                        let body = if rep.parent_msg_body.chars().count() > 80 {
                            // Find the byte offset of the 80th char boundary.
                            let cut = rep.parent_msg_body
                                .char_indices()
                                .nth(80)
                                .map(|(i, _)| i)
                                .unwrap_or(rep.parent_msg_body.len());
                            format!("{}…", &rep.parent_msg_body[..cut])
                        } else {
                            rep.parent_msg_body.clone()
                        };
                        let h = ui.add(
                            Label::new(
                                RichText::new(format!(
                                    "↩ @{}: {}",
                                    rep.parent_display_name, body
                                ))
                                .font(t::small())
                                .color(Color32::from_rgb(130, 130, 155))
                                .italics(),
                            )
                            .sense(egui::Sense::click())
                            .truncate(),
                        );
                        if h.clicked() {
                            let target = rep.parent_msg_id.clone();
                            ui.ctx().data_mut(|d| d.insert_temp(scroll_to_key, target));
                        }
                        if h.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                    });
                }
                // ── Notification banner (first message / highlighted / channel points) ──
                // Rendered inside the Frame so the background fill covers
                // the banner as well, and the interaction rect is contiguous.
                if let Some((label, stripe_color)) = notification_label(&msg.flags, &msg.msg_kind) {
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
                                .font(t::small())
                                .color(stripe_color),
                        ));
                    });
                }

                // Center-align all items vertically so images don't sit above text baseline.
                // Use allocate_ui_with_layout with a constrained height hint
                // (one emote row) instead of with_layout, because Align::Center
                // in a horizontal layout causes egui to expand frame_size.y to
                // fill the full available height - which for the first message
                // in a ScrollArea means the entire viewport, creating huge gaps.
                let wrap_width = ui.available_width();
                ui.allocate_ui_with_layout(
                    egui::vec2(wrap_width, EMOTE_SIZE),
                    egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                    |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 1.0);

                    // Timestamp
                        let ts = msg.timestamp.with_timezone(&chrono::Local).format("%H:%M").to_string();
                        ui.add(Label::new(
                            RichText::new(ts)
                                .color(Color32::from_rgb(90, 90, 90))
                                .font(t::small()),
                        ));

                        // Separator dot between timestamp and badges/name
                        ui.add(Label::new(
                            RichText::new("·")
                                .color(Color32::from_rgb(70, 70, 70))
                                .font(t::small()),
                        ));

                        // Badges: image if loaded, else text fallback
                        for badge in &msg.sender.badges {
                        let tooltip_label = pretty_badge_name(&badge.name, &badge.version);
                            if let Some(url) = &badge.url {
                                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                                    let size = fit_size(w, h, BADGE_SIZE);
                                    let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
                                    let url_key = format!("bytes://{url}");
                                    // Closures capture by reference - clones
                                    // only happen when the tooltip is actually
                                    // shown (on hover), not every frame.
                                    self.show_image(ui, &url_key, raw, size)
                                        .on_hover_ui(|ui| {
                                            ui.set_max_width(200.0);
                                            ui.vertical_centered(|ui| {
                                                ui.add(
                                                    egui::Image::from_bytes(url_key.clone(), egui::load::Bytes::Shared(raw.clone()))
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
                                    .font(t::small()),
                            ))
                            .on_hover_text(&tooltip_label);
                        }

                        // Sender name - clickable to open user profile card.
                        let name_color = sender_color(&msg.sender.color);
                        let name = if msg.flags.is_action {
                            format!("* {}", msg.sender.display_name)
                        } else {
                            msg.sender.display_name.clone()
                        };
                        let name_resp = ui
                            .add(
                                Label::new(
                                    RichText::new(name).color(name_color).strong(),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text(format!("@{}", msg.sender.login));
                        if name_resp.clicked() {
                            // Clone only when clicked - not every frame.
                            let _ = self.cmd_tx.try_send(
                                AppCommand::FetchUserProfile {
                                    login: msg.sender.login.clone(),
                                },
                            );
                            let key = Id::new("ml_profile_req").with(self.channel.as_str());
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(key, (msg.sender.login.clone(), msg.sender.badges.clone()));
                            });
                        }
                        if name_resp.hovered() {
                            ui.ctx()
                                .set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        // Colon separator after name (not shown for /me actions)
                        if !msg.flags.is_action {
                            ui.add(Label::new(
                                RichText::new(":").color(Color32::from_rgb(100, 100, 100)),
                            ));
                        }

                        // Message spans - for deleted messages show the
                        // original content with strikethrough so moderator
                        // actions are visible without being prominent.
                        if msg.flags.is_deleted {
                            ui.add(
                                Label::new(
                                    RichText::new(format!("✂ {}", msg.raw_text))
                                        .strikethrough()
                                        .italics()
                                        .color(Color32::from_rgb(90, 90, 90)),
                                )
                                .wrap(),
                            );
                        } else {
                            for span in &msg.spans {
                                self.render_span(ui, span, msg.flags.is_action);
                            }
                        }
                    },
                );
            });

        // Left accent strip for mentions and highlights — a vivid 3 px bar on
        // the left edge of the row so the eye finds them instantly in fast chat.
        if msg.flags.is_mention || msg.flags.is_highlighted {
            let r = msg_frame.response.rect;
            let bar_col = if msg.flags.is_mention { t::ACCENT } else { Color32::from_rgb(255, 210, 30) };
            let strip = egui::Rect::from_min_size(r.left_top(), egui::vec2(3.0, r.height()));
            ui.painter().rect_filled(strip, 0.0, bar_col);
        }
    }

    /// Render a compact system-event row (mod action, sub alert, raid, notice).
    /// These are centred italic lines with a coloured left stripe and icon,
    /// similar to Chatterino's system-message style.
    fn render_system_event(&self, ui: &mut Ui, msg: &ChatMessage) {
        let (accent, label_override): (Color32, Option<String>) = match &msg.msg_kind {
            MsgKind::Sub { display_name, months, plan, is_gift, .. } => {
                let text = if *is_gift {
                    format!("🎁  {display_name} received a gifted {plan} sub! ({months} months)")
                } else if *months <= 1 {
                    format!("⭐  {display_name} subscribed with {plan}!")
                } else {
                    format!("⭐  {display_name} resubscribed with {plan}! ({months} months)")
                };
                (Color32::from_rgb(255, 215, 0), Some(text))
            }
            MsgKind::Raid { display_name, viewer_count } => (
                Color32::from_rgb(100, 200, 255),
                Some(format!("🎉  {display_name} is raiding with {viewer_count} viewers!")),
            ),
            MsgKind::Timeout { login, seconds } => {
                let dur = if *seconds < 60 {
                    format!("{seconds}s")
                } else if *seconds < 3600 {
                    format!("{}m", seconds / 60)
                } else {
                    format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
                };
                (
                    Color32::from_rgb(220, 160, 50),
                    Some(format!("⏱  {login} was timed out for {dur}.")),
                )
            }
            MsgKind::Ban { login } => (
                t::RED,
                Some(format!("🚫  {login} was permanently banned.")),
            ),
            MsgKind::ChatCleared => (
                Color32::from_rgb(130, 120, 150),
                Some("🗑  Chat was cleared by a moderator.".to_owned()),
            ),
            _ => (
                Color32::from_rgb(100, 100, 120),
                Some(msg.raw_text.clone()),
            ),
        };

        let text = label_override.unwrap_or_else(|| msg.raw_text.clone());
        let opacity = if msg.flags.is_history { 0.55 } else { 1.0 };

        egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 10))
            .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8 + 1))
            .show(ui, |ui| {
                if msg.flags.is_history { ui.set_opacity(opacity); }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // Coloured left stripe
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(3.0, 14.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 1.0, accent);

                    // Timestamp
                    let ts = msg.timestamp.format("%H:%M").to_string();
                    ui.add(Label::new(
                        RichText::new(ts).color(Color32::from_rgb(90, 90, 90)).font(t::small()),
                    ));

                    // Message text
                    ui.add(Label::new(
                        RichText::new(text)
                            .italics()
                            .color(accent)
                            .font(t::small()),
                    ).wrap());
                });
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

                    // Capture shared references - string/Arc clones only
                    // happen when the tooltip is actually shown (on hover).
                    let emote_bytes = self.emote_bytes;  // &HashMap - Copy
                    let cmd_tx      = self.cmd_tx;       // &Sender  - Copy

                    self.show_image(ui, &url_key, raw, size)
                        .on_hover_ui(|ui| {
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
                                None => (url_key.clone(), raw.clone(), w, h),
                            };
                            let tt_size = fit_size(tt_w, tt_h, TOOLTIP_EMOTE_SIZE);

                            ui.set_max_width(280.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                    egui::Image::from_bytes(tt_key, egui::load::Bytes::Shared(tt_raw))
                                        .fit_to_exact_size(tt_size),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(code.as_str()).strong());
                                ui.label(
                                    RichText::new(provider_label(provider))
                                        .small()
                                        .color(Color32::GRAY),
                                );
                            });
                        });
                } else {
                    // Image not yet loaded - show text code as placeholder
                    ui.add(Label::new(
                        RichText::new(code)
                            .italics()
                            .font(t::small())
                            .color(Color32::from_rgb(110, 150, 110)),
                    ));
                }
            }
            Span::Emoji { text, url } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let tooltip_size = fit_size(w, h, TOOLTIP_EMOTE_SIZE);
                    let url_key = format!("bytes://{url}");
                    self.show_image(ui, &url_key, raw, size)
                        .on_hover_ui(|ui| {
                            ui.set_max_width(200.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                                    egui::Image::from_bytes(url_key.clone(), egui::load::Bytes::Shared(raw.clone()))
                                        .fit_to_exact_size(tooltip_size),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(text.as_str()).strong());
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
            Span::Url { text, url } => {
                let cmd_tx      = self.cmd_tx;
                let link_previews = self.link_previews;
                let emote_bytes  = self.emote_bytes;

                // Render as a clickable hyperlink-style label.
                let resp = ui.add(
                    Label::new(
                        RichText::new(text)
                            .color(Color32::from_rgb(100, 180, 255))
                            .underline(),
                    )
                    .sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    let _ = cmd_tx.try_send(AppCommand::OpenUrl { url: url.clone() });
                }
                resp.on_hover_ui(|ui| {
                    // Fire preview fetch on first hover (idempotent in reducer).
                    let preview = link_previews.get(url.as_str());
                    if preview.map(|p| p.fetched).unwrap_or(false) == false {
                        let _ = cmd_tx.try_send(
                            AppCommand::FetchLinkPreview { url: url.clone() }
                        );
                    }

                    ui.set_max_width(300.0);
                    ui.vertical(|ui| {
                        match preview {
                            None => {
                                // Not yet fetched - show hostname + spinner.
                                let host = url_hostname(url);
                                ui.label(
                                    RichText::new(host)
                                        .small()
                                        .color(Color32::from_rgb(100, 180, 255)),
                                );
                                ui.label(
                                    RichText::new("Loading preview…")
                                        .small()
                                        .italics()
                                        .color(Color32::GRAY),
                                );
                            }
                            Some(p) => {
                                // Thumbnail
                                if let Some(ref thumb) = p.thumbnail_url {
                                    if let Some(&(w, h, ref raw)) = emote_bytes.get(thumb.as_str()) {
                                        let scale = (150.0_f32 / h as f32)
                                            .min(280.0 / w as f32);
                                        let size = Vec2::new(
                                            w as f32 * scale,
                                            h as f32 * scale,
                                        );
                                        let key = format!("bytes://{thumb}");
                                        ui.add(
                                            egui::Image::from_bytes(
                                                key,
                                                egui::load::Bytes::Shared(raw.clone()),
                                            )
                                            .fit_to_exact_size(size),
                                        );
                                        ui.add_space(4.0);
                                    }
                                }
                                // Title
                                if let Some(ref t) = p.title {
                                    ui.add(Label::new(RichText::new(t).strong()).wrap());
                                }
                                // Description
                                if let Some(ref d) = p.description {
                                    let snippet = if d.len() > 220 {
                                        format!("{}\u{2026}", &d[..220])
                                    } else {
                                        d.clone()
                                    };
                                    ui.add(
                                        Label::new(RichText::new(snippet).small()).wrap()
                                    );
                                }
                                if p.title.is_none() && p.description.is_none() && p.thumbnail_url.is_none() {
                                    ui.label(
                                        RichText::new("No preview available")
                                            .small()
                                            .italics()
                                            .color(Color32::GRAY),
                                    );
                                }
                                // Domain footer
                                let host = url_hostname(url);
                                ui.label(
                                    RichText::new(host)
                                        .small()
                                        .color(Color32::from_rgb(100, 180, 255)),
                                );
                            }
                        }
                    });
                });
            }
            Span::Badge { name, .. } => {
                ui.add(Label::new(
                    RichText::new(format!("[{name}]"))
                        .color(Color32::GRAY)
                        .font(t::small()),
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

/// Extract just the hostname from a URL for display (e.g. `"youtube.com"`).
fn url_hostname(url: &str) -> String {
    let s = url.trim_start_matches("https://")
               .trim_start_matches("http://");
    let host = s.split('/').next().unwrap_or(s);
    // Strip www. prefix for cleanliness
    host.trim_start_matches("www.").to_owned()
}

/// Return `(label_text, stripe_color)` for messages with a chat notification.
/// Returns `None` for ordinary messages.
fn notification_label(flags: &MessageFlags, kind: &MsgKind) -> Option<(&'static str, Color32)> {
    if flags.is_highlighted {
        Some(("Highlighted Message", Color32::from_rgb(145, 70, 255)))
    } else if flags.is_mention {
        Some(("Mention", Color32::from_rgb(210, 140, 40)))
    } else if matches!(kind, MsgKind::Bits { .. }) {
        Some(("Bits Cheer", Color32::from_rgb(255, 175, 30)))
    } else if flags.is_first_msg {
        Some(("First Message", Color32::from_rgb(60, 160, 100)))
    } else if flags.custom_reward_id.is_some() {
        Some(("Channel Points Reward", Color32::from_rgb(100, 65, 165)))
    } else {
        None
    }
}
