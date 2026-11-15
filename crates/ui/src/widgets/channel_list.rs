use std::collections::HashMap;

use egui::{Color32, Id, RichText, ScrollArea, Ui};

use crate::theme as t;
use crate::widgets::chrome::{self, ChromeIcon, IconButtonState};
use crust_core::model::{ChannelId, ChannelState};
use crust_core::state::TabVisibilityRule;

/// Left-sidebar channel list.
pub struct ChannelList<'a> {
    pub channels: &'a [ChannelId],
    pub active: Option<&'a ChannelId>,
    pub channel_states: &'a HashMap<ChannelId, ChannelState>,
    /// Optional map of channel-login -> is_live for drawing live status dots.
    pub live_channels: Option<&'a HashMap<String, bool>>,
    pub show_live_indicator: bool,
    pub show_close_button: bool,
    /// When true, render the pinned "Live" sentinel row at index 0.
    pub show_live_feed_pin: bool,
    /// Number of currently-live followed channels (for the count badge).
    pub live_feed_count: usize,
    /// When true, render the pinned "Mentions" sentinel row at index 1
    /// (directly under the Live pin when both are present).
    pub show_mentions_pin: bool,
    /// Total mentions in the cross-channel buffer (for the count badge).
    pub mentions_total: usize,
    /// Unread mentions since the Mentions tab was last focused (bold badge).
    pub mentions_unread: u32,
    /// Per-channel tab visibility rules (only non-default entries). Used
    /// to render the "Hide when offline" right-click toggle as a checked
    /// menu entry when a rule is active.
    pub tab_visibility_rules: &'a HashMap<ChannelId, TabVisibilityRule>,
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
    /// Right-click -> "Open in Streamlink" (Twitch channels only).
    pub open_streamlink: Option<ChannelId>,
    /// Right-click -> "Open in player" (Twitch channels only).
    pub open_player: Option<ChannelId>,
    /// Right-click -> "Hide when offline" toggled. Holds the new rule
    /// the caller should persist + mirror into app state.
    pub visibility_change: Option<(ChannelId, TabVisibilityRule)>,
}

/// Persistent-per-frame drag tracking stored in egui temp storage.
#[derive(Clone)]
struct DragState {
    /// Channel identity survives live filtering and tab-order changes.
    channel: ChannelId,
    /// Index *before* which the dragged item will be inserted (0 = top).
    insert_before: usize,
    /// True when the pointer has left the sidebar (split-drop mode).
    outside_sidebar: bool,
}

const ROW_PAD_X: f32 = 8.0;
const ROW_PAD_Y: f32 = 5.0;
const ROW_CONTENT_GAP: f32 = 4.0;

struct SidebarRowStyle {
    name: egui::FontId,
    secondary: egui::FontId,
    count: egui::FontId,
    pin: egui::FontId,
    close_size: f32,
}

impl SidebarRowStyle {
    fn current() -> Self {
        Self {
            name: t::body(),
            secondary: t::small(),
            count: t::tiny(),
            pin: t::tabs_font(),
            close_size: t::icon_btn_sm(),
        }
    }

    fn row_height(&self, ui: &Ui, show_close: bool) -> f32 {
        let text_height = ui.fonts(|fonts| {
            [&self.name, &self.secondary, &self.pin]
                .into_iter()
                .map(|font| fonts.row_height(font))
                .fold(0.0_f32, f32::max)
                .max(fonts.row_height(&self.count) + 4.0)
        });
        let content_height = text_height.max(if show_close { self.close_size } else { 0.0 });
        (content_height + ROW_PAD_Y * 2.0).max(28.0)
    }
}

fn paint_row_text(ui: &Ui, rect: egui::Rect, text: &str, font: egui::FontId, color: Color32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    let galley = ui.fonts(|fonts| fonts.layout_job(job));
    let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5);
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(pos, galley, color);
}

