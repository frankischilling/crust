use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use egui::{Color32, Id, Label, LayerId, Order, RichText, ScrollArea, Ui, Vec2};
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
        Self {
            messages,
            emote_bytes,
            cmd_tx,
            channel,
            link_previews,
        }
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
        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());

        // If the user scrolls over the message panel, immediately pause
        // stick-to-bottom so upward wheel input can take effect this frame.
        let wheel_over_panel = ui.ctx().input(|i| {
            let over_panel = i
                .pointer
                .hover_pos()
                .map(|p| panel_rect.contains(p))
                .unwrap_or(false);
            over_panel && i.raw_scroll_delta.y.abs() > 0.0
        });
        // Store whether the wheel was used this frame, so show_resume_button
        // doesn't immediately clear the paused flag before the delta is applied.
        let wheel_key = egui::Id::new("scroll_wheel_this_frame").with(self.channel.as_str());
        ui.ctx().data_mut(|d| d.insert_temp(wheel_key, wheel_over_panel));
        if wheel_over_panel {
            ui.ctx().data_mut(|d| d.insert_temp(paused_key, true));
        }

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
        //
        // PERF: use remove_temp (ownership via mem::take) instead of get_temp
        // (which clones the entire HashMap every frame).  The cache is put
        // back via insert_temp at the end of each path.
        let hc_id = egui::Id::new("msg_row_h").with(self.channel.as_str());
        let mut height_cache: std::collections::HashMap<u64, f32> =
            ui.ctx().data_mut(|d| d.remove_temp(hc_id).unwrap_or_default());

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
            ui.ctx()
                .data_mut(|d| d.insert_temp::<bool>(init_key, false));
        }

        // Scroll-to-reply target
        // Written by the reply-header click handler; read and cleared here so
        // it only fires once.
        let scroll_to_key = egui::Id::new("ml_scroll_to").with(self.channel.as_str());
        let highlight_key = egui::Id::new("ml_highlight_msg").with(self.channel.as_str());
        let highlight_time_key = egui::Id::new("ml_highlight_t").with(self.channel.as_str());
        let forced_offset: Option<f32> = {
            let target: Option<String> = ui.ctx().data_mut(|d| {
                let v: Option<String> = d.get_temp(scroll_to_key);
                if v.is_some() {
                    d.remove::<String>(scroll_to_key);
                }
                v
            });
            target.and_then(|tgt_id| {
                let idx = self
                    .messages
                    .iter()
                    .position(|m| m.server_id.as_deref() == Some(tgt_id.as_str()))?;
                // Store the target server_id + time for a brief highlight flash.
                let now = ui.input(|i| i.time);
                ui.ctx().data_mut(|d| {
                    d.insert_temp(highlight_key, tgt_id.clone());
                    d.insert_temp(highlight_time_key, now);
                });
                let offset: f32 = (0..idx)
                    .map(|i| {
                        height_cache
                            .get(&self.messages[i].id.0)
                            .copied()
                            .unwrap_or(EST_H)
                    })
                    .sum();
                Some(offset)
            })
        };

        if n < VIRTUAL_THRESHOLD {
            // ── Simple path: render every message, let egui handle layout ─
            // We also measure row heights here so the cache is pre-populated
            // when the channel crosses VIRTUAL_THRESHOLD.
            let scroll_paused: bool = ui
                .ctx()
                .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
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
            self.show_resume_button(ui, &output, panel_rect);
            self.apply_snap(ui, &output);
            return MessageListResult {
                reply: self.take_reply(ui, reply_key),
                profile_request: self.take_profile_request(ui),
            };
        }

        // Build prefix-sum array.  prefix[i] = y-offset of the top of message i.
        // PERF: reuse the previous Vec allocation via remove_temp (ownership
        // transfer) so we don't hit the allocator every frame.
        let ps_id = egui::Id::new("msg_prefix_sum").with(self.channel.as_str());
        let mut prefix: Vec<f32> =
            ui.ctx().data_mut(|d| d.remove_temp(ps_id).unwrap_or_default());
        prefix.clear();
        prefix.reserve(n + 1);
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
        let scroll_paused: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
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
                ui.allocate_exact_size(egui::Vec2::new(full_width, tail), egui::Sense::hover());
            }
        });

        // Persist height cache and prefix-sum Vec for next frame.
        ui.ctx().data_mut(|d| {
            d.insert_temp(hc_id, height_cache);
            d.insert_temp(ps_id, prefix);
        });

        self.show_resume_button(ui, &output, panel_rect);
        self.apply_snap(ui, &output);
        MessageListResult {
            reply: self.take_reply(ui, reply_key),
            profile_request: self.take_profile_request(ui),
        }
    }

    /// Read and clear the reply request stored by a context menu during this frame.
    fn take_reply(&self, ui: &Ui, key: Id) -> Option<ReplyInfo> {
        ui.ctx().data_mut(|d| {
            let v: Option<ReplyInfo> = d.get_temp(key);
            if v.is_some() {
                d.remove::<ReplyInfo>(key);
            }
            v
        })
    }

    /// Read and clear the profile-request stored by a username click this frame.
    fn take_profile_request(&self, ui: &Ui) -> Option<(String, Vec<Badge>)> {
        let key = Id::new("ml_profile_req").with(self.channel.as_str());
        ui.ctx().data_mut(|d| {
            let v: Option<(String, Vec<Badge>)> = d.get_temp(key);
            if v.is_some() {
                d.remove::<(String, Vec<Badge>)>(key);
            }
            v
        })
    }

    fn reply_info_for_message(msg: &ChatMessage) -> Option<ReplyInfo> {
        if msg.flags.is_deleted || !msg.channel.is_twitch() {
            return None;
        }
        let parent_msg_id = msg.server_id.clone()?.trim().to_owned();
        if parent_msg_id.is_empty() {
            return None;
        }
        Some(ReplyInfo {
            parent_msg_id,
            parent_user_login: msg.sender.login.clone(),
            parent_display_name: msg.sender.display_name.clone(),
            parent_msg_body: msg.raw_text.clone(),
        })
    }

    fn show_message_context_menu(&self, ui: &mut Ui, msg: &ChatMessage, reply_key: Id) {
        if let Some(info) = Self::reply_info_for_message(msg) {
            if ui.button("↩  Reply").clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(reply_key, info));
                ui.close_menu();
            }
        } else {
            let hint = if msg.flags.is_deleted {
                "Cannot reply to deleted messages"
            } else if !msg.channel.is_twitch() {
                "Inline replies are currently supported for Twitch messages only"
            } else {
                "Cannot reply to this message yet (missing message id)"
            };
            ui.add_enabled(false, egui::Button::new("↩  Reply"))
                .on_hover_text(hint);
        }

        ui.separator();
        if ui.button("📋  Copy message").clicked() {
            ui.ctx().copy_text(msg.raw_text.clone());
            ui.close_menu();
        }
        if ui.button("👤  Copy username").clicked() {
            ui.ctx().copy_text(msg.sender.login.clone());
            ui.close_menu();
        }
    }

    /// If the snap-to-bottom flag is active, force the scroll offset to the
    /// current real maximum every frame until `stick_to_bottom` takes over.
    fn apply_snap(&self, ui: &mut Ui, output: &egui::scroll_area::ScrollAreaOutput<()>) {
        let snap_key = Id::new("snap_to_bottom").with(self.channel.as_str());
        let snapping: bool = ui.ctx().data_mut(|d| d.get_temp(snap_key).unwrap_or(false));
        if !snapping {
            return;
        }

        // If user has scrolled up, never keep forcing snap-to-bottom.
        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
        let scroll_paused: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
        if scroll_paused {
            ui.ctx().data_mut(|d| d.insert_temp(snap_key, false));
            return;
        }

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
        let wheel_key = egui::Id::new("scroll_wheel_this_frame").with(self.channel.as_str());
        let viewport_h = output.inner_rect.height();
        let max_scroll = (output.content_size.y - viewport_h).max(0.0);
        let at_bottom = max_scroll < 1.0 || output.state.offset.y >= max_scroll - 20.0;

        // Keep the paused flag in sync with where the scroll actually is.
        // When the wheel was used this frame, never clear the flag - the
        // scroll delta may not have been applied yet, so at_bottom could
        // still read as true even though the user just scrolled up.
        let wheel_this_frame: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(wheel_key).unwrap_or(false));
        if !wheel_this_frame {
            ui.ctx()
                .data_mut(|d| d.insert_temp(paused_key, !at_bottom));
        }

        if !at_bottom {
            // Paint a floating button on a foreground layer (no Area/Window needed)
            let btn_size = egui::vec2(170.0, 28.0);
            let btn_center = egui::pos2(panel_rect.center().x, panel_rect.bottom() - 36.0);
            let btn_rect = egui::Rect::from_center_size(btn_center, btn_size);

            let fg_layer = LayerId::new(Order::Foreground, Id::new("resume_scroll_layer").with(self.channel.as_str()));
            let painter = ui.ctx().layer_painter(fg_layer);

            // Button background
            painter.rect_filled(btn_rect, 8.0, t::accent_dim());
            // Subtle border for definition
            painter.rect_stroke(btn_rect, 8.0, egui::Stroke::new(1.0, t::accent()), egui::epaint::StrokeKind::Outside);
            // Button label
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                "↓ Resume scrolling",
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );

            // Detect click on the painted rect
            let btn_response =
                ui.interact(btn_rect, Id::new("resume_scroll_btn").with(self.channel.as_str()), egui::Sense::click());
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
            _ => {
                self.render_system_event(ui, msg);
                return;
            }
        }

        let reply_key = Id::new("ml_reply_req").with(self.channel.as_str());
        let scroll_to_key = egui::Id::new("ml_scroll_to").with(self.channel.as_str());

        // ── Message background ──────────────────────────────────────────
        // Check if this message is the reply-scroll highlight target.
        let highlight_key = egui::Id::new("ml_highlight_msg").with(self.channel.as_str());
        let highlight_time_key = egui::Id::new("ml_highlight_t").with(self.channel.as_str());
        let highlight_alpha: f32 = {
            let hl_id: Option<String> = ui.ctx().data_mut(|d| d.get_temp(highlight_key));
            let hl_time: Option<f64> = ui.ctx().data_mut(|d| d.get_temp(highlight_time_key));
            match (hl_id, hl_time) {
                (Some(id), Some(t0))
                    if msg.server_id.as_deref() == Some(id.as_str()) =>
                {
                    let now = ui.input(|i| i.time);
                    let elapsed = (now - t0) as f32;
                    const FLASH_SECS: f32 = 1.5;
                    if elapsed < FLASH_SECS {
                        // Keep repainting while the flash is visible.
                        ui.ctx().request_repaint();
                        1.0 - (elapsed / FLASH_SECS)
                    } else {
                        // Flash complete — clean up temp data.
                        ui.ctx().data_mut(|d| {
                            d.remove::<String>(highlight_key);
                            d.remove::<f64>(highlight_time_key);
                        });
                        0.0
                    }
                }
                _ => 0.0,
            }
        };
        let bg = if highlight_alpha > 0.0 {
            let a = (50.0 * highlight_alpha) as u8;
            Color32::from_rgba_unmultiplied(100, 140, 255, a)
        } else if msg.flags.is_highlighted {
            Color32::from_rgba_unmultiplied(145, 70, 255, 22)
        } else if msg.flags.is_mention {
            Color32::from_rgba_unmultiplied(210, 140, 40, 24)
        } else if msg.flags.is_deleted {
            Color32::from_rgba_unmultiplied(180, 30, 30, 12)
        } else if matches!(msg.msg_kind, MsgKind::Bits { .. }) {
            Color32::from_rgba_unmultiplied(255, 175, 30, 14)
        } else if msg.flags.custom_reward_id.is_some() {
            Color32::from_rgba_unmultiplied(100, 65, 165, 16)
        } else {
            Color32::TRANSPARENT
        };

        // Context-menu approach:
        //
        // The Frame is registered FIRST on the layer (before any inner
        // widgets), so it has the LOWEST hit-test priority.  After
        // Frame::end(), we augment the frame's response with Sense::click()
        // via Response::interact().  Egui OR's the Sense and updates the
        // widget in-place (same index).  Result: inner widgets (username,
        // URL links) still win primary/secondary clicks in their rects,
        // but right-clicks on the message body (text, emotes, empty space)
        // fall through to the frame and open the context menu.

        // Push a stable ID derived from the message's own identifier so that
        // every inner widget (username label, badge images, emote images, URL
        // links) keeps the same egui widget-ID across frames.  Without this,
        // virtual-scrolling shifts the auto-ID counter whenever new messages
        // arrive and the dead-space allocation above the visible window
        // changes, causing click-press and click-release to see different IDs
        // and silently dropping the click event.
        ui.push_id(msg.id.0, |ui| {

        let mut prepared = egui::Frame::new()
            .fill(bg)
            .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
            .begin(ui);
        // Register a background click sensor EARLY — before any inner widgets —
        // so it gets the lowest idx_in_layer and thus the lowest hit-test
        // priority.  Inner widgets (reply header, username label, emotes, URLs)
        // are registered afterwards and therefore *win* the hit test when the
        // pointer is over them.  Only clicks on "empty" message space (padding,
        // gaps) fall through to this background widget to open the context menu.
        //
        // After Frame::end() we re-register with the SAME id and the actual
        // frame rect; `WidgetRects::insert` updates the rect in-place while
        // keeping the original (low) idx_in_layer.
        let bg_click_id = Id::new("msg_bg_click").with(msg.id.0);
        {
            let ui = &mut prepared.content_ui;
            // Use a zero-size rect for the early placeholder so that the
            // second interact() (with the real frame rect) doesn't trigger
            // egui's ID-clash warning.  The zero rect is fully contained
            // within the final frame rect, so `check_for_id_clash` treats
            // them as the same widget.  The key property we care about —
            // low `idx_in_layer` for hit-test priority — is preserved
            // because the widget is still registered before inner widgets.
            let placeholder_rect =
                egui::Rect::from_min_size(ui.max_rect().left_top(), egui::Vec2::ZERO);
            ui.interact(placeholder_rect, bg_click_id, egui::Sense::click());

            // Keep selectable_labels off globally so timestamp / badge
            // chip / separator labels stay non-interactive.  Text spans
            // opt-in to selection via `.selectable(true)` and each has
            // its own `.context_menu()` so right-click → Reply still works
            // even when the label wins the hit test.
            ui.style_mut().interaction.selectable_labels = false;

            // History messages are rendered at reduced opacity so they
            // read as older context while still being fully legible.
            if msg.flags.is_history {
                ui.set_opacity(0.55);
            }
            if let Some(ref rep) = msg.reply {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // Accent left stripe
                    let (stripe, _) =
                        ui.allocate_exact_size(egui::vec2(2.0, 12.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(stripe, 0.0, Color32::from_rgb(100, 65, 190));
                    let body = if rep.parent_msg_body.chars().count() > 80 {
                        // Find the byte offset of the 80th char boundary.
                        let cut = rep
                            .parent_msg_body
                            .char_indices()
                            .nth(80)
                            .map(|(i, _)| i)
                            .unwrap_or(rep.parent_msg_body.len());
                        format!("{}…", &rep.parent_msg_body[..cut])
                    } else {
                        rep.parent_msg_body.clone()
                    };
                    let reply_color = if t::is_light() {
                        Color32::from_rgb(90, 90, 115)
                    } else {
                        Color32::from_rgb(130, 130, 155)
                    };
                    let h = ui.add(
                        Label::new(
                            RichText::new(format!("↩ @{}: {}", rep.parent_display_name, body))
                                .font(t::small())
                                .color(reply_color)
                                .italics(),
                        )
                        .sense(egui::Sense::click())
                        .truncate(),
                    );
                    h.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
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
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 1.0, stripe_color);
                    ui.add(Label::new(
                        RichText::new(label).font(t::small()).color(stripe_color),
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
                    let ts = msg
                        .timestamp
                        .with_timezone(&chrono::Local)
                        .format("%H:%M")
                        .to_string();
                    ui.add(Label::new(
                        RichText::new(ts)
                            .color(t::timestamp())
                            .font(t::small()),
                    ));

                    // Separator dot between timestamp and badges/name
                    ui.add(Label::new(
                        RichText::new("·")
                            .color(t::separator())
                            .font(t::small()),
                    ));

                    // Badges: image if loaded, else text fallback
                    for badge in &msg.sender.badges {
                        let tooltip_label = pretty_badge_name(&badge.name, &badge.version);
                        if let Some(url) = &badge.url {
                            if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                                let size = fit_size(w, h, BADGE_SIZE);
                                let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
                                let url_key = super::bytes_uri(url, raw);
                                // Closures capture by reference - clones
                                // only happen when the tooltip is actually
                                // shown (on hover), not every frame.
                                self.show_image(ui, &url_key, raw, size)
                                    .on_hover_ui_at_pointer(|ui| {
                                        ui.set_max_width(200.0);
                                        ui.vertical_centered(|ui| {
                                            ui.add(
                                                egui::Image::from_bytes(
                                                    url_key.clone(),
                                                    egui::load::Bytes::Shared(raw.clone()),
                                                )
                                                .fit_to_exact_size(tooltip_size),
                                            );
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(&tooltip_label).strong());
                                        });
                                    })
                                    .context_menu(|ui| {
                                        self.show_message_context_menu(ui, msg, reply_key);
                                    });
                                continue;
                            }
                        }
                        render_badge_fallback(ui, &badge.name, &badge.version, &tooltip_label);
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
                            Label::new(RichText::new(name).color(name_color).strong())
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_ui(|ui| {
                            ui.label(format!("@{}", msg.sender.login));
                        });
                    name_resp
                        .context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
                    if name_resp.clicked_by(egui::PointerButton::Primary) {
                        // Clone only when clicked - not every frame.
                        let _ = self.cmd_tx.try_send(AppCommand::ShowUserCard {
                            login: msg.sender.login.clone(),
                            channel: self.channel.clone(),
                        });
                        let key = Id::new("ml_profile_req").with(self.channel.as_str());
                        ui.ctx().data_mut(|d| {
                            d.insert_temp(
                                key,
                                (msg.sender.login.clone(), msg.sender.badges.clone()),
                            );
                        });
                    }
                    if name_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }

                    // Colon separator after name (not shown for /me actions)
                    if !msg.flags.is_action {
                        ui.add(Label::new(
                            RichText::new(":").color(t::separator()),
                        ));
                    }

                    // Message spans carry their own whitespace from the
                    // tokenizer, so keep inter-widget spacing at zero to
                    // avoid rendering words with visually doubled spaces.
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;

                        // For deleted messages show the original content
                        // with strikethrough so moderator actions are
                        // visible without being prominent.
                        if msg.flags.is_deleted {
                            ui.add(
                                Label::new(
                                    RichText::new(format!("✂ {}", msg.raw_text))
                                        .strikethrough()
                                        .italics()
                                        .color(t::text_muted()),
                                )
                                .wrap(),
                            );
                        } else {
                            for span in &msg.spans {
                                self.render_span(ui, span, msg.flags.is_action, msg, reply_key);
                            }
                        }
                    });
                },
            );
        }
        let msg_frame_resp = prepared.end(ui);

        // Re-register the background click widget with the actual frame rect.
        // Same `bg_click_id` → updates in-place, keeping low hit-test priority.
        let bg_click = ui.interact(msg_frame_resp.rect, bg_click_id, egui::Sense::click());
        bg_click.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));

        // Left accent strip for mentions and highlights - a vivid 3 px bar on
        // the left edge of the row so the eye finds them instantly in fast chat.
        if msg.flags.is_mention || msg.flags.is_highlighted {
            let r = msg_frame_resp.rect;
            let bar_col = if msg.flags.is_mention
                && matches!(msg.msg_kind, MsgKind::Sub { is_gift: true, .. })
            {
                t::raid_cyan()
            } else if msg.flags.is_mention {
                t::accent()
            } else {
                Color32::from_rgb(255, 210, 30)
            };
            let strip = egui::Rect::from_min_size(r.left_top(), egui::vec2(3.0, r.height()));
            ui.painter().rect_filled(strip, 0.0, bar_col);
        }

        }); // end push_id
    }

    /// Render a compact system-event row (mod action, sub alert, raid, notice).
    /// These are centred italic lines with a coloured left stripe and icon,
    /// similar to Chatterino's system-message style.
    fn render_system_event(&self, ui: &mut Ui, msg: &ChatMessage) {
        let (accent, label_override): (Color32, Option<String>) = match &msg.msg_kind {
            MsgKind::Sub {
                display_name,
                months,
                plan,
                is_gift,
                ..
            } => {
                let gifted_to_me = *is_gift && msg.flags.is_mention;
                let text = if gifted_to_me {
                    format!("🎉🎊  You received a gifted {plan} sub! ({months} months)")
                } else if *is_gift {
                    format!("🎁  {display_name} received a gifted {plan} sub! ({months} months)")
                } else if *months <= 1 {
                    format!("⭐  {display_name} subscribed with {plan}!")
                } else {
                    format!("⭐  {display_name} resubscribed with {plan}! ({months} months)")
                };
                (if gifted_to_me { t::raid_cyan() } else { t::gold() }, Some(text))
            }
            MsgKind::Raid {
                display_name,
                viewer_count,
            } => (
                t::raid_cyan(),
                Some(format!(
                    "🎉  {display_name} is raiding with {viewer_count} viewers!"
                )),
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
            MsgKind::Ban { login } => {
                (t::red(), Some(format!("🚫  {login} was permanently banned.")))
            }
            MsgKind::ChatCleared => (
                if t::is_light() {
                    Color32::from_rgb(100, 90, 120)
                } else {
                    Color32::from_rgb(130, 120, 150)
                },
                Some("🗑  Chat was cleared by a moderator.".to_owned()),
            ),
            MsgKind::SystemInfo => {
                let (color, text) = style_system_info_text(&msg.raw_text);
                (color, Some(text))
            }
            _ => (Color32::from_rgb(100, 100, 120), Some(msg.raw_text.clone())),
        };

        let text = label_override.unwrap_or_else(|| msg.raw_text.clone());
        let opacity = if msg.flags.is_history { 0.55 } else { 1.0 };

        // Push a stable ID derived from the message's own identifier so that
        // widget IDs inside the system-event row are stable regardless of
        // where this message falls in the virtual-scroll window.  Without
        // this, the auto-incremented IDs shift every frame as the visible
        // range moves, causing egui to report widget ID clashes for every
        // system event in the loaded history.
        ui.push_id(msg.id.0, |ui| {
        egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(
                accent.r(),
                accent.g(),
                accent.b(),
                10,
            ))
            .inner_margin(egui::Margin::symmetric(
                ROW_PAD_X as i8,
                ROW_PAD_Y as i8 + 1,
            ))
            .show(ui, |ui| {
                if msg.flags.is_history {
                    ui.set_opacity(opacity);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // Coloured left stripe
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 1.0, accent);

                    // Timestamp
                    let ts = msg.timestamp.format("%H:%M").to_string();
                    ui.add(Label::new(
                        RichText::new(ts)
                            .color(t::timestamp())
                            .font(t::small()),
                    ));

                    // Message text
                    let rich = if is_irc_motd_line(&text) {
                        RichText::new(text).color(accent).font(t::small())
                    } else {
                        RichText::new(text).italics().color(accent).font(t::small())
                    };
                    ui.add(Label::new(rich).wrap());
                });
            });
        }); // end push_id
    }

    fn render_span(
        &self,
        ui: &mut Ui,
        span: &Span,
        is_action: bool,
        msg: &ChatMessage,
        reply_key: Id,
    ) {
        let action_color = if t::is_light() {
            Color32::from_rgb(80, 80, 110)
        } else {
            Color32::from_rgb(180, 180, 210)
        };
        match span {
            Span::Text { text, .. } => {
                let cleaned = strip_invisible_chars(text);
                if cleaned.is_empty() {
                    return;
                }
                let rt = if is_action {
                    RichText::new(&cleaned).italics().color(action_color)
                } else {
                    RichText::new(&cleaned)
                };
                let resp = ui.add(Label::new(rt).wrap().selectable(true));
                resp.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
            }
            Span::Emote {
                url,
                url_hd,
                code,
                provider,
                ..
            } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let url_key = super::bytes_uri(url, raw);

                    // Capture shared references - string/Arc clones only
                    // happen when the tooltip is actually shown (on hover).
                    let emote_bytes = self.emote_bytes; // &HashMap - Copy
                    let cmd_tx = self.cmd_tx; // &Sender  - Copy

                    self.show_image(ui, &url_key, raw, size)
                        .on_hover_ui_at_pointer(|ui| {
                        // Check HD availability at hover time, not every frame.
                        let hd_entry = url_hd.as_deref().and_then(|u| emote_bytes.get(u));

                        // Fire HD fetch once on first hover if not yet loaded.
                        if hd_entry.is_none() {
                            if let Some(hd_url) = url_hd.as_deref() {
                                let _ = cmd_tx.try_send(AppCommand::FetchImage {
                                    url: hd_url.to_owned(),
                                });
                            }
                        }

                        let (tt_key, tt_raw, tt_w, tt_h) = match hd_entry {
                            Some(&(hw, hh, ref href)) => (
                                super::bytes_uri(url_hd.as_deref().unwrap(), href),
                                href.clone(),
                                hw,
                                hh,
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
                    })
                    .context_menu(|ui| {
                        self.show_message_context_menu(ui, msg, reply_key);
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
                    let url_key = super::bytes_uri(url, raw);
                    self.show_image(ui, &url_key, raw, size)
                        .on_hover_ui_at_pointer(|ui| {
                            ui.set_max_width(200.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                                    egui::Image::from_bytes(
                                        url_key.clone(),
                                        egui::load::Bytes::Shared(raw.clone()),
                                    )
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
                        })
                        .context_menu(|ui| {
                            self.show_message_context_menu(ui, msg, reply_key);
                        });
                } else {
                    ui.add(Label::new(RichText::new(text)));
                }
            }
            Span::Mention { login } => {
                let resp = ui.add(Label::new(
                    RichText::new(format!("@{login}"))
                        .color(t::mention())
                        .strong(),
                ).selectable(true));
                resp.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
            }
            Span::Url { text, url } => {
                let cmd_tx = self.cmd_tx;
                let link_previews = self.link_previews;
                let emote_bytes = self.emote_bytes;

                // Render as a clickable hyperlink-style label.
                let resp = ui.add(
                    Label::new(
                        RichText::new(text)
                            .color(t::link())
                            .underline(),
                    )
                    .selectable(false)
                    .sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    let _ = cmd_tx.try_send(AppCommand::OpenUrl { url: url.clone() });
                }
                resp.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
                resp.on_hover_ui(|ui| {
                    // Fire preview fetch on first hover (idempotent in reducer).
                    let preview = link_previews.get(url.as_str());
                    if preview.map(|p| p.fetched).unwrap_or(false) == false {
                        let _ = cmd_tx.try_send(AppCommand::FetchLinkPreview { url: url.clone() });
                    }

                    ui.set_max_width(320.0);
                    ui.vertical(|ui| {
                        match preview {
                            None => {
                                // Not yet fetched - show hostname + spinner.
                                let host = url_hostname(url);
                                ui.label(
                                    RichText::new(host)
                                        .small()
                                        .color(t::link()),
                                );
                                ui.label(
                                    RichText::new("Loading preview…")
                                        .small()
                                        .italics()
                                        .color(Color32::GRAY),
                                );
                            }
                            Some(p) => {
                                // Site name badge (YouTube, Twitter, etc.)
                                if let Some(ref sn) = p.site_name {
                                    let (badge_bg, badge_fg) = site_badge_colors(sn);
                                    egui::Frame::new()
                                        .fill(badge_bg)
                                        .corner_radius(egui::CornerRadius::same(3))
                                        .inner_margin(egui::Margin::symmetric(5, 1))
                                        .show(ui, |ui| {
                                            ui.label(
                                                RichText::new(sn)
                                                    .small()
                                                    .strong()
                                                    .color(badge_fg),
                                            );
                                        });
                                    ui.add_space(3.0);
                                }
                                // Thumbnail
                                if let Some(ref thumb) = p.thumbnail_url {
                                    if let Some(&(w, h, ref raw)) = emote_bytes.get(thumb.as_str())
                                    {
                                        let scale = (170.0_f32 / h as f32).min(300.0 / w as f32);
                                        let size = Vec2::new(w as f32 * scale, h as f32 * scale);
                                        let key = super::bytes_uri(thumb, raw);
                                        egui::Frame::new()
                                            .corner_radius(egui::CornerRadius::same(4))
                                            .show(ui, |ui| {
                                                ui.add(
                                                    egui::Image::from_bytes(
                                                        key,
                                                        egui::load::Bytes::Shared(raw.clone()),
                                                    )
                                                    .fit_to_exact_size(size),
                                                );
                                            });
                                        ui.add_space(4.0);
                                    }
                                }
                                // Title
                                if let Some(ref title) = p.title {
                                    ui.add(Label::new(RichText::new(title).strong()).wrap());
                                }
                                // Description
                                if let Some(ref d) = p.description {
                                    let snippet = if d.chars().count() > 260 {
                                        let cut = d.char_indices().nth(260)
                                            .map(|(i, _)| i).unwrap_or(d.len());
                                        format!("{}\u{2026}", &d[..cut])
                                    } else {
                                        d.clone()
                                    };
                                    ui.add(Label::new(
                                        RichText::new(snippet)
                                            .small()
                                            .color(t::text_muted()),
                                    ).wrap());
                                }
                                if p.title.is_none()
                                    && p.description.is_none()
                                    && p.thumbnail_url.is_none()
                                {
                                    ui.label(
                                        RichText::new("No preview available")
                                            .small()
                                            .italics()
                                            .color(Color32::GRAY),
                                    );
                                }
                                // Domain footer
                                let host = url_hostname(url);
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(host)
                                        .small()
                                        .color(t::text_muted()),
                                );
                            }
                        }
                    });
                });
            }
            Span::Badge { name, .. } => {
                let tooltip = pretty_badge_name(name, "1");
                render_badge_fallback(ui, name, "1", &tooltip);
            }
        }
    }

    /// Render an image from raw bytes using egui's image loaders (supports GIF animation).
    ///
    /// Uses `Sense::click()` (not `Sense::hover()`) because egui 0.31's interaction
    /// system only adds widgets to the `hovered` IdSet when they are in `hits.click`
    /// or `hits.drag`.  A `Sense::hover()`-only widget has click=false/drag=false so
    /// it is never selected by hit-test, `response.hovered()` is always false, and
    /// `on_hover_ui_at_pointer` never fires.  Using `Sense::click()` ensures the image
    /// enters the hovered set so tooltips work.  We never actually handle the click
    /// on images – callers that want right-click menus chain `.context_menu()` themselves.
    fn show_image(&self, ui: &mut Ui, uri: &str, raw: &Arc<[u8]>, size: Vec2) -> egui::Response {
        ui.add(
            egui::Image::from_bytes(uri.to_owned(), egui::load::Bytes::Shared(raw.clone()))
                .sense(egui::Sense::click())
                .fit_to_exact_size(size),
        )
    }
}

/// Strip invisible / zero-width Unicode characters that render as squares.
/// Preserves normal whitespace (space, newline) but removes combining marks
/// that appear without a preceding base character, zero-width joiners/non-
/// joiners, direction overrides, and other control characters that most fonts
/// cannot render.
fn strip_invisible_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_is_base = false; // was the previous kept char a base character?
    for c in s.chars() {
        // Keep ASCII printable + common whitespace verbatim.
        if c == ' ' || c == '\n' || c == '\t' {
            prev_is_base = false;
            out.push(c);
            continue;
        }
        // Drop C0/C1 control characters (except the whitespace above).
        if c.is_control() {
            continue;
        }
        let cp = c as u32;
        // Zero-width and invisible formatting characters - always drop.
        if matches!(
            cp,
            0x00AD             // Soft Hyphen
            | 0x034F           // Combining Grapheme Joiner
            | 0x061C           // Arabic Letter Mark
            | 0x180E           // Mongolian Vowel Separator
            | 0x200B           // Zero Width Space
            | 0x200C           // Zero Width Non-Joiner
            | 0x200D           // Zero Width Joiner (outside emoji context)
            | 0x200E           // LTR Mark
            | 0x200F           // RTL Mark
            | 0x2028           // Line Separator
            | 0x2029           // Paragraph Separator
            | 0x202A..=0x202E  // LTR/RTL embedding/override/pop
            | 0x2060           // Word Joiner
            | 0x2061..=0x2064  // Invisible operators
            | 0x2066..=0x2069  // Isolate formatting
            | 0x206A..=0x206F  // Deprecated formatting
            | 0x2800           // Braille Pattern Blank
            | 0x3164           // Hangul Filler
            | 0xFE00..=0xFE0F  // Variation Selectors
            | 0xFEFF           // BOM / Zero Width No-Break Space
            | 0xFFA0           // Halfwidth Hangul Filler
            | 0xFFF9..=0xFFFB  // Interlinear annotations
            | 0xE0000..=0xE007F // Tags block
            | 0xE0100..=0xE01EF // Variation Selectors Supplement
        ) {
            continue;
        }
        // Combining marks: keep them only when they follow a base character,
        // otherwise they render as standalone squares / dotted circles.
        let is_combining = matches!(
            cp,
            0x0300..=0x036F    // Combining Diacritical Marks
            | 0x0483..=0x0489  // Combining Cyrillic
            | 0x0591..=0x05BD  // Hebrew accents
            | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
            | 0x0610..=0x061A  // Arabic combining
            | 0x064B..=0x065F  // Arabic combining marks
            | 0x0670           // Arabic superscript alef
            | 0x06D6..=0x06DC  // Arabic small marks
            | 0x06DF..=0x06E4
            | 0x06E7..=0x06E8
            | 0x06EA..=0x06ED
            | 0x0730..=0x074A  // Syriac combining
            | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E  // Thai
            | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECE  // Lao
            | 0x1AB0..=0x1AFF  // Combining Diacritical Marks Extended
            | 0x1DC0..=0x1DFF  // Combining Diacritical Marks Supplement
            | 0x20D0..=0x20FF  // Combining Marks for Symbols
            | 0xFE20..=0xFE2F  // Combining Half Marks
        );
        if is_combining {
            if prev_is_base {
                out.push(c);
            }
            // Whether kept or dropped, the next char still has a base before it
            // (we don't reset prev_is_base so stacked diacritics work).
            continue;
        }
        // Everything else is a visible base character - keep it.
        prev_is_base = true;
        out.push(c);
    }
    out
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
            let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
            if t::is_light() {
                // Darken colours that are too bright for a light background.
                if lum > 170.0 {
                    let factor = 170.0 / lum.max(1.0);
                    let factor = factor.max(0.4);
                    Color32::from_rgb(
                        (r as f32 * factor) as u8,
                        (g as f32 * factor) as u8,
                        (b as f32 * factor) as u8,
                    )
                } else {
                    Color32::from_rgb(r, g, b)
                }
            } else {
                // Boost dim colours for dark backgrounds.
                if lum < 70.0 {
                    let factor = 70.0 / lum.max(1.0);
                    let factor = factor.min(2.5);
                    Color32::from_rgb(
                        (r as f32 * factor).min(255.0) as u8,
                        (g as f32 * factor).min(255.0) as u8,
                        (b as f32 * factor).min(255.0) as u8,
                    )
                } else {
                    Color32::from_rgb(r, g, b)
                }
            }
        })
        .unwrap_or(t::accent())
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

