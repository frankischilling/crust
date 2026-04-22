use egui::{Align2, Color32, CornerRadius, Frame, Margin, Response, RichText, Sense, Stroke, Ui};

use crate::theme as t;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeIcon {
    Join,
    Sidebar,
    Tabs,
    Settings,
    Analytics,
    Whisper,
    Irc,
    Perf,
    Menu,
    Search,
    Close,
    Account,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IconButtonState {
    pub selected: bool,
    pub compact: bool,
    pub danger: bool,
}

impl Default for IconButtonState {
    fn default() -> Self {
        Self {
            selected: false,
            compact: false,
            danger: false,
        }
    }
}

pub fn icon_button(
    ui: &mut Ui,
    icon: ChromeIcon,
    tooltip: &str,
    state: IconButtonState,
) -> Response {
    let size = if state.compact {
        t::icon_btn_sm()
    } else {
        t::icon_btn()
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), Sense::click());
    let hovered = resp.hovered();
    let fill = if state.danger {
        if hovered {
            t::danger_bg().gamma_multiply(1.2)
        } else {
            t::danger_bg()
        }
    } else if state.selected {
        t::tab_selected_bg()
    } else if hovered {
        t::tab_hover_bg()
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if state.selected {
        Stroke::new(1.0, t::border_accent())
    } else if state.danger {
        Stroke::new(1.0, t::red().gamma_multiply(0.4))
    } else {
        Stroke::new(1.0, t::border_subtle().gamma_multiply(0.6))
    };
    ui.painter()
        .rect(rect, t::RADIUS_SM, fill, stroke, egui::StrokeKind::Middle);
    paint_icon(
        ui,
        rect.shrink(if state.compact { 4.0 } else { 4.5 }),
        icon,
        state,
    );
    resp.on_hover_text(tooltip)
}

pub fn toolbar_group_frame() -> Frame {
    Frame::new()
        .fill(t::bg_header())
        .stroke(Stroke::new(1.0, t::border_subtle()))
        .corner_radius(t::RADIUS)
        .inner_margin(Margin::symmetric(6, 3))
}

pub fn card_frame() -> Frame {
    Frame::new()
        .fill(t::bg_card())
        .stroke(Stroke::new(1.0, t::border_subtle()))
        .corner_radius(t::RADIUS)
        .inner_margin(t::CARD_MARGIN)
}

pub fn dialog_header(ui: &mut Ui, title: &str, subtitle: Option<&str>) {
    ui.label(
        RichText::new(title)
            .font(t::body())
            .strong()
            .color(t::text_primary()),
    );
    if let Some(subtitle) = subtitle {
        ui.label(
            RichText::new(subtitle)
                .font(t::small())
                .color(t::text_muted()),
        );
    }
}

pub fn pill(ui: &mut Ui, text: impl AsRef<str>, fg: Color32, bg: Color32) -> Response {
    Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, fg.gamma_multiply(0.35)))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text.as_ref())
                    .font(t::small())
                    .strong()
                    .color(fg),
            );
        })
        .response
}

