use crate::image_cache::UiImageCache;

use egui::{Color32, CornerRadius, Label, Margin, RichText, Stroke, Ui};

use crust_core::state::SharedChatSessionState;

use crate::theme as t;

// Left padding matches `MessageList`'s ROW_PAD_X (6px) so the banner's
// leading text visually aligns with the chat rows beneath it instead of
// sitting a few pixels further right.
const BANNER_INNER_PAD: Margin = Margin {
    left: 6,
    right: 6,
    top: 8,
    bottom: 8,
};

// The hosting `msg_ui` in single-pane view is inset 6px on the left but
// flush to the right, so without an outer right margin the banner's border
// would kiss the right edge of the chat column. Mirror that 6px on the
// right to keep the banner visually centred.
const BANNER_OUTER_PAD: Margin = Margin {
    left: 0,
    right: 6,
    top: 0,
    bottom: 0,
};

const PARTICIPANT_AVATAR_PX: f32 = 20.0;

/// Render the Shared Chat viewer-total banner above a channel's message list.
/// Shows the combined viewer count across every participating broadcaster
/// plus a per-channel breakdown with avatar, display name, and viewer count.
pub fn show_shared_chat_banner(
    ui: &mut Ui,
    session: &SharedChatSessionState,
    emote_bytes: &UiImageCache,
) {
    let accent = Color32::from_rgb(155, 102, 214);
    let fill = t::alpha(accent, 24);
    let border = t::alpha(accent, 90);
    let avail_w = ui.available_width();
    let total = session.total_viewers();

    let frame = egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(BANNER_INNER_PAD)
        .outer_margin(BANNER_OUTER_PAD);
    let inner_w = avail_w - frame.total_margin().sum().x;
    frame.show(ui, |ui| {
        ui.set_width(inner_w.max(0.0));
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    Label::new(
                        RichText::new("🔗 Shared Chat")
                            .font(t::small())
                            .color(accent)
                            .strong(),
                    )
                    .wrap(),
                );
                ui.add(
                    Label::new(
                        RichText::new(format!("{} total viewers", fmt_u64(total)))
                            .font(t::small())
                            .color(t::text_primary())
                            .strong(),
                    )
                    .wrap(),
                );
                let live = session.participants.iter().filter(|p| p.live).count();
                let n = session.participants.len();
                ui.add(
                    Label::new(
                        RichText::new(format!("{live}/{n} live"))
                            .font(t::tiny())
                            .color(t::text_secondary()),
                    )
                    .wrap(),
                );
            });

            ui.add_space(4.0);

            // Per-participant breakdown: single horizontal strip that
            // wraps onto new rows when there are too many broadcasters
            // to fit.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                for p in &session.participants {
                    render_participant_chip(ui, p, emote_bytes);
                }
            });
        });
    });
}

fn render_participant_chip(
    ui: &mut Ui,
    p: &crust_core::state::SharedChatParticipant,
    emote_bytes: &UiImageCache,
) {
    let text_color = if p.live {
        t::text_primary()
    } else {
        t::text_muted()
    };

    let label = if p.display_name.is_empty() {
        p.login.clone()
    } else {
        p.display_name.clone()
    };

    let tooltip = if p.live {
        format!("{label} is live with {} viewers", fmt_u64(p.viewer_count))
    } else {
        format!("{label} is offline")
    };

    let count_color = if p.live { t::accent() } else { t::text_muted() };
    let count_text = if p.live {
        fmt_u64(p.viewer_count)
    } else {
        "-".to_owned()
    };
    let name_width = ui
        .painter()
        .layout_no_wrap(label.clone(), t::tiny(), text_color)
        .size()
        .x;
    let count_width = ui
        .painter()
        .layout_no_wrap(count_text.clone(), t::tiny(), count_color)
        .size()
        .x;
    let chip_width =
        (PARTICIPANT_AVATAR_PX + name_width + count_width + 10.0).min(ui.max_rect().width());
    let count_width = count_width.min((chip_width - PARTICIPANT_AVATAR_PX - 10.0).max(0.0));
    let name_width = (chip_width - PARTICIPANT_AVATAR_PX - count_width - 10.0).max(0.0);
    let resp = ui
        .allocate_ui_with_layout(
            egui::vec2(chip_width, PARTICIPANT_AVATAR_PX),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                let (avatar_rect, _) = ui.allocate_exact_size(
                    egui::vec2(PARTICIPANT_AVATAR_PX, PARTICIPANT_AVATAR_PX),
                    egui::Sense::hover(),
                );

                if let Some(url) = p.profile_url.as_deref().filter(|s| !s.is_empty()) {
                    if let Some(&(w, h, ref raw)) = emote_bytes.get(url) {
                        let max = PARTICIPANT_AVATAR_PX;
                        let aspect = w as f32 / h.max(1) as f32;
                        let size = if aspect >= 1.0 {
                            egui::vec2(max, max / aspect)
                        } else {
                            egui::vec2(max * aspect, max)
                        };
                        let url_key = super::bytes_uri(url, raw);
                        ui.put(
                            avatar_rect,
                            egui::Image::from_bytes(
                                url_key,
                                egui::load::Bytes::Shared(raw.clone()),
                            )
                            .fit_to_exact_size(size)
                            .corner_radius(CornerRadius::same(3)),
                        );
                    } else {
                        draw_avatar_fallback(ui, avatar_rect, &label, text_color);
                    }
                } else {
                    draw_avatar_fallback(ui, avatar_rect, &label, text_color);
                }

                ui.add_sized(
                    [name_width, PARTICIPANT_AVATAR_PX],
                    Label::new(
                        RichText::new(&label)
                            .font(t::tiny())
                            .color(text_color)
                            .strong(),
                    )
                    .truncate(),
                );
                ui.add_sized(
                    [count_width, PARTICIPANT_AVATAR_PX],
                    Label::new(RichText::new(count_text).font(t::tiny()).color(count_color))
                        .truncate(),
                );
            },
        )
        .response;

    resp.on_hover_ui_at_pointer(|ui| {
        ui.label(RichText::new(&tooltip).strong());
    });
}

