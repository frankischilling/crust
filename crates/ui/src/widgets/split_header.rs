use crust_core::model::ChannelId;
use egui::RichText;

use crate::theme as t;
use crate::widgets::chrome::{self, ChromeIcon, IconButtonState};

/// Height of the per-split header strip. Scales with chat font so text +
/// icons don't clip at larger font sizes.
#[inline]
pub fn split_header_height() -> f32 {
    26.0 * t::font_scale()
}
const HEADER_PAD_X: f32 = 6.0;
const HEADER_CTRL_SIZE: f32 = 18.0;
const HEADER_CTRL_GAP: f32 = 3.0;

#[derive(Default, Debug, Clone, Copy)]
pub struct SplitHeaderResult {
    pub close_clicked: bool,
    pub close_others_clicked: bool,
    pub toggle_search_clicked: bool,
}

pub fn show_split_header(
    ui: &mut egui::Ui,
    pane_rect: egui::Rect,
    channel: &ChannelId,
    is_focused: bool,
    unread_count: u32,
    unread_mentions: u32,
    search_open: bool,
    meta_text: Option<&str>,
) -> SplitHeaderResult {
    let pane_w = pane_rect.width();
    let compact = pane_w < 240.0;
    let ultra_compact = pane_w < 165.0;

    let hdr_rect =
        egui::Rect::from_min_size(pane_rect.min, egui::vec2(pane_w, split_header_height()));
    let hdr_fill = if is_focused {
        t::tab_selected_bg()
    } else {
        t::bg_header()
    };
    ui.painter()
        .rect_filled(hdr_rect, egui::CornerRadius::ZERO, hdr_fill);
    ui.painter().hline(
        hdr_rect.x_range(),
        hdr_rect.bottom(),
        egui::Stroke::new(1.0, t::border_subtle()),
    );
    if is_focused {
        let accent = t::accent();
        let top_line =
            egui::Rect::from_min_size(hdr_rect.left_top(), egui::vec2(hdr_rect.width(), 2.0));
        ui.painter()
            .rect_filled(top_line, egui::CornerRadius::ZERO, t::alpha(accent, 180));
    }

    let mut result = SplitHeaderResult::default();
    let mut control_count = 1_usize; // close
    if !ultra_compact {
        control_count += 1; // search
    }
    if !compact {
        control_count += 1; // menu
    }
    let controls_w = HEADER_PAD_X * 2.0
        + control_count as f32 * HEADER_CTRL_SIZE
        + control_count.saturating_sub(1) as f32 * HEADER_CTRL_GAP;

    let controls_rect = egui::Rect::from_min_max(
        egui::pos2(
            (hdr_rect.right() - controls_w).max(hdr_rect.left()),
            hdr_rect.top(),
        ),
        hdr_rect.right_bottom(),
    );
    let title_rect = egui::Rect::from_min_max(
        egui::pos2(hdr_rect.left() + HEADER_PAD_X, hdr_rect.top()),
        egui::pos2(
            (controls_rect.left() - HEADER_PAD_X).max(hdr_rect.left() + 10.0),
            hdr_rect.bottom(),
        ),
    );

    let mut controls_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(controls_rect.shrink2(egui::vec2(HEADER_PAD_X, 0.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    controls_ui.spacing_mut().item_spacing.x = HEADER_CTRL_GAP;

    if !compact {
        controls_ui.menu_button(
            RichText::new("...").font(t::small()).color(t::text_muted()),
            |ui| {
                let search_label = if search_open {
                    "Hide search"
                } else {
                    "Show search"
                };
                if ui
                    .button(RichText::new(search_label).font(t::small()))
                    .clicked()
                {
                    result.toggle_search_clicked = true;
                    ui.close_menu();
                }
                if ui
                    .button(RichText::new("Close split").font(t::small()))
                    .clicked()
                {
                    result.close_clicked = true;
                    ui.close_menu();
                }
                if ui
                    .button(RichText::new("Close other splits").font(t::small()))
                    .clicked()
                {
                    result.close_others_clicked = true;
                    ui.close_menu();
                }
            },
        );
    }

    let close = chrome::icon_button(
        &mut controls_ui,
        ChromeIcon::Close,
        "Close split",
        IconButtonState {
            compact: true,
            danger: false,
            ..Default::default()
        },
    )
    .on_hover_text("Close split");
    if close.clicked() {
        result.close_clicked = true;
    }

    if !ultra_compact {
        let search = chrome::icon_button(
            &mut controls_ui,
            ChromeIcon::Search,
            "Toggle message search (Ctrl+F)",
            IconButtonState {
                compact: true,
                selected: search_open,
                ..Default::default()
            },
        )
        .on_hover_text("Toggle message search (Ctrl+F)");
        if search.clicked() {
            result.toggle_search_clicked = true;
        }
    }

    let mut title = if !ultra_compact && unread_mentions > 0 {
        format!(
            "{}{} [{}]",
            if ultra_compact { "" } else { "# " },
            channel.display_name(),
            badge_count_label(unread_mentions)
        )
    } else if !ultra_compact && unread_count > 0 {
        format!(
            "{}{} ({})",
            if ultra_compact { "" } else { "# " },
            channel.display_name(),
            badge_count_label(unread_count)
        )
    } else {
        format!(
            "{}{}",
            if ultra_compact { "" } else { "# " },
            channel.display_name()
        )
    };
    if !compact {
        if let Some(meta_text) = meta_text.filter(|text| !text.is_empty()) {
            title.push_str("  |  ");
            title.push_str(meta_text);
        }
    }

    let mut title_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(title_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let title_resp = title_ui.add_sized(
        [title_rect.width().max(8.0), 16.0],
        egui::Label::new(
            RichText::new(title)
                .font(t::small())
                .strong()
                .color(if is_focused {
                    t::text_primary()
                } else {
                    t::text_secondary()
                }),
        )
        .truncate(),
    );
    if let Some(meta_text) = meta_text.filter(|text| !text.is_empty()) {
        title_resp.on_hover_text(meta_text);
    }

    ui.allocate_space(egui::vec2(pane_w, split_header_height()));
    result
}

fn badge_count_label(count: u32) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}