fn paint_icon(ui: &Ui, rect: egui::Rect, icon: ChromeIcon, state: IconButtonState) {
    let stroke_color = if state.danger {
        t::red()
    } else if state.selected {
        t::text_primary()
    } else {
        t::text_secondary()
    };
    let stroke = Stroke::new(if state.compact { 1.4 } else { 1.6 }, stroke_color);
    let painter = ui.painter();
    match icon {
        ChromeIcon::Join => {
            painter.line_segment(
                [
                    egui::pos2(rect.center().x, rect.top()),
                    egui::pos2(rect.center().x, rect.bottom()),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left(), rect.center().y),
                    egui::pos2(rect.right(), rect.center().y),
                ],
                stroke,
            );
        }
        ChromeIcon::Sidebar => {
            painter.rect_stroke(
                rect,
                CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Outside,
            );
            let x = rect.left() + rect.width() * 0.34;
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                stroke,
            );
        }
        ChromeIcon::Tabs => {
            let h = rect.height() * 0.34;
            for idx in 0..2 {
                let y = rect.top() + idx as f32 * (h + 2.0);
                let r = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + 1.0 + idx as f32 * 2.0, y + idx as f32 * 2.0),
                    egui::vec2(rect.width() - 4.0, h),
                );
                painter.rect_stroke(r, CornerRadius::same(2), stroke, egui::StrokeKind::Outside);
            }
        }
        ChromeIcon::Settings => {
            painter.circle_stroke(rect.center(), rect.width() * 0.22, stroke);
            for idx in 0..8 {
                let angle = idx as f32 * std::f32::consts::TAU / 8.0;
                let inner =
                    rect.center() + egui::vec2(angle.cos(), angle.sin()) * rect.width() * 0.28;
                let outer =
                    rect.center() + egui::vec2(angle.cos(), angle.sin()) * rect.width() * 0.42;
                painter.line_segment([inner, outer], stroke);
            }
        }
        ChromeIcon::Analytics => {
            let widths = [0.2, 0.45, 0.7];
            for (idx, factor) in widths.into_iter().enumerate() {
                let x = rect.left() + 2.0 + idx as f32 * (rect.width() / 3.0);
                let bar = egui::Rect::from_min_size(
                    egui::pos2(x, rect.bottom() - rect.height() * factor),
                    egui::vec2(rect.width() / 5.0, rect.height() * factor - 1.0),
                );
                painter.rect_filled(bar, CornerRadius::same(1), stroke_color);
            }
        }
        ChromeIcon::Whisper => {
            let bubble = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 1.5, rect.top() + 2.0),
                egui::pos2(rect.right() - 1.5, rect.bottom() - 4.0),
            );
            painter.rect_stroke(
                bubble,
                CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Outside,
            );
            let tail_top = egui::pos2(bubble.left() + 5.0, bubble.bottom());
            painter.line_segment([tail_top, tail_top + egui::vec2(-2.5, 3.0)], stroke);
            painter.line_segment(
                [
                    tail_top + egui::vec2(-2.5, 3.0),
                    tail_top + egui::vec2(1.5, 2.0),
                ],
                stroke,
            );
            let y = bubble.center().y;
            painter.line_segment(
                [
                    egui::pos2(bubble.left() + 3.0, y),
                    egui::pos2(bubble.right() - 3.0, y),
                ],
                stroke,
            );
        }
        ChromeIcon::Irc => {
            painter.rect_stroke(
                rect,
                CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Outside,
            );
            let left = rect.left() + 3.0;
            let cy = rect.center().y;
            painter.line_segment(
                [egui::pos2(left + 2.0, cy - 3.0), egui::pos2(left, cy)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(left, cy), egui::pos2(left + 2.0, cy + 3.0)],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(left + 5.0, cy + 4.0),
                    egui::pos2(left + 9.0, cy + 4.0),
                ],
                stroke,
            );
        }
        ChromeIcon::Perf => {
            let p0 = egui::pos2(rect.left(), rect.center().y);
            let p1 = egui::pos2(rect.left() + rect.width() * 0.22, rect.center().y);
            let p2 = egui::pos2(
                rect.left() + rect.width() * 0.38,
                rect.center().y - rect.height() * 0.24,
            );
            let p3 = egui::pos2(
                rect.left() + rect.width() * 0.52,
                rect.center().y + rect.height() * 0.18,
            );
            let p4 = egui::pos2(
                rect.left() + rect.width() * 0.68,
                rect.center().y - rect.height() * 0.34,
            );
            let p5 = egui::pos2(rect.right(), rect.center().y - rect.height() * 0.05);
            painter.line_segment([p0, p1], stroke);
            painter.line_segment([p1, p2], stroke);
            painter.line_segment([p2, p3], stroke);
            painter.line_segment([p3, p4], stroke);
            painter.line_segment([p4, p5], stroke);
        }
        ChromeIcon::Menu => {
            for dx in [-4.0_f32, 0.0, 4.0] {
                painter.circle_filled(rect.center() + egui::vec2(dx, 0.0), 1.4, stroke_color);
            }
        }
        ChromeIcon::Search => {
            let c = rect.center() + egui::vec2(-1.0, -1.0);
            painter.circle_stroke(c, rect.width() * 0.22, stroke);
            painter.line_segment(
                [
                    c + egui::vec2(rect.width() * 0.15, rect.width() * 0.15),
                    rect.right_bottom() - egui::vec2(1.5, 1.5),
                ],
                stroke,
            );
        }
        ChromeIcon::Close => {
            painter.line_segment([rect.left_top(), rect.right_bottom()], stroke);
            painter.line_segment([rect.right_top(), rect.left_bottom()], stroke);
        }
        ChromeIcon::Account => {
            let head_r = rect.width() * 0.18;
            let head = rect.center_top() + egui::vec2(0.0, rect.height() * 0.25);
            painter.circle_stroke(head, head_r, stroke);
            let body = egui::Rect::from_center_size(
                rect.center_bottom() - egui::vec2(0.0, rect.height() * 0.18),
                egui::vec2(rect.width() * 0.62, rect.height() * 0.32),
            );
            painter.rect_stroke(
                body,
                CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Outside,
            );
        }
    }
    let _ = Align2::CENTER_CENTER;
}