/// Draw the count at the right edge and return the remaining text boundary.
fn paint_row_count(
    ui: &Ui,
    rect: egui::Rect,
    text: &str,
    font: egui::FontId,
    mention: bool,
    color: Color32,
) -> f32 {
    let text_size = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), color)
        .size();
    let padding = if mention {
        egui::vec2(5.0, 2.0)
    } else {
        egui::Vec2::ZERO
    };
    let size = (text_size + padding * 2.0).min(rect.size().max(egui::Vec2::ZERO));
    let badge = egui::Rect::from_center_size(
        egui::pos2(rect.right() - size.x * 0.5, rect.center().y),
        size,
    );
    if mention {
        ui.painter()
            .with_clip_rect(rect.intersect(ui.clip_rect()))
            .rect_filled(badge, t::RADIUS_SM, t::mention_pill_bg());
    }
    paint_row_text(ui, badge.shrink2(padding), text, font, color);
    (badge.left() - ROW_CONTENT_GAP).max(rect.left())
}

#[derive(Clone, Copy)]
enum PinKind {
    Live,
    Mentions,
}

impl PinKind {
    fn label(self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::Mentions => "Mentions",
        }
    }

    fn id(self) -> Id {
        Id::new(match self {
            Self::Live => "live_feed_pin_row",
            Self::Mentions => "mentions_pin_row",
        })
    }
}

fn pinned_row(
    ui: &mut Ui,
    kind: PinKind,
    active: bool,
    count: Option<(String, bool)>,
    row_h: f32,
    style: &SidebarRowStyle,
) -> egui::Response {
    let row_rect = egui::Rect::from_min_size(
        ui.available_rect_before_wrap().min,
        egui::vec2(ui.available_width(), row_h),
    );
    let response = ui.interact(row_rect, kind.id(), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            active,
            kind.label(),
        )
    });
    if active || response.hovered() || response.has_focus() {
        ui.painter().rect_filled(
            row_rect,
            t::RADIUS_SM,
            if active {
                t::active_channel_bg()
            } else {
                t::hover_row_bg()
            },
        );
    }
    let icon_width = ui
        .painter()
        .layout_no_wrap("@".to_owned(), style.pin.clone(), t::accent())
        .size()
        .x
        .max(12.0);
    let icon_rect = egui::Rect::from_min_max(
        egui::pos2(row_rect.left() + ROW_PAD_X, row_rect.top() + ROW_PAD_Y),
        egui::pos2(
            row_rect.left() + ROW_PAD_X + icon_width,
            row_rect.bottom() - ROW_PAD_Y,
        ),
    );
    let painter = ui
        .painter()
        .with_clip_rect(row_rect.intersect(ui.clip_rect()));
    match kind {
        PinKind::Live => {
            painter.circle_filled(icon_rect.center(), 4.0, t::red());
        }
        PinKind::Mentions => {
            painter.text(
                icon_rect.center(),
                egui::Align2::CENTER_CENTER,
                "@",
                style.pin.clone(),
                t::accent(),
            );
        }
    }
    let mut label_rect = row_rect.shrink2(egui::vec2(ROW_PAD_X, ROW_PAD_Y));
    label_rect.min.x = (icon_rect.right() + ROW_CONTENT_GAP).min(label_rect.right());
    if let Some((text, mention)) = &count {
        label_rect.max.x = paint_row_count(
            ui,
            label_rect,
            text,
            if *mention {
                style.count.clone()
            } else {
                style.pin.clone()
            },
            *mention,
            if active || response.hovered() || *mention {
                t::text_primary()
            } else {
                t::text_secondary()
            },
        );
    }
    paint_row_text(
        ui,
        label_rect,
        kind.label(),
        style.pin.clone(),
        t::text_primary(),
    );
    ui.allocate_space(egui::vec2(row_rect.width(), row_h));
    response.on_hover_text(count.map_or_else(
        || kind.label().to_owned(),
        |(count, unread)| {
            format!(
                "{}: {count}{}",
                kind.label(),
                if unread { " unread" } else { "" }
            )
        },
    ))
}

impl DragState {
    fn reordered(&self, channels: &[ChannelId]) -> Option<Vec<ChannelId>> {
        let from = channels
            .iter()
            .position(|channel| channel == &self.channel)?;
        let before = self.insert_before.min(channels.len());
        let insert = if before > from { before - 1 } else { before };
        if insert == from {
            return None;
        }
        let mut reordered = channels.to_vec();
        let channel = reordered.remove(from);
        reordered.insert(insert, channel);
        Some(reordered)
    }
}

