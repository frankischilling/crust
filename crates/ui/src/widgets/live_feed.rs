use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use egui::{CornerRadius, Frame, Layout, Margin, RichText, ScrollArea, Sense, Ui, Vec2};

use crust_core::model::LiveChannelSnapshot;

use crate::theme as t;

pub enum LiveFeedAction {
    /// Left-click on a row: focus or join + open.
    OpenChannel(String),
    /// Refresh button in the header.
    Refresh,
    /// Right-click → "Open in Streamlink".
    OpenStreamlink(String),
    /// Right-click → "Open in player".
    OpenInPlayer(String),
}

pub struct LiveFeed<'a> {
    pub snapshots: &'a [LiveChannelSnapshot],
    pub loaded: bool,
    pub error: Option<&'a str>,
    pub last_updated: Option<Instant>,
    pub joined_logins: &'a HashSet<String>,
    /// URL → (width, height, bytes) cache for thumbnails. Entries populated
    /// by the app after `AppCommand::FetchImage` → `AppEvent::EmoteImageReady`.
    pub thumbnail_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
}

const ROW_CORNER: u8 = 6;

/// Current chat-font scale (1.0 = default 14 pt). All live-feed dimensions
/// and secondary font sizes are derived from this so the widget resizes
/// consistently with the rest of the UI when the user changes font size.
fn scale() -> f32 {
    (t::chat_font_size() / 14.0).clamp(0.75, 2.5)
}

/// Row dimensions scale with the chat font so Ctrl-+/- and Appearance >
/// Font settings resize the Live-feed rows consistently with the rest of
/// the UI.
struct RowMetrics {
    row_height: f32,
    pad_x: f32,
    pad_y: f32,
    thumb_w: f32,
    thumb_h: f32,
    thumb_gap: f32,
    right_col_w: f32,
    live_pill_font: f32,
    live_pill_w: f32,
    live_pill_h: f32,
}

impl RowMetrics {
    fn compute() -> Self {
        let s = scale();
        Self {
            row_height: 70.0 * s,
            pad_x: 12.0 * s,
            pad_y: 8.0 * s,
            thumb_w: 96.0 * s,
            thumb_h: 54.0 * s,
            thumb_gap: 12.0 * s,
            right_col_w: 140.0 * s,
            live_pill_font: 9.0 * s,
            live_pill_w: 34.0 * s,
            live_pill_h: 14.0 * s,
        }
    }
}