fn render_badge_fallback(ui: &mut Ui, name: &str, version: &str, tooltip: &str) {
    let (bg, fg) = badge_chip_colors(name);
    let chip_text = badge_chip_text(name, version);
    let response = egui::Frame::new()
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.add(Label::new(
                RichText::new(&chip_text).font(t::small()).color(fg).strong(),
            ));
        })
        .response;
    response.on_hover_ui_at_pointer(|ui| {
        ui.vertical_centered(|ui| {
            ui.add(Label::new(
                RichText::new(&chip_text).size(18.0).color(fg).strong(),
            ));
            ui.add_space(4.0);
            ui.label(RichText::new(tooltip).strong());
        });
    });
}

fn badge_chip_text(name: &str, version: &str) -> String {
    match name {
        "subscriber" => {
            if let Ok(months) = version.parse::<u32>() {
                if months > 1 {
                    return format!("SUB{months}");
                }
            }
            "SUB".to_owned()
        }
        "sub_gifter" => "GIFT".to_owned(),
        "founder" => "FND".to_owned(),
        "moderator" => "MOD".to_owned(),
        "broadcaster" => "LIVE".to_owned(),
        "vip" => "VIP".to_owned(),
        "verified" => "VER".to_owned(),
        "staff" => "STAFF".to_owned(),
        _ => {
            let first = name
                .split(|c: char| c == '-' || c == '_')
                .find(|part| !part.is_empty())
                .unwrap_or(name);
            first.chars().take(5).collect::<String>().to_uppercase()
        }
    }
}

