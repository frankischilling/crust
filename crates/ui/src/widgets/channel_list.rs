use std::collections::HashMap;

use egui::{Color32, Id, RichText, ScrollArea, Ui};

use crust_core::model::{ChannelId, ChannelState};
use crate::theme as t;

/// Left-sidebar channel list.
pub struct ChannelList<'a> {
    pub channels: &'a [ChannelId],
    pub active: Option<&'a ChannelId>,
    pub channel_states: &'a HashMap<ChannelId, ChannelState>,
}

pub struct ChannelListResult {
    pub selected: Option<ChannelId>,
    pub closed: Option<ChannelId>,
    /// Set when the user dragged a tab to a new position; contains the full
    /// new ordered channel list.
    pub reordered: Option<Vec<ChannelId>>,
}

/// Persistent-per-frame drag tracking stored in egui temp storage.
#[derive(Clone)]
struct DragState {
    /// Index of the channel being dragged.
    dragging_idx: usize,
    /// Index *before* which the dragged item will be inserted (0 = top).
    insert_before: usize,
}

impl<'a> ChannelList<'a> {
    pub fn show(&mut self, ui: &mut Ui) -> ChannelListResult {
        let mut result = ChannelListResult { selected: None, closed: None, reordered: None };

        let drag_id = Id::new("channel_list_drag");
        const ROW_H: f32 = 28.0;
        const STRIDE: f32 = ROW_H + t::CHANNEL_ROW_GAP;
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
                            ui.painter().hline(x_range, y, egui::Stroke::new(2.0, t::ACCENT));
                        }
                    }

                    let interact_id = egui::Id::new("ch_row").with(ch);

                    // Allocate the full-width row rect.
                    let row_rect = {
                        let avail = ui.available_rect_before_wrap();
                        egui::Rect::from_min_size(
                            avail.min,
                            egui::vec2(avail.width(), ROW_H),
                        )
                    };
                    let row_resp = ui.interact(
                        row_rect,
                        interact_id,
                        egui::Sense::click_and_drag(),
                    );

                    // ── Drag start ────────────────────────────────────────
                    if row_resp.drag_started() {
                        ui.data_mut(|d| {
                            d.insert_temp(drag_id, DragState {
                                dragging_idx: idx,
                                insert_before: idx,
                            })
                        });
                    }

                    // ── Drag update: recompute insert position ────────────
                    if row_resp.dragged() {
                        if let Some(pos) = ui.ctx().pointer_latest_pos() {
                            let rel_y = pos.y - list_top;
                            let new_insert = ((rel_y / STRIDE + 0.5) as usize).min(n);
                            ui.data_mut(|d| {
                                let mut ds: DragState = d.get_temp(drag_id).unwrap_or(DragState {
                                    dragging_idx: idx,
                                    insert_before: idx,
                                });
                                ds.insert_before = new_insert;
                                d.insert_temp(drag_id, ds);
                            });
                        }
                        ui.ctx().request_repaint();
                    }

                    // ── Drag release: build new order ─────────────────────
                    if row_resp.drag_stopped() {
                        if let Some(ds) = ui.data(|d| d.get_temp::<DragState>(drag_id)) {
                            let raw = ds.insert_before;
                            let insert = if raw > ds.dragging_idx { raw - 1 } else { raw };
                            if insert != ds.dragging_idx {
                                let mut new_order: Vec<ChannelId> = self.channels.to_vec();
                                let moved = new_order.remove(ds.dragging_idx);
                                new_order.insert(insert, moved);
                                result.reordered = Some(new_order);
                            }
                        }
                        ui.data_mut(|d| d.remove::<DragState>(drag_id));
                    }

                    let is_dragging_this =
                        drag.as_ref().map(|ds| ds.dragging_idx == idx).unwrap_or(false);
                    // Suppress hover style while a drag is in progress.
                    let is_hovered = row_resp.contains_pointer() && drag.is_none();

                    if is_hovered && !is_active {
                        ui.painter().rect_filled(row_rect, t::RADIUS_SM, t::HOVER_ROW_BG);
                    }

                    let frame_bg = if is_dragging_this {
                        Color32::from_rgba_unmultiplied(100, 70, 180, 50)
                    } else if is_active {
                        t::ACTIVE_CHANNEL_BG
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

                                // Channel name label
                                let name_text = if unread_mentions > 0 {
                                    RichText::new(format!("# {}", ch.as_str()))
                                        .font(t::body())
                                        .color(t::YELLOW)
                                        .strong()
                                } else if unread_count > 0 {
                                    RichText::new(format!("# {}", ch.as_str()))
                                        .font(t::body())
                                        .color(t::TEXT_PRIMARY)
                                        .strong()
                                } else if is_active {
                                    RichText::new(format!("# {}", ch.as_str()))
                                        .font(t::body())
                                        .color(t::TEXT_PRIMARY)
                                        .strong()
                                } else {
                                    RichText::new(format!("# {}", ch.as_str()))
                                        .font(t::body())
                                        .color(t::TEXT_SECONDARY)
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
                                            // Mentions / highlights badge — amber
                                            let label = if unread_count > 99 {
                                                "99+".to_owned()
                                            } else {
                                                format!("{unread_count}")
                                            };
                                            ui.label(
                                                RichText::new(label)
                                                    .font(t::small())
                                                    .strong()
                                                    .color(t::YELLOW),
                                            );
                                        } else if unread_count > 0 {
                                            // Plain unreads badge — muted
                                            let label = if unread_count > 99 {
                                                "99+".to_owned()
                                            } else {
                                                format!("{unread_count}")
                                            };
                                            ui.label(
                                                RichText::new(label)
                                                    .font(t::small())
                                                    .color(t::TEXT_SECONDARY),
                                            );
                                        } else {
                                            let show_close = is_hovered || is_active;
                                            let close = ui.add_visible(
                                                show_close,
                                                egui::Label::new(
                                                    RichText::new("x")
                                                        .font(t::small())
                                                        .color(t::TEXT_MUTED),
                                                )
                                                .sense(egui::Sense::click()),
                                            );
                                            if close.clicked() {
                                                result.closed = Some(ch.clone());
                                            }
                                        }
                                    },
                                );
                            });
                        });

                    if row_resp.clicked() && result.selected.is_none() && result.closed.is_none() {
                        result.selected = Some(ch.clone());
                    }
                }

                // Trailing insert indicator — drop after the last row.
                if let Some(ref ds) = drag {
                    if ds.insert_before >= n {
                        let y = ui.cursor().min.y - t::CHANNEL_ROW_GAP * 0.5;
                        let x_range = ui.max_rect().x_range();
                        ui.painter().hline(
                            x_range, y,
                            egui::Stroke::new(2.0, t::ACCENT),
                        );
                    }
                }
            });

        result
    }
}