impl<'a> ChannelList<'a> {
    pub fn show(&mut self, ui: &mut Ui) -> ChannelListResult {
        self.show_with_style(ui, &SidebarRowStyle::current())
    }

    fn show_with_style(&mut self, ui: &mut Ui, style: &SidebarRowStyle) -> ChannelListResult {
        let mut result = ChannelListResult {
            selected: None,
            closed: None,
            reordered: None,
            drag_split: None,
            dragging_outside: false,
            open_streamlink: None,
            open_player: None,
            visibility_change: None,
        };

        let drag_id = Id::new("channel_list_drag");
        // Drawing, pointer targets, and drag insertion share the same geometry.
        let row_h = style.row_height(ui, self.show_close_button);
        let stride: f32 = row_h + t::CHANNEL_ROW_GAP;
        let n = self.channels.len();

        ScrollArea::vertical()
            .id_salt("channel_list")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.set_min_width(t::SIDEBAR_MIN_W - 16.0);
                ui.spacing_mut().item_spacing.y = t::CHANNEL_ROW_GAP;

                // Snapshot drag state at the start of the frame.
                let drag: Option<DragState> = ui.data_mut(|data| {
                    let drag = data.get_temp::<DragState>(drag_id);
                    if drag
                        .as_ref()
                        .is_some_and(|drag| !self.channels.contains(&drag.channel))
                    {
                        data.remove::<DragState>(drag_id);
                        None
                    } else {
                        drag
                    }
                });

                if self.show_live_feed_pin {
                    let live_id = crust_core::ChannelId::live_feed();
                    if pinned_row(
                        ui,
                        PinKind::Live,
                        self.active == Some(&live_id),
                        Some((self.live_feed_count.to_string(), false)),
                        row_h,
                        style,
                    )
                    .clicked()
                    {
                        result.selected = Some(live_id);
                    }
                }

                if self.show_mentions_pin {
                    let mentions_id = crust_core::ChannelId::mentions();
                    let count = if self.mentions_unread > 0 {
                        Some((
                            if self.mentions_unread > 99 {
                                "99+".to_owned()
                            } else {
                                self.mentions_unread.to_string()
                            },
                            true,
                        ))
                    } else if self.mentions_total > 0 {
                        Some((self.mentions_total.to_string(), false))
                    } else {
                        None
                    };
                    if pinned_row(
                        ui,
                        PinKind::Mentions,
                        self.active == Some(&mentions_id),
                        count,
                        row_h,
                        style,
                    )
                    .clicked()
                    {
                        result.selected = Some(mentions_id);
                    }
                }

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
                        if &ds.channel != ch && ds.insert_before == idx {
                            let y = ui.cursor().min.y - t::CHANNEL_ROW_GAP * 0.5;
                            let x_range = ui.max_rect().x_range();
                            ui.painter()
                                .hline(x_range, y, egui::Stroke::new(2.0_f32, t::accent()));
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
                    row_resp.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            is_active,
                            ch.display_name(),
                        )
                    });

                    // Drag start
                    if row_resp.drag_started() {
                        ui.data_mut(|d| {
                            d.insert_temp(
                                drag_id,
                                DragState {
                                    channel: ch.clone(),
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
                                    channel: ch.clone(),
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
                        if let Some(mut ds) = ui.data(|d| d.get_temp::<DragState>(drag_id)) {
                            let sidebar_rect = ui.max_rect();
                            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                                ds.insert_before =
                                    (((pos.y - list_top) / stride + 0.5) as usize).min(n);
                            }
                            let outside = ui
                                .ctx()
                                .pointer_latest_pos()
                                .map(|p| p.x > sidebar_rect.right() + 30.0)
                                .unwrap_or(false);
                            if outside && self.channels.contains(&ds.channel) {
                                // Pointer released outside sidebar -> split pane
                                result.drag_split = Some(ds.channel.clone());
                            } else {
                                result.reordered = ds.reordered(self.channels);
                            }
                        }
                        ui.data_mut(|d| d.remove::<DragState>(drag_id));
                    }

                    // Right-click menu for quick channel actions.
                    row_resp.context_menu(|ui| {
                        if ui
                            .button(
                                RichText::new(if ch.is_irc_query() {
                                    "Switch to private chat"
                                } else {
                                    "Switch to channel"
                                })
                                .font(t::small()),
                            )
                            .clicked()
                        {
                            result.selected = Some(ch.clone());
                            ui.close_menu();
                        }

                        if ui
                            .button(
                                RichText::new(if ch.is_irc_query() {
                                    "Copy nickname"
                                } else {
                                    "Copy channel"
                                })
                                .font(t::small()),
                            )
                            .clicked()
                        {
                            let copy = if ch.is_irc_query() {
                                ch.display_name().to_owned()
                            } else if ch.is_kick() {
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

                        // Streamlink / player integrationTwitch only.
                        if ch.is_twitch() {
                            ui.separator();
                            if ui
                                .button(RichText::new("Open in Streamlink").font(t::small()))
                                .clicked()
                            {
                                result.open_streamlink = Some(ch.clone());
                                ui.close_menu();
                            }
                            if ui
                                .button(RichText::new("Open in player").font(t::small()))
                                .clicked()
                            {
                                result.open_player = Some(ch.clone());
                                ui.close_menu();
                            }
                            ui.separator();
                            let current_rule = self
                                .tab_visibility_rules
                                .get(ch)
                                .copied()
                                .unwrap_or(TabVisibilityRule::Always);
                            let mut hide_offline =
                                current_rule == TabVisibilityRule::HideWhenOffline;
                            if ui
                                .checkbox(
                                    &mut hide_offline,
                                    RichText::new("Hide when offline").font(t::small()),
                                )
                                .changed()
                            {
                                let new_rule = if hide_offline {
                                    TabVisibilityRule::HideWhenOffline
                                } else {
                                    TabVisibilityRule::Always
                                };
                                result.visibility_change = Some((ch.clone(), new_rule));
                                ui.close_menu();
                            }
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

                    let is_dragging_this =
                        drag.as_ref().map(|ds| &ds.channel == ch).unwrap_or(false);
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

                    ui.painter().rect_filled(row_rect, t::RADIUS_SM, frame_bg);
                    let mut row_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt(("channel_row_content", ch))
                            .max_rect(row_rect.shrink2(egui::vec2(ROW_PAD_X, ROW_PAD_Y)))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    row_ui.set_clip_rect(row_rect.intersect(ui.clip_rect()));
                    // The row owns label and badge clicks; only its close button
                    // has a separate action.
                    row_ui.style_mut().interaction.selectable_labels = false;
                    {
                        let ui = &mut row_ui;
                        ui.spacing_mut().item_spacing.x = 4.0;

                        // Live-status indicator dot.
                        if self.show_live_indicator && ch.is_twitch() {
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
                                    ui.painter()
                                        .circle_filled(dot_rect.center(), dot_r, dot_col);
                                }
                            }
                        }

                        // Platform badge for non-Twitch channels
                        if ch.is_kick() {
                            ui.label(
                                RichText::new("K")
                                    .font(style.secondary.clone())
                                    .strong()
                                    .color(t::kick_green()),
                            );
                        } else if ch.is_irc() {
                            ui.label(
                                RichText::new("IRC")
                                    .font(style.secondary.clone())
                                    .strong()
                                    .color(t::text_muted()),
                            );
                        }

                        // Channel name label
                        let display = ch.display_name();
                        let prefix = if ch.is_irc_query() {
                            "@ "
                        } else if ch.is_kick() || ch.is_irc_server_tab() {
                            ""
                        } else {
                            "# "
                        };
                        let name_text = if unread_mentions > 0 {
                            RichText::new(format!("{prefix}{display}"))
                                .font(style.name.clone())
                                .color(t::text_primary())
                                .strong()
                        } else if unread_count > 0 {
                            RichText::new(format!("{prefix}{display}"))
                                .font(style.name.clone())
                                .color(t::text_primary())
                                .strong()
                        } else if is_active {
                            RichText::new(format!("{prefix}{display}"))
                                .font(style.name.clone())
                                .color(t::text_primary())
                                .strong()
                        } else {
                            RichText::new(format!("{prefix}{display}"))
                                .font(style.name.clone())
                                .color(t::text_secondary())
                        };

                        let badge = if unread_mentions > 0 {
                            Some((unread_mentions, true))
                        } else if unread_count > 0 {
                            Some((unread_count, false))
                        } else {
                            None
                        };
                        let badge_label = badge.map(|(count, _)| {
                            if count > 99 {
                                "99+".to_owned()
                            } else {
                                count.to_string()
                            }
                        });
                        let close_size = style.close_size;
                        let mut trailing_width = if self.show_close_button {
                            close_size + ui.spacing().item_spacing.x
                        } else {
                            0.0
                        };
                        if let Some(label) = &badge_label {
                            let mention = badge.is_some_and(|(_, mention)| mention);
                            trailing_width += ui
                                .painter()
                                .layout_no_wrap(
                                    label.clone(),
                                    if mention {
                                        style.count.clone()
                                    } else {
                                        style.secondary.clone()
                                    },
                                    t::text_secondary(),
                                )
                                .size()
                                .x
                                + if mention { 10.0 } else { 0.0 }
                                + ui.spacing().item_spacing.x;
                        }
                        ui.add_sized(
                            [
                                (ui.available_width()
                                    - trailing_width.max(ui.spacing().item_spacing.x))
                                .max(0.0),
                                style.name.size,
                            ],
                            egui::Label::new(name_text).truncate(),
                        );

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.show_close_button {
                                let mut builder = egui::UiBuilder::new();
                                if !(is_hovered || is_active) {
                                    builder = builder.invisible();
                                }
                                let close = ui
                                    .scope_builder(builder, |ui| {
                                        chrome::icon_button(
                                            ui,
                                            ChromeIcon::Close,
                                            "Close channel",
                                            IconButtonState {
                                                compact: true,
                                                ..Default::default()
                                            },
                                        )
                                    })
                                    .inner;
                                if close.clicked() {
                                    result.closed = Some(ch.clone());
                                }
                            }
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
                                                .font(style.count.clone())
                                                .strong()
                                                .color(t::text_primary()),
                                        );
                                    });
                            } else if unread_count > 0 {
                                // Plain unread count.
                                let label = if unread_count > 99 {
                                    "99+".to_owned()
                                } else {
                                    format!("{unread_count}")
                                };
                                ui.label(RichText::new(label).font(style.secondary.clone()).color(
                                    if is_active || is_hovered {
                                        t::text_primary()
                                    } else {
                                        t::text_secondary()
                                    },
                                ));
                            }
                        });
                    }
                    ui.allocate_space(row_rect.size());

                    // Reset opacity after the frame.
                    if is_dragging_this {
                        ui.set_opacity(1.0);
                    }

                    if row_resp.clicked() && result.selected.is_none() && result.closed.is_none() {
                        result.selected = Some(ch.clone());
                    }
                    row_resp.on_hover_text(ch.display_name());
                }

                // Floating drag ghost
                // Rendered on a foreground layer, follows the cursor.
                if let Some(ref ds) = drag {
                    if let Some(pos) = ui.ctx().pointer_latest_pos() {
                        let dragged_ch = &ds.channel;
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
                            .hline(x_range, y, egui::Stroke::new(2.0_f32, t::accent()));
                    }
                    // Signal to the app that a drop-zone overlay should be shown.
                    result.dragging_outside = ds.outside_sidebar;
                }
            });

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_style(size: f32) -> SidebarRowStyle {
        SidebarRowStyle {
            name: egui::FontId::proportional(size),
            secondary: egui::FontId::proportional((size - 1.0).max(8.0)),
            count: egui::FontId::proportional((size - 3.0).max(8.0)),
            pin: egui::FontId::proportional(size),
            close_size: 18.0 * size / t::DEFAULT_CHAT_FONT_SIZE,
        }
    }

    fn geometry_frame(
        ctx: &egui::Context,
        style: &SidebarRowStyle,
        show_close: bool,
        events: Vec<egui::Event>,
    ) -> (ChannelListResult, Vec<egui::Rect>, egui::FullOutput) {
        let channels: Vec<_> = (0..8)
            .map(|index| ChannelId::new(&format!("room{index}")))
            .collect();
        let states = channels
            .iter()
            .map(|channel| {
                let mut state = ChannelState::new(channel.clone());
                state.unread_mentions = 2;
                (channel.clone(), state)
            })
            .collect();
        let rules = HashMap::new();
        let mut list = ChannelList {
            channels: &channels,
            active: channels.first(),
            channel_states: &states,
            live_channels: None,
            show_live_indicator: false,
            show_close_button: show_close,
            show_live_feed_pin: false,
            live_feed_count: 0,
            show_mentions_pin: false,
            mentions_total: 0,
            mentions_unread: 0,
            tab_visibility_rules: &rules,
        };
        let mut result = None;
        let mut rects = Vec::new();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(240.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    result = Some(list.show_with_style(ui, style));
                    rects = channels
                        .iter()
                        .map(|channel| {
                            ctx.read_response(Id::new("ch_row").with(channel))
                                .unwrap()
                                .rect
                        })
                        .collect();
                });
            },
        );
        (result.unwrap(), rects, output)
    }

    fn visible_text_rects(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        fn collect(
            shape: &egui::epaint::Shape,
            clip: egui::Rect,
            text: &mut Vec<(String, egui::Rect)>,
        ) {
            match shape {
                egui::epaint::Shape::Text(shape) => {
                    let rect = shape
                        .galley
                        .rect
                        .translate(shape.pos.to_vec2())
                        .intersect(clip);
                    if rect.is_positive() {
                        text.push((shape.galley.text().to_owned(), rect));
                    }
                }
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, clip, text);
                    }
                }
                _ => {}
            }
        }
        let mut text = Vec::new();
        for shape in &output.shapes {
            collect(&shape.shape, shape.clip_rect, &mut text);
        }
        text
    }

    #[test]
    fn sidebar_row_targets_follow_rendered_rows_at_large_fonts_and_button_padding() {
        for size in [13.5, 24.0, 32.0] {
            for show_close in [false, true] {
                let ctx = egui::Context::default();
                ctx.style_mut(|style| style.spacing.button_padding = egui::vec2(10.0, 5.0));
                let style = row_style(size);
                geometry_frame(&ctx, &style, show_close, Vec::new());
                let (_, rows, output) = geometry_frame(&ctx, &style, show_close, Vec::new());
                for pair in rows.windows(2) {
                    assert!(
                        (pair[1].top() - pair[0].bottom() - t::CHANNEL_ROW_GAP).abs() < 0.1,
                        "row targets diverged from rendered spacing at font size {size}"
                    );
                }
                let texts = visible_text_rects(&output);
                for (index, row) in rows.iter().enumerate() {
                    let (_, text) = texts
                        .iter()
                        .find(|(text, _)| text == &format!("# room{index}"))
                        .unwrap();
                    assert!(
                        row.expand(0.5).contains_rect(*text),
                        "row {index} clips text at font size {size}"
                    );
                }
            }
        }
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn sidebar_drag_uses_visible_row_geometry_and_the_final_release_position() {
        let ctx = egui::Context::default();
        ctx.style_mut(|style| style.spacing.button_padding = egui::vec2(10.0, 5.0));
        let style = row_style(32.0);
        geometry_frame(&ctx, &style, true, Vec::new());
        let (_, rows, _) = geometry_frame(&ctx, &style, true, Vec::new());
        let start = egui::pos2(rows[0].left() + 2.0, rows[0].center().y);
        let intermediate = egui::pos2(start.x, rows[2].center().y);
        let release = egui::pos2(start.x, rows[5].top() + rows[5].height() * 0.25);
        geometry_frame(
            &ctx,
            &style,
            true,
            vec![
                egui::Event::PointerMoved(start),
                pointer_button(start, true),
            ],
        );
        geometry_frame(
            &ctx,
            &style,
            true,
            vec![egui::Event::PointerMoved(intermediate)],
        );
        let (result, _, _) =
            geometry_frame(&ctx, &style, true, vec![pointer_button(release, false)]);
        let reordered = result
            .reordered
            .expect("dragging a row should reorder channels");
        let expected: Vec<_> = [1, 2, 3, 4, 0, 5, 6, 7]
            .into_iter()
            .map(|index| ChannelId::new(&format!("room{index}")))
            .collect();
        assert_eq!(reordered, expected);
        assert!(result.drag_split.is_none());
    }

    #[test]
    fn channel_names_and_unread_badges_select_their_row() {
        for label in ["# room2", "2"] {
            let ctx = egui::Context::default();
            ctx.style_mut(|style| style.interaction.selectable_labels = true);
            let style = row_style(24.0);
            geometry_frame(&ctx, &style, true, Vec::new());
            let (_, rows, output) = geometry_frame(&ctx, &style, true, Vec::new());
            let point = visible_text_rects(&output)
                .into_iter()
                .find(|(text, rect)| text == label && rows[2].contains_rect(*rect))
                .expect("the target row label is visible")
                .1
                .center();
            geometry_frame(
                &ctx,
                &style,
                true,
                vec![
                    egui::Event::PointerMoved(point),
                    pointer_button(point, true),
                ],
            );
            let (result, _, _) =
                geometry_frame(&ctx, &style, true, vec![pointer_button(point, false)]);
            assert_eq!(
                result.selected,
                Some(ChannelId::new("room2")),
                "click on {label}"
            );
            assert!(result.closed.is_none());
        }
    }

    #[test]
    fn pinned_labels_and_counts_do_not_overlap_in_narrow_sidebars() {
        for size in [13.5, 28.0, 32.0] {
            for width in [80.0, 124.0, 210.0] {
                let ctx = egui::Context::default();
                let style = row_style(size);
                let mut row = egui::Rect::NOTHING;
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 160.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let height = style.row_height(ui, false);
                            row = pinned_row(
                                ui,
                                PinKind::Mentions,
                                true,
                                Some(("99+".to_owned(), true)),
                                height,
                                &style,
                            )
                            .rect;
                        });
                    },
                );
                let text = visible_text_rects(&output);
                for (_, rect) in &text {
                    assert!(row.expand(0.5).contains_rect(*rect));
                }
                let label = text
                    .iter()
                    .find(|(text, _)| text == "Mentions")
                    .map(|(_, rect)| rect);
                let count = text
                    .iter()
                    .find(|(text, _)| text == "99+")
                    .map(|(_, rect)| rect);
                if let (Some(label), Some(count)) = (label, count) {
                    assert!(
                        label.right() <= count.left(),
                        "label overlaps count at {width}px, font {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn live_status_does_not_leak_to_other_platforms_with_the_same_channel_name() {
        let channels = [
            ChannelId::new("room"),
            ChannelId::parse_user_input("kick:room").unwrap(),
            ChannelId::parse_user_input("ircs://irc.example/#room").unwrap(),
        ];
        let states = HashMap::new();
        let rules = HashMap::new();
        let live = HashMap::from([("room".to_owned(), true)]);
        let mut list = ChannelList {
            channels: &channels,
            active: None,
            channel_states: &states,
            live_channels: Some(&live),
            show_live_indicator: true,
            show_close_button: false,
            show_live_feed_pin: false,
            live_feed_count: 0,
            show_mentions_pin: false,
            mentions_total: 0,
            mentions_unread: 0,
            tab_visibility_rules: &rules,
        };
        let ctx = egui::Context::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(240.0, 300.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    list.show(ui);
                });
            },
        );
        let indicators = output.shapes.iter().filter(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Circle(circle) if circle.radius == 3.5 && circle.fill == t::red())
        }).count();
        assert_eq!(
            indicators, 1,
            "only the Twitch channel should carry the Twitch live status"
        );
    }

    #[test]
    fn virtual_sidebar_buttons_expose_their_names_selection_and_actions() {
        let pins = [
            (ChannelId::live_feed(), "Live"),
            (ChannelId::mentions(), "Mentions"),
        ];
        for (active, _) in &pins {
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let states = HashMap::new();
            let rules = HashMap::new();
            let mut list = ChannelList {
                channels: &[],
                active: Some(active),
                channel_states: &states,
                live_channels: None,
                show_live_indicator: false,
                show_close_button: false,
                show_live_feed_pin: true,
                live_feed_count: 3,
                show_mentions_pin: true,
                mentions_total: 5,
                mentions_unread: 2,
                tab_visibility_rules: &rules,
            };
            let mut frame = |events| {
                let mut result = None;
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(240.0, 300.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            result = Some(list.show(ui));
                        });
                    },
                );
                (output, result.unwrap())
            };
            let (output, _) = frame(Vec::new());
            let nodes = output.platform_output.accesskit_update.unwrap().nodes;
            for (channel, label) in &pins {
                let (target, node) = nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(*label))
                    .expect("each virtual sidebar row exposes a named button");
                assert_eq!(node.role(), egui::accesskit::Role::Button);
                assert!(node.supports_action(egui::accesskit::Action::Click));
                assert_eq!(
                    node.toggled(),
                    Some(if channel == active {
                        egui::accesskit::Toggled::True
                    } else {
                        egui::accesskit::Toggled::False
                    })
                );
                let (_, result) = frame(vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: *target,
                        data: None,
                    },
                )]);
                assert_eq!(result.selected.as_ref(), Some(channel));
                assert!(result.closed.is_none());
                assert!(result.reordered.is_none());
            }
        }
    }

    #[test]
    fn drag_reorders_the_same_channel_after_visible_tabs_change() {
        let first = ChannelId::new("first");
        let dragged = ChannelId::new("dragged");
        let last = ChannelId::new("last");
        let drag = DragState {
            channel: dragged.clone(),
            insert_before: usize::MAX,
            outside_sidebar: false,
        };
        assert_eq!(
            drag.reordered(&[dragged.clone(), last.clone()]),
            Some(vec![last, dragged.clone()])
        );
        assert!(drag.reordered(&[first]).is_none());
    }

    #[test]
    fn hiding_the_dragged_channel_cancels_the_drag_without_indexing_another_tab() {
        let ctx = egui::Context::default();
        ctx.data_mut(|data| {
            data.insert_temp(
                Id::new("channel_list_drag"),
                DragState {
                    channel: ChannelId::new("removed"),
                    insert_before: 8,
                    outside_sidebar: true,
                },
            )
        });
        let (result, _) = sidebar_frame(
            &ctx,
            0,
            vec![egui::Event::PointerMoved(egui::pos2(40.0, 40.0))],
        );
        assert!(result.reordered.is_none());
        assert!(result.drag_split.is_none());
        assert!(!result.dragging_outside);
        assert!(ctx
            .data(|data| data.get_temp::<DragState>(Id::new("channel_list_drag")))
            .is_none());
    }

    fn sidebar_frame(
        ctx: &egui::Context,
        unread_mentions: u32,
        events: Vec<egui::Event>,
    ) -> (ChannelListResult, egui::Pos2) {
        let channel = ChannelId::new("a_channel_name_far_longer_than_the_sidebar");
        let channels = vec![channel.clone()];
        let mut state = ChannelState::new(channel.clone());
        state.unread_count = 25;
        state.unread_mentions = unread_mentions;
        let states = HashMap::from([(channel.clone(), state)]);
        let rules = HashMap::new();
        let mut list = ChannelList {
            channels: &channels,
            active: Some(&channel),
            channel_states: &states,
            live_channels: None,
            show_live_indicator: false,
            show_close_button: true,
            show_live_feed_pin: false,
            live_feed_count: 0,
            show_mentions_pin: false,
            mentions_total: 0,
            mentions_unread: 0,
            tab_visibility_rules: &rules,
        };
        let mut result = None;
        let mut close_center = egui::Pos2::ZERO;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(210.0, 200.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let area = ui.available_rect_before_wrap();
                    close_center = egui::pos2(
                        area.right() - 8.0 - t::icon_btn_sm() / 2.0,
                        area.top() + 5.0 + t::icon_btn_sm().max(ui.spacing().interact_size.y) / 2.0,
                    );
                    result = Some(list.show(ui));
                });
            },
        );
        (result.unwrap(), close_center)
    }

    #[test]
    fn long_channel_name_and_unread_badge_leave_close_button_reachable() {
        for mentions in [0, 3, 100] {
            let ctx = egui::Context::default();
            let (_, pos) = sidebar_frame(&ctx, mentions, Vec::new());
            sidebar_frame(
                &ctx,
                mentions,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            let (result, _) = sidebar_frame(
                &ctx,
                mentions,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert!(
                result.closed.is_some(),
                "Close was not clickable with {mentions} mentions"
            );
        }
    }
}
