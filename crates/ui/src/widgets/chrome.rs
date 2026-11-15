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
    Emotes,
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
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            state.selected,
            tooltip,
        )
    });
    let hovered = resp.hovered();
    let focused = resp.has_focus();
    let fill = if state.danger {
        if hovered || focused {
            t::danger_bg().gamma_multiply(1.2)
        } else {
            t::danger_bg()
        }
    } else if state.selected {
        t::tab_selected_bg()
    } else if hovered || focused {
        t::tab_hover_bg()
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if focused {
        Stroke::new(2.0_f32, t::accent())
    } else if state.selected {
        Stroke::new(1.0_f32, t::border_accent())
    } else if state.danger {
        Stroke::new(1.0_f32, t::red().gamma_multiply(0.4))
    } else {
        Stroke::new(1.0_f32, t::border_subtle().gamma_multiply(0.6))
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
        .stroke(Stroke::new(1.0_f32, t::border_subtle()))
        .corner_radius(t::RADIUS)
        .inner_margin(Margin::symmetric(6, 3))
}

pub fn icon_menu_button<R>(
    ui: &mut Ui,
    icon: ChromeIcon,
    tooltip: &str,
    mut state: IconButtonState,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    let menu_id = ui.make_persistent_id(("icon_menu", tooltip));
    let mut menu = egui::menu::BarState::load(ui.ctx(), menu_id);
    state.selected |= menu.is_some();
    let response = icon_button(ui, icon, tooltip, state);
    let contents = menu.bar_menu(&response, add_contents);
    menu.store(ui.ctx(), menu_id);
    egui::InnerResponse::new(contents.map(|contents| contents.inner), response)
}

pub fn card_frame() -> Frame {
    Frame::new()
        .fill(t::bg_card())
        .stroke(Stroke::new(1.0_f32, t::border_subtle()))
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
                .color(t::text_helper()),
        );
    }
}

pub fn pill(ui: &mut Ui, text: impl AsRef<str>, fg: Color32, bg: Color32) -> Response {
    Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, fg.gamma_multiply(0.35)))
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
    let stroke = Stroke::new(if state.compact { 1.4_f32 } else { 1.6_f32 }, stroke_color);
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
            painter.rect_stroke(
                rect,
                CornerRadius::same(3),
                stroke,
                egui::StrokeKind::Outside,
            );
            let tab_bottom = rect.top() + rect.height() * 0.36;
            painter.line_segment(
                [
                    egui::pos2(rect.left(), tab_bottom),
                    egui::pos2(rect.right(), tab_bottom),
                ],
                stroke,
            );
            for fraction in [0.36, 0.7] {
                let x = rect.left() + rect.width() * fraction;
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, tab_bottom)],
                    stroke,
                );
            }
        }
        ChromeIcon::Settings => {
            let teeth = (0..32)
                .map(|index| {
                    let angle = index as f32 * std::f32::consts::TAU / 32.0;
                    let radius = if index % 4 < 2 { 0.46 } else { 0.34 };
                    rect.center() + egui::vec2(angle.cos(), angle.sin()) * rect.width() * radius
                })
                .collect();
            painter.add(egui::Shape::closed_line(teeth, stroke));
            painter.circle_stroke(rect.center(), rect.width() * 0.16, stroke);
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
            // An envelope distinguishes private messages from public chat.
            let envelope = rect.shrink2(egui::vec2(0.5, 2.5));
            painter.rect_stroke(
                envelope,
                CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
            let fold = egui::pos2(
                envelope.center().x,
                envelope.top() + envelope.height() * 0.56,
            );
            painter.line_segment([envelope.left_top() + egui::vec2(0.7, 0.8), fold], stroke);
            painter.line_segment([fold, envelope.right_top() + egui::vec2(-0.7, 0.8)], stroke);
            painter.line_segment(
                [
                    envelope.left_bottom() + egui::vec2(0.7, -0.8),
                    egui::pos2(
                        envelope.left() + envelope.width() * 0.34,
                        envelope.center().y,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(
                        envelope.right() - envelope.width() * 0.34,
                        envelope.center().y,
                    ),
                    envelope.right_bottom() + egui::vec2(-0.7, -0.8),
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
            let radius = (rect.width() * 0.1).max(0.7);
            for fraction in [-0.3_f32, 0.0, 0.3] {
                painter.circle_filled(
                    rect.center() + egui::vec2(rect.width() * fraction, 0.0),
                    radius,
                    stroke_color,
                );
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
        ChromeIcon::Emotes => {
            let radius = rect.width().min(rect.height()) * 0.44;
            let center = rect.center();
            painter.circle_stroke(center, radius, stroke);
            for direction in [-1.0, 1.0] {
                painter.circle_filled(
                    center + egui::vec2(direction * radius * 0.35, -radius * 0.25),
                    1.0,
                    stroke_color,
                );
            }
            let smile: Vec<_> = (0..=8)
                .map(|step| {
                    let angle = std::f32::consts::PI * (0.2 + step as f32 * 0.6 / 8.0);
                    center + egui::vec2(angle.cos(), angle.sin()) * radius * 0.55
                })
                .collect();
            painter.add(egui::Shape::line(smile, stroke));
        }
    }
    let _ = Align2::CENTER_CENTER;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>) -> (egui::FullOutput, [Response; 2]) {
        let mut responses = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 200.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        responses = Some([
                            icon_button(
                                ui,
                                ChromeIcon::Settings,
                                "Open application settings",
                                IconButtonState::default(),
                            ),
                            icon_button(
                                ui,
                                ChromeIcon::Account,
                                "Open accounts",
                                IconButtonState::default(),
                            ),
                        ]);
                    });
                });
            },
        );
        (output, responses.unwrap())
    }

    fn key(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn icon_buttons_show_keyboard_focus_and_publish_named_activation_events() {
        let ctx = egui::Context::default();
        let _ = frame(&ctx, Vec::new());
        let (_, buttons) = frame(&ctx, vec![key(egui::Key::Tab, true)]);
        assert!(ctx.memory(|memory| memory.has_focus(buttons[0].id)));

        let (output, buttons) = frame(
            &ctx,
            vec![key(egui::Key::Tab, false), key(egui::Key::Tab, true)],
        );
        assert!(buttons[1].has_focus());
        assert!(!buttons[0].has_focus());
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::epaint::Shape::Rect(rect) if rect.rect == buttons[1].rect
                && rect.stroke.width == 2.0 && rect.stroke.color == t::accent())));
        assert!(output
            .platform_output
            .events
            .iter()
            .any(|event| matches!(event,
            egui::output::OutputEvent::FocusGained(info)
                if info.typ == egui::WidgetType::Button && info.enabled
                    && info.label.as_deref() == Some("Open accounts"))));

        let (output, buttons) = frame(
            &ctx,
            vec![key(egui::Key::Tab, false), key(egui::Key::Enter, true)],
        );
        assert!(buttons[1].clicked());
        assert!(!buttons[0].clicked());
        assert!(output
            .platform_output
            .events
            .iter()
            .any(|event| matches!(event,
            egui::output::OutputEvent::Clicked(info)
                if info.typ == egui::WidgetType::Button
                    && info.label.as_deref() == Some("Open accounts"))));
    }
}
