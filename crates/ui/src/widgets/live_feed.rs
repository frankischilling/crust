use crate::image_cache::UiImageCache;
use std::collections::HashSet;
use std::time::Instant;

use egui::{CornerRadius, Frame, Layout, Margin, RichText, ScrollArea, Sense, Ui, Vec2};

use crust_core::model::LiveChannelSnapshot;

use crate::theme as t;

pub enum LiveFeedAction {
    /// Left-click on a row: focus or join + open.
    OpenChannel(String),
    /// Refresh button in the header.
    Refresh,
    /// Right-click -> "Open in Streamlink".
    OpenStreamlink(String),
    /// Right-click -> "Open in player".
    OpenInPlayer(String),
}

pub struct LiveFeed<'a> {
    pub snapshots: &'a [LiveChannelSnapshot],
    pub loaded: bool,
    pub error: Option<&'a str>,
    pub last_updated: Option<Instant>,
    pub joined_logins: &'a HashSet<String>,
    /// URL -> (width, height, bytes) cache for thumbnails. Entries populated
    /// by the app after `AppCommand::FetchImage` -> `AppEvent::EmoteImageReady`.
    pub thumbnail_bytes: &'a UiImageCache,
}

const ROW_CORNER: u8 = 6;

struct FeedFonts {
    body: egui::FontId,
    small: egui::FontId,
    heading: egui::FontId,
    tiny: egui::FontId,
}

impl FeedFonts {
    fn current() -> Self {
        Self {
            body: t::body(),
            small: t::small(),
            heading: t::heading(),
            tiny: t::tiny(),
        }
    }

    fn scale(&self) -> f32 {
        (self.body.size / 14.0).clamp(0.75, 2.5)
    }
}

/// Row dimensions scale with the chat font so Ctrl-+/- and Appearance >
/// Font settings resize the Live-feed rows consistently with the rest of
/// the UI.
struct RowMetrics {
    compact: bool,
    row_height: f32,
    pad_x: f32,
    pad_y: f32,
    body_height: f32,
    small_height: f32,
    line_gap: f32,
    thumb_w: f32,
    thumb_h: f32,
    thumb_gap: f32,
    right_col_w: f32,
    live_pill_font: f32,
    live_pill_w: f32,
    live_pill_h: f32,
}

impl RowMetrics {
    fn compute(ui: &Ui, width: f32, fonts: &FeedFonts) -> Self {
        let s = fonts.scale();
        let compact = width < 440.0 * s;
        let thumb_w = if !compact {
            96.0 * s
        } else if width >= 320.0 * s {
            64.0 * s
        } else {
            0.0
        };
        let thumb_h = thumb_w * 9.0 / 16.0;
        let (body_height, small_height) =
            ui.fonts(|f| (f.row_height(&fonts.body), f.row_height(&fonts.small)));
        let line_gap = 3.0 * s;
        let pad_y = 8.0 * s;
        let text_height = body_height
            + small_height
            + line_gap
            + if compact {
                small_height + line_gap
            } else {
                0.0
            };
        Self {
            compact,
            row_height: text_height.max(thumb_h) + pad_y * 2.0,
            pad_x: (12.0 * s).min(16.0),
            pad_y,
            body_height,
            small_height,
            line_gap,
            thumb_w,
            thumb_h,
            thumb_gap: if thumb_w > 0.0 { 12.0 * s } else { 0.0 },
            right_col_w: 140.0 * s,
            live_pill_font: 9.0 * s,
            live_pill_w: 34.0 * s,
            live_pill_h: 14.0 * s,
        }
    }
}

impl<'a> LiveFeed<'a> {
    pub fn show(&self, ui: &mut Ui) -> Option<LiveFeedAction> {
        self.show_with_fonts(ui, &FeedFonts::current())
    }

