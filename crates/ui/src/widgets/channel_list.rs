use std::collections::HashMap;

use egui::{Color32, RichText, ScrollArea, Ui};

use crust_core::model::{ChannelId, ChannelState};

/// Left-sidebar channel tab list.
pub struct ChannelList<'a> {
    pub channels: &'a [ChannelId],
    pub active: Option<&'a ChannelId>,
    pub channel_states: &'a HashMap<ChannelId, ChannelState>,
}

pub struct ChannelListResult {
    pub selected: Option<ChannelId>,
    pub closed: Option<ChannelId>,
}

impl<'a> ChannelList<'a> {
    pub fn show(&mut self, ui: &mut Ui) -> ChannelListResult {
        let mut result = ChannelListResult {
            selected: None,
            closed: None,
        };

        ScrollArea::vertical()
            .id_salt("channel_list")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.set_min_width(100.0);
                ui.spacing_mut().item_spacing.y = 2.0;

                for ch in self.channels {
                    let is_active = self.active == Some(ch);
                    let highlights = self
                        .channel_states
                        .get(ch)
                        .map(|s| s.unread_highlights)
                        .unwrap_or(0);

                    // Full-width row: active channel gets a filled background.
                    let row_color = if is_active {
                        Color32::from_rgba_unmultiplied(100, 70, 200, 60)
                    } else {
                        Color32::TRANSPARENT
                    };

                    egui::Frame::new()
                        .fill(row_color)
                        .corner_radius(egui::CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(6, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // Channel name — live highlight dot prefix
                                let name_text = if highlights > 0 {
                                    RichText::new(format!("● #{}", ch.as_str()))
                                        .color(Color32::from_rgb(255, 200, 60))
                                        .strong()
                                } else if is_active {
                                    RichText::new(format!("# {}", ch.as_str()))
                                        .color(Color32::WHITE)
                                        .strong()
                                } else {
                                    RichText::new(format!("# {}", ch.as_str()))
                                        .color(Color32::from_rgb(180, 180, 180))
                                };

                                let resp = ui.add(
                                    egui::Label::new(name_text)
                                        .sense(egui::Sense::click())
                                        .truncate(),
                                );
                                if resp.clicked() {
                                    result.selected = Some(ch.clone());
                                }

                                // Highlight badge
                                if highlights > 0 {
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(
                                            RichText::new(format!("{highlights}"))
                                                .small()
                                                .strong()
                                                .color(Color32::from_rgb(255, 200, 60)),
                                        );
                                    });
                                } else {
                                    // Close button appears on hover
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        let close = ui.add_visible(
                                            resp.hovered() || is_active,
                                            egui::Label::new(
                                                RichText::new("✕")
                                                    .small()
                                                    .color(Color32::from_rgb(140, 140, 140)),
                                            )
                                            .sense(egui::Sense::click()),
                                        );
                                        if close.clicked() {
                                            result.closed = Some(ch.clone());
                                        }
                                    });
                                }
                            });
                        });
                }
            });

        result
    }
}
