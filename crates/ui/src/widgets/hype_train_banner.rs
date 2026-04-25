use egui::{Color32, CornerRadius, Label, Margin, RichText, Sense, Stroke, Ui, Vec2};

use crust_core::state::{HypeTrainState, RaidBannerState};

use crate::theme as t;

// Left padding matches `MessageList::ROW_PAD_X` so the banner's leading
// text aligns with the chat rows beneath it. The right side also gets 6px
// of *outer* margin (see `BANNER_OUTER_PAD`) so the Frame border doesn't
// kiss the right edge of the chat column, which `msg_ui` leaves flush.
const BANNER_INNER_PAD: Margin = Margin {
    left: 6,
    right: 6,
    top: 8,
    bottom: 8,
};

const BANNER_OUTER_PAD: Margin = Margin {
    left: 0,
    right: 6,
    top: 0,
    bottom: 0,
};

/// Render a live hype-train banner into `ui`.  Pulls its state from
/// [`HypeTrainState`]; rendered above the message list for the active
/// channel.  Cleared by `AppState::expire_stale_hype_trains` once the
/// end-phase cooldown elapses.
pub fn show_hype_train_banner(ui: &mut Ui, state: &HypeTrainState) {
    let accent = hype_accent(state.level);
    let ended = state.phase.eq_ignore_ascii_case("end");
    let fill = t::alpha(accent, if ended { 18 } else { 32 });

    // Reserve the full chat-column width up front so the Frame stretches
    // edge-to-edge instead of shrinking to the widest label.  Without this
    // the Frame was sizing to content and appearing off-center against the
    // message list.
    let avail_w = ui.available_width();

    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, t::alpha(accent, 90)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(BANNER_INNER_PAD)
        .outer_margin(BANNER_OUTER_PAD)
        .show(ui, |ui| {
            let inner_w = avail_w
                - (BANNER_INNER_PAD.left + BANNER_INNER_PAD.right) as f32
                - (BANNER_OUTER_PAD.left + BANNER_OUTER_PAD.right) as f32;
            ui.set_width(inner_w.max(0.0));
            ui.vertical(|ui| {
                let row_width = ui.available_width();
                ui.horizontal(|ui| {
                    ui.set_width(row_width);
                    let title = if ended {
                        format!("🚂 Hype Train ended - Level {}", state.level.max(1))
                    } else {
                        format!("🚂 Hype Train - Level {}", state.level.max(1))
                    };
                    ui.add(Label::new(
                        RichText::new(title).font(t::small()).color(accent).strong(),
                    ));

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let total = state.total;
                            if total > 0 {
                                ui.add(Label::new(
                                    RichText::new(format!("{} total pts", fmt_u64(total)))
                                        .font(t::tiny())
                                        .color(t::text_secondary()),
                                ));
                            }
                        },
                    );
                });

                ui.add_space(4.0);

                let (progress, goal) = (state.progress, state.goal);
                let frac = if ended || goal == 0 {
                    if ended {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    (progress as f32 / goal as f32).clamp(0.0, 1.0)
                };
                draw_progress_bar(ui, frac, accent);

                ui.add_space(3.0);

                ui.horizontal(|ui| {
                    ui.set_width(row_width);
                    let progress_label = if ended {
                        format!("Level {} reached", state.level.max(1))
                    } else if goal == 0 {
                        format!("{} pts", fmt_u64(progress))
                    } else {
                        format!("{} / {} pts", fmt_u64(progress), fmt_u64(goal))
                    };
                    ui.add(Label::new(
                        RichText::new(progress_label)
                            .font(t::tiny())
                            .color(t::text_secondary()),
                    ));

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if let Some(login) = state
                                .top_contributor_login
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                let ty = state
                                    .top_contributor_type
                                    .as_deref()
                                    .map(humanize_contribution)
                                    .unwrap_or_else(|| "contribution".to_owned());
                                let txt = match state.top_contributor_total {
                                    Some(n) if n > 0 => {
                                        format!("Top: {login} ({} {ty})", fmt_u64(n))
                                    }
                                    _ => format!("Top: {login} ({ty})"),
                                };
                                ui.add(Label::new(
                                    RichText::new(txt)
                                        .font(t::tiny())
                                        .color(t::text_secondary()),
                                ));
                            }
                        },
                    );
                });
            });
        });
}