impl<'a> LiveFeed<'a> {
    pub fn show(&self, ui: &mut Ui) -> Option<LiveFeedAction> {
        let mut action: Option<LiveFeedAction> = None;
        let s = scale();
        let margin_x = (16.0 * s) as i8;
        let margin_y = (14.0 * s) as i8;

        Frame::new()
            .inner_margin(Margin::symmetric(margin_x, margin_y))
            .show(ui, |ui| {
                // Header
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Live followed channels")
                            .font(t::heading())
                            .strong(),
                    );
                    ui.add_space(8.0 * s);
                    ui.label(
                        RichText::new(format!("({})", self.snapshots.len()))
                            .font(t::body())
                            .color(t::text_muted()),
                    );
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(RichText::new("\u{27F3}  Refresh").font(t::body()))
                            .clicked()
                        {
                            action = Some(LiveFeedAction::Refresh);
                        }
                    });
                });

                // Stale / error line
                if let Some(msg) = self.error {
                    ui.add_space(4.0 * s);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("!").font(t::body()).color(t::red()).strong());
                        ui.add_space(4.0 * s);
                        ui.label(RichText::new(msg).font(t::body()).color(t::red()));
                        if let Some(ts) = self.last_updated {
                            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "updated {}s ago",
                                        ts.elapsed().as_secs()
                                    ))
                                    .font(t::small())
                                    .color(t::text_muted()),
                                );
                            });
                        }
                    });
                }

                ui.add_space(8.0 * s);
                ui.separator();
                ui.add_space(6.0 * s);

                if !self.loaded {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0 * s);
                        ui.label(
                            RichText::new("Loading live channels...")
                                .font(t::body())
                                .color(t::text_muted()),
                        );
                    });
                    return;
                }
                if self.snapshots.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0 * s);
                        ui.label(
                            RichText::new("No followed channels live.")
                                .font(t::body())
                                .color(t::text_muted()),
                        );
                    });
                    return;
                }

                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0 * s;
                        for snap in self.snapshots {
                            if let Some(act) = self.row(ui, snap) {
                                action = Some(act);
                            }
                        }
                    });
            });

        action
    }

    fn row(&self, ui: &mut Ui, snap: &LiveChannelSnapshot) -> Option<LiveFeedAction> {
        let m = RowMetrics::compute();
        let s = scale();
        let mut action: Option<LiveFeedAction> = None;
        let row_size = Vec2::new(ui.available_width(), m.row_height);
        let (rect, resp) = ui.allocate_exact_size(row_size, Sense::click());

        // Background: hover / default
        let bg = if resp.hovered() {
            t::hover_row_bg()
        } else {
            t::bg_surface()
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(ROW_CORNER), bg);
        // Subtle bottom border for visual separation
        ui.painter().hline(
            rect.left() + m.pad_x..=rect.right() - m.pad_x,
            rect.bottom(),
            egui::Stroke::new(1.0, t::border_subtle()),
        );

        // Thumbnail rect, vertically centered
        let thumb_y = rect.center().y - m.thumb_h * 0.5;
        let thumb_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + m.pad_x, thumb_y),
            Vec2::new(m.thumb_w, m.thumb_h),
        );
        let cached = if snap.thumbnail_url.is_empty() {
            None
        } else {
            self.thumbnail_bytes.get(&snap.thumbnail_url)
        };
        if let Some(&(_, _, ref raw)) = cached {
            let mut thumb_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(thumb_rect)
                    .layout(Layout::left_to_right(egui::Align::Center)),
            );
            thumb_ui.put(
                thumb_rect,
                egui::Image::from_bytes(
                    snap.thumbnail_url.clone(),
                    egui::load::Bytes::Shared(raw.clone()),
                )
                .fit_to_exact_size(Vec2::new(m.thumb_w, m.thumb_h))
                .corner_radius(CornerRadius::same(4)),
            );
        } else {
            ui.painter()
                .rect_filled(thumb_rect, CornerRadius::same(4), t::bg_raised());
        }

        // LIVE pill over the top-left of the thumbnail
        let pill_pos = egui::pos2(thumb_rect.left() + 4.0 * s, thumb_rect.top() + 4.0 * s);
        let pill_size = Vec2::new(m.live_pill_w, m.live_pill_h);
        let pill_rect = egui::Rect::from_min_size(pill_pos, pill_size);
        ui.painter()
            .rect_filled(pill_rect, CornerRadius::same(3), t::red());
        ui.painter().text(
            pill_rect.center(),
            egui::Align2::CENTER_CENTER,
            "LIVE",
            egui::FontId::proportional(m.live_pill_font),
            egui::Color32::WHITE,
        );

        // Right-aligned viewer count area
        let meta_right = egui::Rect::from_min_max(
            egui::pos2(rect.right() - m.pad_x - m.right_col_w, rect.top() + m.pad_y),
            egui::pos2(rect.right() - m.pad_x, rect.bottom() - m.pad_y),
        );
        let mut right_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(meta_right)
                .layout(Layout::top_down(egui::Align::RIGHT)),
        );
        right_ui.label(
            RichText::new(format_viewers(snap.viewer_count))
                .font(t::body())
                .strong()
                .color(t::text_primary()),
        );
        if let Some(uptime) = format_uptime(&snap.started_at) {
            right_ui.label(
                RichText::new(uptime)
                    .font(t::small())
                    .color(t::text_muted()),
            );
        }

        // Text column: name + "(joined)" pill on line 1, user_login on line 2.
        let text_left = thumb_rect.right() + m.thumb_gap;
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, rect.top() + m.pad_y),
            egui::pos2(meta_right.left() - 8.0 * s, rect.bottom() - m.pad_y),
        );
        let mut text_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(Layout::top_down(egui::Align::LEFT)),
        );
        text_ui.horizontal(|ui| {
            ui.label(
                RichText::new(&snap.user_name)
                    .font(t::body())
                    .strong()
                    .color(t::text_primary()),
            );
            if self.joined_logins.contains(&snap.user_login) {
                ui.add_space(6.0 * s);
                let pill_pad_x = (5.0 * s) as i8;
                let pill_pad_y = (1.0 * s) as i8;
                Frame::new()
                    .fill(t::accent_dim())
                    .corner_radius(CornerRadius::same(3))
                    .inner_margin(Margin::symmetric(pill_pad_x, pill_pad_y))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("joined")
                                .font(t::tiny())
                                .color(t::text_on_accent()),
                        );
                    });
            }
        });
        text_ui.label(
            RichText::new(&snap.user_login)
                .font(t::small())
                .color(t::text_muted()),
        );

        // Context menu
        resp.context_menu(|ui| {
            let login = snap.user_login.clone();
            let joined = self.joined_logins.contains(&login);
            let open_label = if joined {
                "Focus channel"
            } else {
                "Open channel"
            };
            if ui
                .button(RichText::new(open_label).font(t::body()))
                .clicked()
            {
                action = Some(LiveFeedAction::OpenChannel(login.clone()));
                ui.close_menu();
            }
            if ui
                .button(RichText::new("Copy login").font(t::body()))
                .clicked()
            {
                ui.ctx().copy_text(login.clone());
                ui.close_menu();
            }
            if ui
                .button(RichText::new("Copy channel URL").font(t::body()))
                .clicked()
            {
                ui.ctx().copy_text(format!("https://twitch.tv/{login}"));
                ui.close_menu();
            }
            ui.separator();
            if ui
                .button(RichText::new("Open in Streamlink").font(t::body()))
                .clicked()
            {
                action = Some(LiveFeedAction::OpenStreamlink(login.clone()));
                ui.close_menu();
            }
            if ui
                .button(RichText::new("Open in player").font(t::body()))
                .clicked()
            {
                action = Some(LiveFeedAction::OpenInPlayer(login));
                ui.close_menu();
            }
        });

        if resp.clicked() {
            action = Some(LiveFeedAction::OpenChannel(snap.user_login.clone()));
        }

        action
    }
}

