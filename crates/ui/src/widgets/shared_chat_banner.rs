use std::collections::HashMap;
use std::sync::Arc;

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
    emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
) {
    let accent = Color32::from_rgb(155, 102, 214);
    let fill = t::alpha(accent, 24);
    let border = t::alpha(accent, 90);
    let avail_w = ui.available_width();
    let total = session.total_viewers();

    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(BANNER_INNER_PAD)
        .outer_margin(BANNER_OUTER_PAD)
        .show(ui, |ui| {
            let inner_w = avail_w
                - (BANNER_INNER_PAD.left + BANNER_INNER_PAD.right) as f32
                - (BANNER_OUTER_PAD.left + BANNER_OUTER_PAD.right) as f32;
            ui.set_width(inner_w.max(0.0));
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.add(Label::new(
                        RichText::new("🔗 Shared Chat")
                            .font(t::small())
                            .color(accent)
                            .strong(),
                    ));
                    ui.add_space(8.0);
                    ui.add(Label::new(
                        RichText::new(format!("{} total viewers", fmt_u64(total)))
                            .font(t::small())
                            .color(t::text_primary())
                            .strong(),
                    ));
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let live = session.participants.iter().filter(|p| p.live).count();
                            let n = session.participants.len();
                            ui.add(Label::new(
                                RichText::new(format!("{live}/{n} live"))
                                    .font(t::tiny())
                                    .color(t::text_secondary()),
                            ));
                        },
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
    emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
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

    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;

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
                    ui.add(
                        egui::Image::from_bytes(
                            url_key,
                            egui::load::Bytes::Shared(raw.clone()),
                        )
                        .fit_to_exact_size(size)
                        .corner_radius(CornerRadius::same(3)),
                    );
                } else {
                    draw_avatar_fallback(ui, &label, text_color);
                }
            } else {
                draw_avatar_fallback(ui, &label, text_color);
            }

            ui.add(Label::new(
                RichText::new(&label).font(t::tiny()).color(text_color).strong(),
            ));
            let count_color = if p.live {
                t::accent()
            } else {
                t::text_muted()
            };
            let count_text = if p.live {
                fmt_u64(p.viewer_count)
            } else {
                "-".to_owned()
            };
            ui.add(Label::new(
                RichText::new(count_text)
                    .font(t::tiny())
                    .color(count_color),
            ));
        })
        .response;

    resp.on_hover_ui_at_pointer(|ui| {
        ui.label(RichText::new(&tooltip).strong());
    });
}

fn draw_avatar_fallback(ui: &mut Ui, label: &str, color: Color32) {
    let initial = label.chars().next().unwrap_or('|').to_ascii_uppercase();
    egui::Frame::new()
        .fill(t::alpha(color, 40))
        .corner_radius(CornerRadius::same(3))
        .inner_margin(Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.add(Label::new(
                RichText::new(initial.to_string())
                    .font(t::tiny())
                    .color(color)
                    .strong(),
            ));
        });
}

/// Format an unsigned integer as a human-readable viewer count: `251`,
/// `4,787`, `28.4K`, `1.2M`. Chatterino doesn't have an equivalent, but
/// Twitch's web UI collapses to K/M once counts cross 10k / 1M.
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
    use super::fmt_u64;

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
