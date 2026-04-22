use std::collections::HashMap;

use egui::{Color32, Id, RichText, ScrollArea, Ui};

use crate::theme as t;
use crust_core::model::{ChannelId, ChannelState};

/// Left-sidebar channel list.
pub struct ChannelList<'a> {
    pub channels: &'a [ChannelId],
    pub active: Option<&'a ChannelId>,
    pub channel_states: &'a HashMap<ChannelId, ChannelState>,
    /// Optional map of channel-login → is_live for drawing live status dots.
    pub live_channels: Option<&'a HashMap<String, bool>>,
    pub show_live_indicator: bool,
    pub show_close_button: bool,
}

pub struct ChannelListResult {
    pub selected: Option<ChannelId>,
    pub closed: Option<ChannelId>,
    /// Set when the user dragged a tab to a new position; contains the full
    /// new ordered channel list.
    pub reordered: Option<Vec<ChannelId>>,
    /// Set when a channel is being dragged outside the sidebar bounds
    /// (pointer left the sidebar).  The app can use this to initiate a split.
    pub drag_split: Option<ChannelId>,
    /// True while a drag is actively in progress (for rendering drop-zone
    /// overlay in the central panel).
    pub dragging_outside: bool,
}

/// Persistent-per-frame drag tracking stored in egui temp storage.
#[derive(Clone)]
struct DragState {
    /// Index of the channel being dragged.
    dragging_idx: usize,
    /// Index *before* which the dragged item will be inserted (0 = top).
    insert_before: usize,
    /// True when the pointer has left the sidebar (split-drop mode).
    outside_sidebar: bool,
}