fn format_viewers(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M viewers", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K viewers", n as f64 / 1_000.0)
    } else {
        format!("{n} viewers")
    }
}

/// Parse an RFC 3339 started_at timestamp into an uptime string like
/// `"2h 14m"` or `"47m"`. Returns `None` when the timestamp is empty or
/// unparseable.
fn format_uptime(started_at: &str) -> Option<String> {
    if started_at.is_empty() {
        return None;
    }
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let now = chrono::Utc::now();
    let delta = now.signed_duration_since(started.with_timezone(&chrono::Utc));
    if delta.num_seconds() < 0 {
        return None;
    }
    let hours = delta.num_hours();
    let minutes = delta.num_minutes() % 60;
    if hours > 0 {
        Some(format!("{hours}h {minutes}m"))
    } else {
        Some(format!("{minutes}m"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_viewers_under_1000_shows_full_number() {
        assert_eq!(format_viewers(0), "0 viewers");
        assert_eq!(format_viewers(999), "999 viewers");
    }

    #[test]
    fn format_viewers_thousands_uses_one_decimal_k() {
        assert_eq!(format_viewers(1_000), "1.0K viewers");
        assert_eq!(format_viewers(12_345), "12.3K viewers");
    }

    #[test]
    fn format_viewers_millions_uses_one_decimal_m() {
        assert_eq!(format_viewers(1_000_000), "1.0M viewers");
        assert_eq!(format_viewers(2_500_000), "2.5M viewers");
    }

    #[test]
    fn format_uptime_empty_returns_none() {
        assert!(format_uptime("").is_none());
    }

    #[test]
    fn format_uptime_garbage_returns_none() {
        assert!(format_uptime("not-a-date").is_none());
    }
}