fn badge_chip_colors(name: &str) -> (Color32, Color32) {
    let light = t::is_light();
    match name {
        "subscriber" => if light {
            (Color32::from_rgb(210, 246, 218), Color32::from_rgb(32, 66, 38))
        } else {
            (Color32::from_rgb(52, 86, 58), Color32::from_rgb(210, 246, 218))
        },
        "sub_gifter" => if light {
            (Color32::from_rgb(252, 232, 180), Color32::from_rgb(64, 47, 14))
        } else {
            (Color32::from_rgb(84, 67, 34), Color32::from_rgb(252, 222, 154))
        },
        "founder" => if light {
            (Color32::from_rgb(215, 218, 255), Color32::from_rgb(40, 42, 75))
        } else {
            (Color32::from_rgb(60, 62, 95), Color32::from_rgb(200, 206, 255))
        },
        "moderator" => if light {
            (Color32::from_rgb(200, 244, 217), Color32::from_rgb(23, 69, 39))
        } else {
            (Color32::from_rgb(43, 89, 59), Color32::from_rgb(196, 244, 217))
        },
        "broadcaster" => if light {
            (Color32::from_rgb(255, 220, 220), Color32::from_rgb(82, 25, 25))
        } else {
            (Color32::from_rgb(102, 45, 45), Color32::from_rgb(255, 206, 206))
        },
        "vip" => if light {
            (Color32::from_rgb(255, 220, 248), Color32::from_rgb(92, 37, 78))
        } else {
            (Color32::from_rgb(112, 57, 98), Color32::from_rgb(255, 206, 242))
        },
        "verified" => if light {
            (Color32::from_rgb(210, 235, 255), Color32::from_rgb(26, 48, 87))
        } else {
            (Color32::from_rgb(46, 68, 107), Color32::from_rgb(191, 223, 255))
        },
        "staff" => if light {
            (Color32::from_rgb(228, 234, 240), Color32::from_rgb(56, 64, 72))
        } else {
            (Color32::from_rgb(76, 84, 92), Color32::from_rgb(220, 226, 233))
        },
        _ => if light {
            (Color32::from_rgb(220, 220, 228), Color32::from_rgb(50, 50, 54))
        } else {
            (Color32::from_rgb(70, 70, 74), Color32::from_rgb(210, 210, 215))
        },
    }
}