fn draw_avatar_fallback(ui: &mut Ui, rect: egui::Rect, label: &str, color: Color32) {
    let initial = label.chars().next().unwrap_or('|').to_ascii_uppercase();
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), t::alpha(color, 40));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        t::tiny(),
        color,
    );
}

/// Format an unsigned integer as a human-readable viewer count: `251`,
/// `4,787`, `28.4K`, `1.2M`. Counts use K/M above 10,000 and 1,000,000.
pub(crate) fn fmt_u64(n: u64) -> String {
    if n >= 1_000_000 {
        let v = (n as f64) / 1_000_000.0;
        if v >= 10.0 {
            format!("{:.0}M", v)
        } else {
            format!("{:.1}M", v)
        }
    } else if n >= 10_000 {
        let v = (n as f64) / 1_000.0;
        if v >= 100.0 {
            format!("{:.0}K", v)
        } else {
            format!("{:.1}K", v)
        }
    } else if n >= 1_000 {
        // 1,234
        let s = n.to_string();
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len() + 1);
        let first_group = bytes.len() % 3;
        if first_group != 0 {
            out.push_str(&s[..first_group]);
            if bytes.len() > first_group {
                out.push(',');
            }
        }
        for (i, chunk) in bytes[first_group..].chunks(3).enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        }
        out
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_chat_banner_fits_narrow_panes() {
        let session = SharedChatSessionState {
            session_id: "session".to_owned(),
            host_broadcaster_id: "host".to_owned(),
            participants: (0..4)
                .map(|index| crust_core::state::SharedChatParticipant {
                    broadcaster_id: index.to_string(),
                    login: format!("long_participant_login_{index}"),
                    display_name: format!("LongParticipantDisplayName{index}"),
                    profile_url: None,
                    viewer_count: 123_456,
                    live: index != 3,
                })
                .collect(),
            updated_at: std::time::Instant::now(),
        };
        for width in [150.0, 240.0, 480.0] {
            let ctx = egui::Context::default();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 600.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let available = ui.max_rect();
                        show_shared_chat_banner(ui, &session, &UiImageCache::default());
                        assert!(ui.min_rect().right() <= available.right() + 1.0);
                    });
                },
            );
            for shape in output.shapes {
                if let egui::Shape::Text(text) = shape.shape {
                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                    assert!(
                        rect.left() >= -1.0 && rect.right() <= width + 1.0,
                        "text {:?} escapes a {width}-point pane: {rect:?}",
                        text.galley.text()
                    );
                }
            }
        }
    }

    #[test]
    fn fmt_u64_compacts_large_counts() {
        assert_eq!(fmt_u64(0), "0");
        assert_eq!(fmt_u64(251), "251");
        assert_eq!(fmt_u64(4_787), "4,787");
        assert_eq!(fmt_u64(28_400), "28.4K");
        assert_eq!(fmt_u64(123_400), "123K");
        assert_eq!(fmt_u64(1_200_000), "1.2M");
        // `{:.0}` truncates toward zero rather than rounding half-up, so
        // 12.5M renders as 12M; that's acceptable for the banner.
        assert_eq!(fmt_u64(12_500_000), "12M");
    }
}