    fn show_with_fonts(&self, ui: &mut Ui, fonts: &FeedFonts) -> Option<LiveFeedAction> {
        let mut action: Option<LiveFeedAction> = None;
        let s = fonts.scale();
        let margin_x = (16.0 * s).clamp(8.0, 20.0) as i8;
        let margin_y = (14.0 * s) as i8;

        Frame::new()
            .inner_margin(Margin::symmetric(margin_x, margin_y))
            .show(ui, |ui| {
                // Allocate Refresh first so a long title cannot push it out of a split.
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        let refresh = if ui.available_width() < 220.0 * s {
                            "\u{27F3}"
                        } else {
                            "\u{27F3}  Refresh"
                        };
                        if ui
                            .button(RichText::new(refresh).font(fonts.body.clone()))
                            .on_hover_text("Refresh live channels")
                            .clicked()
                        {
                            action = Some(LiveFeedAction::Refresh);
                        }
                        let title = if ui.available_width() < 200.0 * s {
                            "Live"
                        } else {
                            "Live followed channels"
                        };
                        let title_height = ui.fonts(|f| f.row_height(&fonts.heading));
                        let (title_rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), title_height),
                            Sense::hover(),
                        );
                        row_label(
                            ui,
                            title_rect,
                            RichText::new(format!("{title} ({})", self.snapshots.len()))
                                .font(fonts.heading.clone())
                                .strong(),
                            egui::Align::LEFT,
                        );
                    });
                });

                // Stale / error line
                if let Some(msg) = self.error {
                    ui.add_space(4.0 * s);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("! {msg}"))
                                .font(fonts.body.clone())
                                .color(t::red()),
                        )
                        .wrap(),
                    );
                    if let Some(ts) = self.last_updated {
                        ui.label(
                            RichText::new(format!("updated {}s ago", ts.elapsed().as_secs()))
                                .font(fonts.small.clone())
                                .color(t::text_muted()),
                        );
                    }
                }

                ui.add_space(8.0 * s);
                ui.separator();
                ui.add_space(6.0 * s);

                if !self.loaded {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0 * s);
                        ui.label(
                            RichText::new("Loading live channels...")
                                .font(fonts.body.clone())
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
                                .font(fonts.body.clone())
                                .color(t::text_muted()),
                        );
                    });
                    return;
                }

                ui.spacing_mut().item_spacing.y = 4.0 * s;
                // Use one layout for every row so show_rows keeps an exact scroll extent.
                let row_width = ui.available_width() - ui.spacing().scroll.allocated_width();
                let metrics = RowMetrics::compute(ui, row_width, fonts);
                ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
                    ui,
                    metrics.row_height,
                    self.snapshots.len(),
                    |ui, rows| {
                        for row in rows {
                            let snap = &self.snapshots[row];
                            if let Some(act) = ui
                                .push_id(&snap.user_login, |ui| self.row(ui, snap, &metrics, fonts))
                                .inner
                            {
                                action = Some(act);
                            }
                        }
                    },
                );
            });

        action
    }

    fn row(
        &self,
        ui: &mut Ui,
        snap: &LiveChannelSnapshot,
        m: &RowMetrics,
        fonts: &FeedFonts,
    ) -> Option<LiveFeedAction> {
        let s = fonts.scale();
        let mut action: Option<LiveFeedAction> = None;
        let row_size = Vec2::new(ui.available_width(), m.row_height);
        let (rect, resp) = ui.allocate_exact_size(row_size, Sense::click());
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                ui.is_enabled(),
                format!("Open #{}", snap.user_login),
            )
        });
        if !ui.is_rect_visible(rect) {
            return None;
        }

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
            egui::Stroke::new(1.0_f32, t::border_subtle()),
        );

        // Thumbnail rect, vertically centered
        let thumb_y = rect.center().y - m.thumb_h * 0.5;
        let thumb_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + m.pad_x, thumb_y),
            Vec2::new(m.thumb_w, m.thumb_h),
        );
        if m.thumb_w > 0.0 {
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
                        super::bytes_uri(&snap.thumbnail_url, raw),
                        egui::load::Bytes::Shared(raw.clone()),
                    )
                    .fit_to_exact_size(Vec2::new(m.thumb_w, m.thumb_h))
                    .corner_radius(CornerRadius::same(4)),
                );
            } else {
                ui.painter()
                    .rect_filled(thumb_rect, CornerRadius::same(4), t::bg_raised());
            }

            // LIVE pill over the top-left of the thumbnail.
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
        }

        let text_left = thumb_rect.right() + m.thumb_gap;
        let text_right = rect.right()
            - m.pad_x
            - if m.compact {
                0.0
            } else {
                m.right_col_w + 8.0 * s
            };
        let text_width = (text_right - text_left).max(0.0);
        let text_top = rect.top() + m.pad_y;
        let name_rect = egui::Rect::from_min_size(
            egui::pos2(text_left, text_top),
            egui::vec2(text_width, m.body_height),
        );
        let mut name_text_rect = name_rect;
        if self.joined_logins.contains(&snap.user_login) {
            let joined = ui.painter().layout_no_wrap(
                "joined".to_owned(),
                fonts.tiny.clone(),
                t::text_on_accent(),
            );
            let pill_width = (joined.size().x + 10.0 * s).min(text_width * 0.45);
            let pill_rect = egui::Rect::from_min_size(
                egui::pos2(name_rect.right() - pill_width, name_rect.top()),
                egui::vec2(pill_width, m.body_height),
            );
            ui.painter()
                .rect_filled(pill_rect, CornerRadius::same(3), t::accent_dim());
            row_label(
                ui,
                pill_rect,
                RichText::new("joined")
                    .font(fonts.tiny.clone())
                    .color(t::text_on_accent()),
                egui::Align::Center,
            );
            name_text_rect.max.x = (pill_rect.left() - 6.0 * s).max(name_text_rect.left());
        }
        row_label(
            ui,
            name_text_rect,
            RichText::new(&snap.user_name)
                .font(fonts.body.clone())
                .strong()
                .color(t::text_primary()),
            egui::Align::LEFT,
        );
        let login_top = text_top + m.body_height + m.line_gap;
        row_label(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(text_left, login_top),
                egui::vec2(text_width, m.small_height),
            ),
            RichText::new(&snap.user_login)
                .font(fonts.small.clone())
                .color(t::text_muted()),
            egui::Align::LEFT,
        );

        let viewers = format_viewers(snap.viewer_count);
        let uptime = format_uptime(&snap.started_at);
        if m.compact {
            let metadata = if let Some(uptime) = uptime {
                format!("{viewers} · {uptime}")
            } else {
                viewers
            };
            row_label(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(text_left, login_top + m.small_height + m.line_gap),
                    egui::vec2(text_width, m.small_height),
                ),
                RichText::new(metadata)
                    .font(fonts.small.clone())
                    .color(t::text_secondary()),
                egui::Align::LEFT,
            );
        } else {
            let meta_left = rect.right() - m.pad_x - m.right_col_w;
            row_label(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(meta_left, text_top),
                    egui::vec2(m.right_col_w, m.body_height),
                ),
                RichText::new(viewers)
                    .font(fonts.body.clone())
                    .strong()
                    .color(t::text_primary()),
                egui::Align::RIGHT,
            );
            if let Some(uptime) = uptime {
                row_label(
                    ui,
                    egui::Rect::from_min_size(
                        egui::pos2(meta_left, login_top),
                        egui::vec2(m.right_col_w, m.small_height),
                    ),
                    RichText::new(uptime)
                        .font(fonts.small.clone())
                        .color(t::text_muted()),
                    egui::Align::RIGHT,
                );
            }
        }

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
                .button(RichText::new(open_label).font(fonts.body.clone()))
                .clicked()
            {
                action = Some(LiveFeedAction::OpenChannel(login.clone()));
                ui.close_menu();
            }
            if ui
                .button(RichText::new("Copy login").font(fonts.body.clone()))
                .clicked()
            {
                ui.ctx().copy_text(login.clone());
                ui.close_menu();
            }
            if ui
                .button(RichText::new("Copy channel URL").font(fonts.body.clone()))
                .clicked()
            {
                ui.ctx().copy_text(format!("https://twitch.tv/{login}"));
                ui.close_menu();
            }
            ui.separator();
            if ui
                .button(RichText::new("Open in Streamlink").font(fonts.body.clone()))
                .clicked()
            {
                action = Some(LiveFeedAction::OpenStreamlink(login.clone()));
                ui.close_menu();
            }
            if ui
                .button(RichText::new("Open in player").font(fonts.body.clone()))
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