fn style_system_info_text(raw: &str) -> (Color32, String) {
    let s = raw.trim();
    let Some((code, payload)) = parse_bracket_numeric_prefix(s) else {
        return (Color32::from_rgb(120, 125, 145), s.to_owned());
    };

    match code {
        "375" => (
            Color32::from_rgb(130, 165, 220),
            format!("IRC MOTD: {}", payload.trim()),
        ),
        "372" => (
            Color32::from_rgb(145, 150, 170),
            format!("  {}", payload.trim()),
        ),
        "376" => (
            Color32::from_rgb(120, 175, 135),
            "IRC MOTD complete".to_owned(),
        ),
        "001" => (Color32::from_rgb(120, 195, 145), payload.trim().to_owned()),
        "002" | "003" | "004" | "005" => {
            (Color32::from_rgb(125, 165, 210), payload.trim().to_owned())
        }
        "251" | "252" | "253" | "254" | "255" | "265" | "266" | "250" => {
            (Color32::from_rgb(140, 155, 175), payload.trim().to_owned())
        }
        _ => (Color32::from_rgb(120, 125, 145), s.to_owned()),
    }
}

fn parse_bracket_numeric_prefix(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('[')?;
    let end = rest.find(']')?;
    let code = &rest[..end];
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let payload = rest[end + 1..].trim_start();
    Some((code, payload))
}