/// Render the raid banner.  Returns `true` when the user clicked the ✕
/// dismiss button so the caller can dispatch [`AppState::dismiss_raid_banner`].
#[must_use]
pub fn show_raid_banner(ui: &mut Ui, state: &RaidBannerState) -> bool {
    let accent = t::raid_cyan();
    let mut dismissed = false;
    let avail_w = ui.available_width();
    egui::Frame::new()
        .fill(t::alpha(accent, 28))
        .stroke(Stroke::new(1.0, t::alpha(accent, 110)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(BANNER_INNER_PAD)
        .outer_margin(BANNER_OUTER_PAD)
        .show(ui, |ui| {
            let inner_w = avail_w
                - (BANNER_INNER_PAD.left + BANNER_INNER_PAD.right) as f32
                - (BANNER_OUTER_PAD.left + BANNER_OUTER_PAD.right) as f32;
            ui.set_width(inner_w.max(0.0));
            ui.horizontal(|ui| {
                let name = state.display_name.trim();
                let display = if name.is_empty() { "A channel" } else { name };
                let mut body = format!(
                    "🚀  {display} is raiding with {} viewer{}",
                    fmt_u32(state.viewer_count),
                    if state.viewer_count == 1 { "" } else { "s" },
                );
                if let Some(login) = state
                    .source_login
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(name))
                {
                    body.push_str(&format!(" (from {login})"));
                }
                ui.add(Label::new(
                    RichText::new(body).font(t::small()).color(accent).strong(),
                ));

                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(18.0, 18.0),
                            Sense::click(),
                        );
                        let color = if resp.hovered() {
                            t::text_primary()
                        } else {
                            t::text_secondary()
                        };
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "✕",
                            t::small(),
                            color,
                        );
                        if resp.clicked() {
                            dismissed = true;
                        }
                    },
                );
            });
        });
    dismissed
}

fn draw_progress_bar(ui: &mut Ui, frac: f32, accent: Color32) {
    let avail = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(Vec2::new(avail, 8.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let track = t::alpha(accent, 40);
    painter.rect_filled(rect, CornerRadius::same(3), track);

    if frac > 0.0 {
        let mut fill_rect = rect;
        fill_rect.max.x = rect.min.x + rect.width() * frac.clamp(0.0, 1.0);
        painter.rect_filled(fill_rect, CornerRadius::same(3), accent);
    }
}

fn hype_accent(level: u32) -> Color32 {
    match level {
        0 | 1 => Color32::from_rgb(168, 85, 247),
        2 => Color32::from_rgb(139, 92, 246),
        3 => Color32::from_rgb(99, 102, 241),
        4 => Color32::from_rgb(236, 72, 153),
        _ => Color32::from_rgb(244, 114, 182),
    }
}

fn humanize_contribution(ty: &str) -> String {
    match ty.trim().to_ascii_lowercase().as_str() {
        "bits" => "bits".to_owned(),
        "subscription" | "subs" => "sub pts".to_owned(),
        "other" => "pts".to_owned(),
        other if !other.is_empty() => other.replace('_', " "),
        _ => "pts".to_owned(),
    }
}

fn fmt_u32(n: u32) -> String {
    fmt_u64(n as u64)
}

fn fmt_u64(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_u64_inserts_thousands_separators() {
        assert_eq!(fmt_u64(0), "0");
        assert_eq!(fmt_u64(999), "999");
        assert_eq!(fmt_u64(1_000), "1,000");
        assert_eq!(fmt_u64(12_345), "12,345");
        assert_eq!(fmt_u64(1_234_567), "1,234,567");
    }

    #[test]
    fn humanize_contribution_maps_known_types() {
        assert_eq!(humanize_contribution("bits"), "bits");
        assert_eq!(humanize_contribution("SUBSCRIPTION"), "sub pts");
        assert_eq!(humanize_contribution(""), "pts");
        assert_eq!(humanize_contribution("community_sub"), "community sub");
    }
}