fn row_label(ui: &mut Ui, rect: egui::Rect, text: RichText, align: egui::Align) {
    let layout = match align {
        egui::Align::Min => Layout::left_to_right(egui::Align::Center),
        egui::Align::Max => Layout::right_to_left(egui::Align::Center),
        egui::Align::Center => Layout::centered_and_justified(egui::Direction::LeftToRight),
    };
    let mut label_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    label_ui.set_clip_rect(rect.intersect(ui.clip_rect()));
    label_ui.add(
        egui::Label::new(text)
            .truncate()
            .selectable(false)
            .halign(align),
    );
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

    struct PaintedText {
        source: String,
        glyphs: String,
        bounds: egui::Rect,
        clip: egui::Rect,
    }

    fn painted_text(output: &egui::FullOutput) -> Vec<PaintedText> {
        fn collect(shape: &egui::Shape, clip: egui::Rect, texts: &mut Vec<PaintedText>) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, clip, texts);
                    }
                }
                egui::Shape::Text(shape) => {
                    let bounds = shape.galley.rect.translate(shape.pos.to_vec2());
                    if bounds.intersects(clip) {
                        texts.push(PaintedText {
                            source: shape.galley.text().to_owned(),
                            glyphs: shape
                                .galley
                                .rows
                                .iter()
                                .flat_map(|row| row.glyphs.iter().map(|glyph| glyph.chr))
                                .collect(),
                            bounds,
                            clip,
                        });
                    }
                }
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for shape in &output.shapes {
            collect(&shape.shape, shape.clip_rect, &mut texts);
        }
        texts
    }

    fn feed_frame(
        ctx: &egui::Context,
        feed: &LiveFeed<'_>,
        width: f32,
        font_size: f32,
        events: Vec<egui::Event>,
    ) -> (Vec<PaintedText>, Option<LiveFeedAction>) {
        let fonts = FeedFonts {
            body: egui::FontId::proportional(font_size),
            small: egui::FontId::proportional(font_size - 1.0),
            heading: egui::FontId::proportional(font_size - 2.0),
            tiny: egui::FontId::proportional(font_size - 3.0),
        };
        let mut action = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 560.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    action = feed.show_with_fonts(ui, &fonts);
                });
            },
        );
        (painted_text(&output), action)
    }

    #[test]
    fn live_rows_expose_named_accessible_button_actions() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let snapshots = [LiveChannelSnapshot {
            user_id: "1".to_owned(),
            user_login: "channel".to_owned(),
            user_name: "Channel".to_owned(),
            viewer_count: 1,
            thumbnail_url: String::new(),
            started_at: String::new(),
        }];
        let joined = HashSet::from(["channel".to_owned()]);
        let images = UiImageCache::default();
        let feed = LiveFeed {
            snapshots: &snapshots,
            loaded: true,
            error: None,
            last_updated: None,
            joined_logins: &joined,
            thumbnail_bytes: &images,
        };
        let fonts = FeedFonts {
            body: egui::FontId::proportional(14.0),
            small: egui::FontId::proportional(13.0),
            heading: egui::FontId::proportional(12.0),
            tiny: egui::FontId::proportional(11.0),
        };
        let frame = |events| {
            let mut action = None;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(640.0, 480.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = feed.show_with_fonts(ui, &fonts);
                    });
                },
            );
            (output, action)
        };
        let (output, _) = frame(Vec::new());
        let (target, node) = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .find(|(_, node)| node.label() == Some("Open #channel"))
            .expect("the entire Live row exposes a named action");
        assert_eq!(node.role(), egui::accesskit::Role::Button);
        assert!(node.supports_action(egui::accesskit::Action::Click));
        let (_, action) = frame(vec![egui::Event::AccessKitActionRequest(
            egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Click,
                target,
                data: None,
            },
        )]);
        assert!(matches!(action, Some(LiveFeedAction::OpenChannel(login)) if login == "channel"));
    }

    #[test]
    fn live_feed_keeps_names_metadata_and_controls_usable_in_narrow_panes() {
        let snapshots: Vec<_> = (0..100)
            .map(|index| LiveChannelSnapshot {
                user_id: index.to_string(),
                user_login: if index == 1 {
                    "ro".into()
                } else {
                    format!("coffee_code_channel_{index}")
                },
                user_name: if index == 1 {
                    "Ro".into()
                } else {
                    format!("CoffeeCode {index} with a long display name")
                },
                viewer_count: if index == 1 { 86 } else { 1_200 },
                thumbnail_url: String::new(),
                started_at: (chrono::Utc::now() - chrono::Duration::minutes(42)).to_rfc3339(),
            })
            .collect();
        let joined = HashSet::from([snapshots[0].user_login.clone()]);
        let images = UiImageCache::default();
        let feed = LiveFeed {
            snapshots: &snapshots,
            loaded: true,
            error: None,
            last_updated: None,
            joined_logins: &joined,
            thumbnail_bytes: &images,
        };
        for (width, font_size) in [
            (240.0, 13.5),
            (320.0, 13.5),
            (240.0, 24.0),
            (320.0, 24.0),
            (640.0, 13.5),
        ] {
            let ctx = egui::Context::default();
            feed_frame(&ctx, &feed, width, font_size, Vec::new());
            let (texts, _) = feed_frame(&ctx, &feed, width, font_size, Vec::new());
            let find = |source: &str| texts.iter().find(|text| text.source == source).unwrap();
            let name = find(&snapshots[0].user_name);
            let login = find(&snapshots[0].user_login);
            let second_name = find(&snapshots[1].user_name);
            let second_login = find(&snapshots[1].user_login);
            let joined = find("joined");
            let viewers = texts
                .iter()
                .find(|text| text.source.starts_with("1.2K viewers"))
                .unwrap();
            let refresh = texts
                .iter()
                .find(|text| text.source.starts_with('\u{27f3}'))
                .unwrap();
            let title = texts
                .iter()
                .find(|text| text.source.starts_with("Live "))
                .unwrap();
            assert!(
                name.glyphs.starts_with("Cof"),
                "name disappeared at {width}/{font_size}: {}",
                name.glyphs
            );
            assert!(login.glyphs.starts_with("coffee"));
            assert!(viewers.glyphs.starts_with("1.2K"));
            assert!(
                (name.bounds.left() - second_name.bounds.left()).abs() <= 0.5
                    && (login.bounds.left() - second_login.bounds.left()).abs() <= 0.5
                    && (name.bounds.left() - login.bounds.left()).abs() <= 0.5,
                "names and logins do not share a left edge at {width}/{font_size}"
            );
            assert!(
                (title.bounds.left() - title.clip.left()).abs() <= 0.5,
                "the header title is not aligned to the left"
            );
            assert!((joined.bounds.center().x - joined.clip.center().x).abs() <= 0.5);
            if width == 640.0 {
                let second_viewers = texts
                    .iter()
                    .find(|text| text.source == "86 viewers")
                    .unwrap();
                assert!(
                    (viewers.bounds.right() - second_viewers.bounds.right()).abs() <= 0.5,
                    "wide viewer counts do not share a right edge"
                );
            }
            let visible = [name, login, joined, viewers, refresh, title];
            for (index, text) in visible.iter().enumerate() {
                assert!(
                    text.bounds.left() >= text.clip.left() - 1.0
                        && text.bounds.right() <= text.clip.right() + 1.0,
                    "{} overflows at {width}/{font_size}: {:?} / {:?}",
                    text.source,
                    text.bounds,
                    text.clip
                );
                for other in &visible[index + 1..] {
                    assert!(
                        !text.bounds.shrink(0.5).intersects(other.bounds.shrink(0.5)),
                        "{} overlaps {} at {width}/{font_size}",
                        text.source,
                        other.source
                    );
                }
            }
            for (pos, expect_refresh) in [
                (name.bounds.center(), false),
                (refresh.bounds.center(), true),
            ] {
                for pressed in [true, false] {
                    let (_, action) = feed_frame(
                        &ctx,
                        &feed,
                        width,
                        font_size,
                        vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                    if !pressed {
                        if expect_refresh {
                            assert!(
                                matches!(action, Some(LiveFeedAction::Refresh)),
                                "Refresh is not clickable at {width}/{font_size}"
                            );
                        } else {
                            assert!(
                                matches!(action, Some(LiveFeedAction::OpenChannel(ref login)) if login == &snapshots[0].user_login),
                                "row is not clickable at {width}/{font_size}"
                            );
                        }
                    }
                }
            }
            // Context actions use the same row target after compact layout and truncation.
            let pos = name.bounds.center();
            for pressed in [true, false] {
                feed_frame(
                    &ctx,
                    &feed,
                    width,
                    font_size,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Secondary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            let (menu, _) = feed_frame(&ctx, &feed, width, font_size, Vec::new());
            let player = menu
                .iter()
                .find(|text| text.source == "Open in player")
                .expect("row context menu opens")
                .bounds
                .center();
            for pressed in [true, false] {
                let (_, action) = feed_frame(
                    &ctx,
                    &feed,
                    width,
                    font_size,
                    vec![
                        egui::Event::PointerMoved(player),
                        egui::Event::PointerButton {
                            pos: player,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
                if !pressed {
                    assert!(
                        matches!(action, Some(LiveFeedAction::OpenInPlayer(ref login)) if login == &snapshots[0].user_login)
                    );
                }
            }
        }
    }

    #[test]
    fn only_visible_live_rows_request_thumbnail_images() {
        let ctx = egui::Context::default();
        let mut images = UiImageCache::default();
        let snapshots: Vec<_> = (0..100)
            .map(|index| LiveChannelSnapshot {
                user_id: index.to_string(),
                user_login: format!("channel{index}"),
                user_name: format!("Channel {index}"),
                viewer_count: 1,
                thumbnail_url: format!("https://example.test/{index}.png"),
                started_at: String::new(),
            })
            .collect();
        images.begin_frame(&ctx);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 280.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    LiveFeed {
                        snapshots: &snapshots,
                        loaded: true,
                        error: None,
                        last_updated: None,
                        joined_logins: &HashSet::new(),
                        thumbnail_bytes: &images,
                    }
                    .show(ui);
                });
            },
        );
        let mut requested = Vec::new();
        images.end_frame(&ctx, |command| {
            let crust_core::events::AppCommand::FetchImage { url } = command else {
                panic!("expected an image request");
            };
            requested.push(url);
            true
        });
        assert!(!requested.is_empty());
        assert!(requested.len() < 10, "offscreen thumbnails were requested");
        assert!(!requested.contains(&snapshots.last().unwrap().thumbnail_url));
    }

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