fn is_irc_motd_line(text: &str) -> bool {
    text.starts_with("IRC MOTD:") || text.starts_with("  ")
}

/// Map short provider codes to human-readable labels.
fn provider_label(provider: &str) -> &'static str {
    match provider {
        "bttv" => "BetterTTV",
        "ffz" => "FrankerFaceZ",
        "7tv" => "7TV",
        "twitch" => "Twitch",
        "kick" => "Kick",
        _ => "Emote",
    }
}

/// Extract just the hostname from a URL for display (e.g. `"youtube.com"`).
fn url_hostname(url: &str) -> String {
    let s = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = s.split('/').next().unwrap_or(s);
    // Strip www. prefix for cleanliness
    host.trim_start_matches("www.").to_owned()
}

/// Badge-style (background, foreground) colours for known site names shown
/// in the link-preview tooltip.  Roughly matches each site's brand colour.
fn site_badge_colors(site: &str) -> (Color32, Color32) {
    let light = t::is_light();
    match site {
        "YouTube" => if light {
            (Color32::from_rgb(255, 220, 220), Color32::from_rgb(180, 18, 18))
        } else {
            (Color32::from_rgb(120, 20, 20), Color32::from_rgb(255, 100, 100))
        },
        "Twitter" | "X" => if light {
            (Color32::from_rgb(210, 235, 255), Color32::from_rgb(20, 100, 175))
        } else {
            (Color32::from_rgb(20, 55, 90), Color32::from_rgb(100, 180, 255))
        },
        "Twitch" | "Twitch Clip" => if light {
            (Color32::from_rgb(230, 215, 255), Color32::from_rgb(100, 65, 165))
        } else {
            (Color32::from_rgb(60, 40, 100), Color32::from_rgb(190, 160, 255))
        },
        "Reddit" => if light {
            (Color32::from_rgb(255, 225, 210), Color32::from_rgb(200, 70, 20))
        } else {
            (Color32::from_rgb(100, 35, 10), Color32::from_rgb(255, 135, 80))
        },
        "GitHub" => if light {
            (Color32::from_rgb(225, 225, 235), Color32::from_rgb(40, 40, 50))
        } else {
            (Color32::from_rgb(40, 40, 50), Color32::from_rgb(210, 210, 220))
        },
        "Instagram" => if light {
            (Color32::from_rgb(255, 220, 235), Color32::from_rgb(175, 30, 100))
        } else {
            (Color32::from_rgb(90, 15, 50), Color32::from_rgb(255, 120, 175))
        },
        "TikTok" => if light {
            (Color32::from_rgb(230, 245, 250), Color32::from_rgb(20, 20, 30))
        } else {
            (Color32::from_rgb(20, 20, 30), Color32::from_rgb(230, 245, 250))
        },
        "Wikipedia" => if light {
            (Color32::from_rgb(230, 230, 230), Color32::from_rgb(50, 50, 50))
        } else {
            (Color32::from_rgb(50, 50, 55), Color32::from_rgb(220, 220, 225))
        },
        "Steam" => if light {
            (Color32::from_rgb(210, 220, 240), Color32::from_rgb(25, 40, 80))
        } else {
            (Color32::from_rgb(25, 35, 65), Color32::from_rgb(150, 180, 230))
        },
        _ => if light {
            (Color32::from_rgb(225, 230, 240), Color32::from_rgb(60, 65, 80))
        } else {
            (Color32::from_rgb(50, 55, 65), Color32::from_rgb(180, 185, 200))
        },
    }
}

/// Return `(label_text, stripe_color)` for messages with a chat notification.
/// Returns `None` for ordinary messages.
fn notification_label(flags: &MessageFlags, kind: &MsgKind) -> Option<(&'static str, Color32)> {
    if flags.is_highlighted {
        Some(("Highlighted Message", t::twitch_purple()))
    } else if flags.is_mention {
        Some(("Mention", Color32::from_rgb(210, 140, 40)))
    } else if matches!(kind, MsgKind::Bits { .. }) {
        Some(("Bits Cheer", t::bits_orange()))
    } else if flags.is_first_msg {
        Some(("First Message", t::green()))
    } else if flags.custom_reward_id.is_some() {
        Some(("Channel Points Reward", Color32::from_rgb(100, 65, 165)))
    } else {
        None
    }
}