impl<'a> ChannelList<'a> {
    pub fn show(&mut self, ui: &mut Ui) -> ChannelListResult {
        let mut result = ChannelListResult {
            selected: None,
            closed: None,
            reordered: None,
            drag_split: None,
            dragging_outside: false,
        };

        let drag_id = Id::new("channel_list_drag");
        // Row height scales with chat font so channel labels don't clip.
        let row_h: f32 = (t::chat_font_size() + 14.0).max(28.0);
        let stride: f32 = row_h + t::CHANNEL_ROW_GAP;
        let n = self.channels.len();

        ScrollArea::vertical()
            .id_salt("channel_list")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.set_min_width(t::SIDEBAR_MIN_W - 16.0);
                ui.spacing_mut().item_spacing.y = t::CHANNEL_ROW_GAP;

                // Snapshot drag state at the start of the frame.
                let drag: Option<DragState> = ui.data(|d| d.get_temp(drag_id));

                // Y reference for computing insert position from pointer.
                let list_top = ui.cursor().min.y;

                for (idx, ch) in self.channels.iter().enumerate() {
                    let is_active = self.active == Some(ch);
                    let (unread_count, unread_mentions) = self
                        .channel_states
                        .get(ch)
                        .map(|s| (s.unread_count, s.unread_mentions))
                        .unwrap_or((0, 0));

                    // Draw insert-indicator line above this row when dragging.
                    if let Some(ref ds) = drag {
                        if ds.dragging_idx != idx && ds.insert_before == idx {
                            let y = ui.cursor().min.y - t::CHANNEL_ROW_GAP * 0.5;
                            let x_range = ui.max_rect().x_range();
                            ui.painter()
                                .hline(x_range, y, egui::Stroke::new(2.0, t::accent()));
                        }
                    }

                    let interact_id = egui::Id::new("ch_row").with(ch);

                    // Allocate the full-width row rect.
                    let row_rect = {
                        let avail = ui.available_rect_before_wrap();
                        egui::Rect::from_min_size(avail.min, egui::vec2(avail.width(), row_h))
                    };
                    let row_resp =
                        ui.interact(row_rect, interact_id, egui::Sense::click_and_drag());

                    // Drag start
                    if row_resp.drag_started() {
                        ui.data_mut(|d| {
                            d.insert_temp(
                                drag_id,
                                DragState {
                                    dragging_idx: idx,
                                    insert_before: idx,
                                    outside_sidebar: false,
                                },
                            )
                        });
                    }

                    // Drag update: recompute insert position + outside check
                    if row_resp.dragged() {
                        if let Some(pos) = ui.ctx().pointer_latest_pos() {
                            let sidebar_rect = ui.max_rect();
                            let is_outside = pos.x > sidebar_rect.right() + 30.0;
                            let rel_y = pos.y - list_top;
                            let new_insert = ((rel_y / stride + 0.5) as usize).min(n);
                            ui.data_mut(|d| {
                                let mut ds: DragState = d.get_temp(drag_id).unwrap_or(DragState {
                                    dragging_idx: idx,
                                    insert_before: idx,
                                    outside_sidebar: false,
                                });
                                ds.insert_before = new_insert;
                                ds.outside_sidebar = is_outside;
                                d.insert_temp(drag_id, ds);
                            });
                        }
                        ui.ctx().request_repaint();
                    }

                    // Drag release: split or reorder
                    if row_resp.drag_stopped() {
                        if let Some(ds) = ui.data(|d| d.get_temp::<DragState>(drag_id)) {
                            let sidebar_rect = ui.max_rect();
                            let outside = ui
                                .ctx()
                                .pointer_latest_pos()
                                .map(|p| p.x > sidebar_rect.right() + 30.0)
                                .unwrap_or(false);
                            if outside {
                                // Pointer released outside sidebar → split pane
                                result.drag_split = Some(self.channels[ds.dragging_idx].clone());
                            } else {
                                let raw = ds.insert_before;
                                let insert = if raw > ds.dragging_idx { raw - 1 } else { raw };
                                if insert != ds.dragging_idx {
                                    let mut new_order: Vec<ChannelId> = self.channels.to_vec();
                                    let moved = new_order.remove(ds.dragging_idx);
                                    new_order.insert(insert, moved);
                                    result.reordered = Some(new_order);
                                }
                            }
                        }
                        ui.data_mut(|d| d.remove::<DragState>(drag_id));
                    }

                    // Right-click menu for quick channel actions.
                    row_resp.context_menu(|ui| {
                        if ui
                            .button(RichText::new("Switch to channel").font(t::small()))
                            .clicked()
                        {
                            result.selected = Some(ch.clone());
                            ui.close_menu();
                        }

                        if ui
                            .button(RichText::new("Copy channel").font(t::small()))
                            .clicked()
                        {
                            let copy = if ch.is_kick() {
                                format!("kick:{}", ch.display_name())
                            } else if ch.is_irc() {
                                if let Some(t) = ch.irc_target() {
                                    let scheme = if t.tls { "ircs" } else { "irc" };
                                    format!("{scheme}://{}:{}/{}", t.host, t.port, t.channel)
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
                            .button(RichText::new("Remove channel").font(t::small()))
                            .clicked()
                        {
                            result.closed = Some(ch.clone());
                            ui.close_menu();
                        }
                    });

                    let is_dragging_this = drag
                        .as_ref()
                        .map(|ds| ds.dragging_idx == idx)
                        .unwrap_or(false);
                    // Suppress hover style while a drag is in progress.
                    let is_hovered = row_resp.contains_pointer() && drag.is_none();

                    // Ghost the original row when it's being dragged.
                    if is_dragging_this {
                        ui.set_opacity(0.35);
                    }

                    if is_hovered && !is_active {
                        ui.painter()
                            .rect_filled(row_rect, t::RADIUS_SM, t::hover_row_bg());
                    }

                    let frame_bg = if is_dragging_this {
                        t::alpha(t::accent(), 50)
                    } else if is_active {
                        t::active_channel_bg()
                    } else {
                        Color32::TRANSPARENT
                    };

                    egui::Frame::new()
                        .fill(frame_bg)
                        .corner_radius(t::RADIUS_SM)
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;

                                // Live-status indicator dot.
                                if self.show_live_indicator {
                                    if let Some(live_map) = self.live_channels {
                                        if let Some(&is_live) = live_map.get(ch.display_name()) {
                                            let dot_r = 3.5_f32;
                                            let (dot_rect, _) = ui.allocate_exact_size(
                                                egui::vec2(dot_r * 2.0 + 2.0, dot_r * 2.0),
                                                egui::Sense::hover(),
                                            );
                                            let dot_col = if is_live {
                                                t::red()
                                            } else {
                                                t::alpha(t::text_secondary(), 70)
                                            };
                                            ui.painter().circle_filled(
                                                dot_rect.center(),
                                                dot_r,
                                                dot_col,
                                            );
                                        }
                                    }
                                }

                                // Platform badge for non-Twitch channels
                                if ch.is_kick() {
                                    ui.label(
                                        RichText::new("K")
                                            .font(t::small())
                                            .strong()
                                            .color(t::kick_green()),
                                    );
                                } else if ch.is_irc() {
                                    ui.label(
                                        RichText::new("IRC")
                                            .font(t::small())
                                            .strong()
                                            .color(t::text_muted()),
                                    );
                                }

                                // Channel name label
                                let display = ch.display_name();
                                let prefix = if ch.is_kick() || ch.is_irc_server_tab() {
                                    ""
                                } else {
                                    "# "
                                };
                                let name_text = if unread_mentions > 0 {
                                    RichText::new(format!("{prefix}{display}"))
                                        .font(t::body())
                                        .color(t::text_primary())
                                        .strong()
                                } else if unread_count > 0 {
                                    RichText::new(format!("{prefix}{display}"))
                                        .font(t::body())
                                        .color(t::text_primary())
                                        .strong()
                                } else if is_active {
                                    RichText::new(format!("{prefix}{display}"))
                                        .font(t::body())
                                        .color(t::text_primary())
                                        .strong()
                                } else {
                                    RichText::new(format!("{prefix}{display}"))
                                        .font(t::body())
                                        .color(t::text_secondary())
                                };

                                let label_resp = ui.add(
                                    egui::Label::new(name_text)
                                        .sense(egui::Sense::click())
                                        .truncate(),
                                );
                                if label_resp.clicked() {
                                    result.selected = Some(ch.clone());
                                }

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if unread_mentions > 0 {
                                            // Mentions / highlights badge - semantic warning pill.
                                            let label = if unread_mentions > 99 {
                                                "99+".to_owned()
                                            } else {
                                                format!("{unread_mentions}")
                                            };
                                            egui::Frame::new()
                                                .fill(t::mention_pill_bg())
                                                .corner_radius(t::RADIUS_SM)
                                                .inner_margin(egui::Margin::symmetric(5, 0))
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        egui::RichText::new(label)
                                                            .font(t::tiny())
                                                            .strong()
                                                            .color(t::text_primary()),
                                                    );
                                                });
                                        } else if unread_count > 0 {
                                            // Plain unreads badge - muted
                                            let label = if unread_count > 99 {
                                                "99+".to_owned()
                                            } else {
                                                format!("{unread_count}")
                                            };
                                            ui.label(
                                                RichText::new(label)
                                                    .font(t::small())
                                                    .color(t::text_secondary()),
                                            );
                                        } else {
                                            let show_close =
                                                self.show_close_button && (is_hovered || is_active);
                                            let close = ui.add_visible(
                                                show_close,
                                                egui::Label::new(
                                                    RichText::new("✕").font(t::small()).color(
                                                        if is_hovered {
                                                            t::text_secondary()
                                                        } else {
                                                            t::text_muted()
                                                        },
                                                    ),
                                                )
                                                .sense(egui::Sense::click()),
                                            );
                                            if close.clicked() {
                                                result.closed = Some(ch.clone());
                                            }
                                            if close.hovered() {
                                                ui.ctx().set_cursor_icon(
                                                    egui::CursorIcon::PointingHand,
                                                );
                                            }
                                        }
                                    },
                                );
                            });
                        });

                    // Reset opacity after the frame.
                    if is_dragging_this {
                        ui.set_opacity(1.0);
                    }

                    if row_resp.clicked() && result.selected.is_none() && result.closed.is_none() {
                        result.selected = Some(ch.clone());
                    }
                }

                // -- Floating drag ghost --------------------------
                // Rendered on a foreground layer, follows the cursor.
                if let Some(ref ds) = drag {
                    if let Some(pos) = ui.ctx().pointer_latest_pos() {
                        let dragged_ch = &self.channels[ds.dragging_idx];
                        let label_text = format!("# {}", dragged_ch.display_name());
                        let is_outside = ds.outside_sidebar;

                        let layer_id =
                            egui::LayerId::new(egui::Order::Tooltip, Id::new("drag_ghost_layer"));
                        let ghost_rect = egui::Rect::from_min_size(
                            egui::pos2(pos.x + 12.0, pos.y - 14.0),
                            egui::vec2(130.0, row_h),
                        );
                        let painter = ui.ctx().layer_painter(layer_id);

                        // Slight rotation via skewed rounded rect
                        let fill = if is_outside {
                            // Green-ish tint for "will split"
                            t::split_success_bg()
                        } else {
                            t::alpha(t::accent(), 210)
                        };
                        painter.rect_filled(ghost_rect, egui::CornerRadius::same(6), fill);
                        painter.text(
                            ghost_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            &label_text,
                            t::small(),
                            t::text_on_accent(),
                        );

                        // Sub-label when outside sidebar
                        if is_outside {
                            let hint_pos =
                                egui::pos2(ghost_rect.center().x, ghost_rect.bottom() + 3.0);
                            painter.text(
                                hint_pos,
                                egui::Align2::CENTER_TOP,
                                "Split view",
                                t::small(),
                                t::split_success_text(),
                            );
                        }
                    }
                }

                // Trailing insert indicator - drop after the last row.
                if let Some(ref ds) = drag {
                    if ds.insert_before >= n {
                        let y = ui.cursor().min.y - t::CHANNEL_ROW_GAP * 0.5;
                        let x_range = ui.max_rect().x_range();
                        ui.painter()
                            .hline(x_range, y, egui::Stroke::new(2.0, t::accent()));
                    }
                    // Signal to the app that a drop-zone overlay should be shown.
                    result.dragging_outside = ds.outside_sidebar;
                }
            });

        result
    }
}
