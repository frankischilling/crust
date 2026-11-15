use crust_core::model::{ChannelId, SplitAxis};
use egui::RichText;

use crate::theme as t;
use crate::widgets::chrome::{self, ChromeIcon, IconButtonState};

/// Height of the per-split header strip. Scales with chat font so text +
/// icons don't clip at larger font sizes.
#[inline]
pub fn split_header_height() -> f32 {
    (26.0 * t::font_scale())
        .max(t::icon_btn_sm() + 4.0)
        .max(t::small().size + 4.0)
}
const HEADER_PAD_X: f32 = 6.0;
const HEADER_CTRL_GAP: f32 = 3.0;

#[derive(Default, Debug, Clone, Copy)]
pub struct SplitHeaderResult {
    pub focus_clicked: bool,
    pub close_clicked: bool,
    pub close_others_clicked: bool,
    pub toggle_search_clicked: bool,
    pub split: Option<SplitAxis>,
    pub reopen_clicked: bool,
    pub filters_clicked: bool,
    pub open_window_clicked: bool,
    pub open_overlay_clicked: bool,
    pub manage_windows_clicked: bool,
}

#[derive(Clone, Copy)]
pub struct SplitHeaderOptions {
    pub can_split: bool,
    pub can_reopen: bool,
    pub can_close_others: bool,
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
    options: SplitHeaderOptions,
) -> SplitHeaderResult {
    let pane_w = pane_rect.width();
    let compact = pane_w < 240.0 * t::font_scale();
    let ultra_compact = pane_w < 165.0 * t::font_scale();
    let can_search = !channel.is_virtual();

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
        egui::Stroke::new(1.0_f32, t::border_subtle()),
    );
    if is_focused {
        let accent = t::accent();
        let top_line =
            egui::Rect::from_min_size(hdr_rect.left_top(), egui::vec2(hdr_rect.width(), 2.0));
        ui.painter()
            .rect_filled(top_line, egui::CornerRadius::ZERO, t::alpha(accent, 180));
    }

    let mut result = SplitHeaderResult::default();
    let icon_size = t::icon_btn_sm();
    let show_close = pane_w >= HEADER_PAD_X * 2.0 + icon_size * 2.0 + HEADER_CTRL_GAP;
    let base_controls_w = HEADER_PAD_X * 2.0
        + icon_size
        + if show_close {
            icon_size + HEADER_CTRL_GAP
        } else {
            0.0
        };
    let show_search = can_search
        && pane_w >= base_controls_w + icon_size + HEADER_CTRL_GAP + 60.0 * t::font_scale();
    let controls_w = base_controls_w
        + if show_search {
            icon_size + HEADER_CTRL_GAP
        } else {
            0.0
        };

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
            (controls_rect.left() - HEADER_PAD_X).max(hdr_rect.left() + HEADER_PAD_X),
            hdr_rect.bottom(),
        ),
    );

    let mut controls_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(controls_rect.shrink2(egui::vec2(HEADER_PAD_X, 0.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    controls_ui.set_clip_rect(hdr_rect.intersect(ui.clip_rect()));
    controls_ui.spacing_mut().item_spacing.x = HEADER_CTRL_GAP;

    // Keep Close at the right edge as optional controls appear or disappear.
    if show_close {
        let close = chrome::icon_button(
            &mut controls_ui,
            ChromeIcon::Close,
            "Close split",
            IconButtonState {
                compact: true,
                danger: false,
                ..Default::default()
            },
        );
        result.close_clicked = close.clicked();
    }

    chrome::icon_menu_button(
        &mut controls_ui,
        ChromeIcon::Menu,
        "Split options",
        IconButtonState {
            compact: true,
            ..Default::default()
        },
        |ui| show_split_menu(ui, can_search, search_open, options, &mut result),
    );

    if show_search {
        let search = chrome::icon_button(
            &mut controls_ui,
            ChromeIcon::Search,
            "Toggle message search (Ctrl+F)",
            IconButtonState {
                compact: true,
                selected: search_open,
                toggle_state: Some(search_open),
                ..Default::default()
            },
        )
        .on_hover_text("Toggle message search (Ctrl+F)");
        if search.clicked() {
            result.toggle_search_clicked = true;
        }
    }

    let prefix = if channel.is_irc_query() {
        "@ "
    } else if ultra_compact
        || channel.is_kick()
        || channel.is_irc_server_tab()
        || channel.is_virtual()
    {
        ""
    } else {
        "# "
    };
    let display_name = if channel.is_live_feed() {
        "Live"
    } else if channel.is_mentions() {
        "Mentions"
    } else {
        channel.display_name()
    };
    let mut title = if !ultra_compact && unread_mentions > 0 {
        format!(
            "{}{} [{}]",
            prefix,
            display_name,
            badge_count_label(unread_mentions)
        )
    } else if !ultra_compact && unread_count > 0 {
        format!(
            "{}{} ({})",
            prefix,
            display_name,
            badge_count_label(unread_count)
        )
    } else {
        format!("{}{}", prefix, display_name)
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
    title_ui.set_clip_rect(title_rect.intersect(ui.clip_rect()));
    let title_resp = title_ui.add_sized(
        [title_rect.width(), split_header_height()],
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
        .truncate()
        .sense(egui::Sense::click()),
    );
    result.focus_clicked = title_resp.clicked();
    title_resp
        .context_menu(|ui| show_split_menu(ui, can_search, search_open, options, &mut result));
    let mut tooltip = format!("{prefix}{display_name}");
    if let Some(meta_text) = meta_text.filter(|text| !text.is_empty()) {
        tooltip.push('\n');
        tooltip.push_str(meta_text);
    }
    title_resp.on_hover_text(tooltip);

    ui.allocate_space(egui::vec2(pane_w, split_header_height()));
    result
}

fn show_split_menu(
    ui: &mut egui::Ui,
    can_search: bool,
    search_open: bool,
    options: SplitHeaderOptions,
    result: &mut SplitHeaderResult,
) {
    let frame_margin = egui::Frame::menu(ui.style()).total_margin().sum();
    let maximum = (ui.ctx().screen_rect().size() - frame_margin - egui::vec2(16.0, 16.0))
        .max(egui::Vec2::splat(1.0));
    let font = t::small();
    let label_width = ui.fonts(|fonts| {
        [
            "Split right",
            "Split below",
            "Reopen closed split",
            "Pane filters",
            "Show search",
            "Hide search",
            "Open in new window",
            "Open overlay",
            "Manage chat windows",
            "Close split",
            "Close other splits",
        ]
        .into_iter()
        .map(|label| {
            fonts
                .layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0, f32::max)
    });
    let scroll = &ui.spacing().scroll;
    let scroll_width = scroll.bar_width + scroll.bar_inner_margin + scroll.bar_outer_margin;
    let width = (label_width + ui.spacing().button_padding.x * 2.0 + scroll_width)
        .ceil()
        .min(maximum.x);
    ui.set_width(width);
    egui::ScrollArea::vertical()
        .id_salt("split_options_scroll")
        .max_width(width)
        .max_height(maximum.y)
        .min_scrolled_height(0.0)
        .show(ui, |ui| {
            show_split_menu_contents(ui, can_search, search_open, options, result)
        });
}

fn show_split_menu_contents(
    ui: &mut egui::Ui,
    can_search: bool,
    search_open: bool,
    options: SplitHeaderOptions,
    result: &mut SplitHeaderResult,
) {
    for (label, axis) in [
        ("Split right", SplitAxis::Horizontal),
        ("Split below", SplitAxis::Vertical),
    ] {
        if ui
            .add_enabled(
                options.can_split,
                egui::Button::new(RichText::new(label).font(t::small())),
            )
            .clicked()
        {
            result.split = Some(axis);
            ui.close_menu();
        }
    }
    if ui
        .add_enabled(
            options.can_reopen,
            egui::Button::new(RichText::new("Reopen closed split").font(t::small())),
        )
        .clicked()
    {
        result.reopen_clicked = true;
        ui.close_menu();
    }
    ui.separator();
    if can_search {
        if ui
            .button(RichText::new("Pane filters").font(t::small()))
            .clicked()
        {
            result.filters_clicked = true;
            ui.close_menu();
        }
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
    }
    ui.separator();
    if can_search
        && ui
            .button(RichText::new("Open in new window").font(t::small()))
            .clicked()
    {
        result.open_window_clicked = true;
        ui.close_menu();
    }
    if can_search
        && ui
            .button(RichText::new("Open overlay").font(t::small()))
            .clicked()
    {
        result.open_overlay_clicked = true;
        ui.close_menu();
    }
    if ui
        .button(RichText::new("Manage chat windows").font(t::small()))
        .clicked()
    {
        result.manage_windows_clicked = true;
        ui.close_menu();
    }
    ui.separator();
    if ui
        .button(RichText::new("Close split").font(t::small()))
        .clicked()
    {
        result.close_clicked = true;
        ui.close_menu();
    }
    if ui
        .add_enabled(
            options.can_close_others,
            egui::Button::new(RichText::new("Close other splits").font(t::small())),
        )
        .clicked()
    {
        result.close_others_clicked = true;
        ui.close_menu();
    }
}

fn badge_count_label(count: u32) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct HeaderViewport {
        size: egui::Vec2,
        pane: egui::Rect,
    }

    impl HeaderViewport {
        fn narrow_pane(width: f32) -> Self {
            Self {
                size: egui::vec2(width.max(400.0), 200.0),
                pane: egui::Rect::from_min_size(
                    egui::pos2(8.0, 8.0),
                    egui::vec2(width - 16.0, 184.0),
                ),
            }
        }

        fn at_right_edge(size: egui::Vec2, pane_width: f32) -> Self {
            Self {
                size,
                pane: egui::Rect::from_min_max(
                    egui::pos2((size.x - pane_width).max(8.0), 96.0),
                    egui::pos2(size.x - 8.0, size.y - 8.0),
                ),
            }
        }
    }

    fn header_frame(
        ctx: &egui::Context,
        width: f32,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, SplitHeaderResult, egui::Pos2) {
        viewport_frame(ctx, HeaderViewport::narrow_pane(width), events)
    }

    fn viewport_frame(
        ctx: &egui::Context,
        viewport: HeaderViewport,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, SplitHeaderResult, egui::Pos2) {
        let mut result = SplitHeaderResult::default();
        let mut close_center = egui::Pos2::ZERO;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.size)),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let pane = viewport.pane;
                    close_center = egui::pos2(
                        pane.right() - HEADER_PAD_X - t::icon_btn_sm() / 2.0,
                        pane.top() + split_header_height() / 2.0,
                    );
                    result = show_split_header(
                        ui,
                        pane,
                        &ChannelId::new("a_very_long_channel_name"),
                        true,
                        15,
                        2,
                        false,
                        Some("A long stream title that must not cover the controls"),
                        SplitHeaderOptions {
                            can_split: true,
                            can_reopen: true,
                            can_close_others: true,
                        },
                    );
                });
            },
        );
        (output, result, close_center)
    }

    #[test]
    fn close_button_stays_at_right_edge_at_each_header_width() {
        for width in [150.0, 210.0, 360.0] {
            let ctx = egui::Context::default();
            let (_, _, pos) = header_frame(&ctx, width, Vec::new());
            header_frame(
                &ctx,
                width,
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
            let (_, result, _) = header_frame(
                &ctx,
                width,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert!(
                result.close_clicked,
                "Close was not clickable at width {width}"
            );
            assert!(!result.toggle_search_clicked);
        }
    }

    fn text_rect(output: &egui::FullOutput, wanted: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            let egui::Shape::Text(text) = &shape.shape else {
                return None;
            };
            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
            (text.galley.text() == wanted && shape.clip_rect.contains_rect(rect)).then_some(rect)
        })
    }

    fn click(
        ctx: &egui::Context,
        width: f32,
        pos: egui::Pos2,
        button: egui::PointerButton,
    ) -> SplitHeaderResult {
        click_in_viewport(ctx, HeaderViewport::narrow_pane(width), pos, button)
    }

    fn click_in_viewport(
        ctx: &egui::Context,
        viewport: HeaderViewport,
        pos: egui::Pos2,
        button: egui::PointerButton,
    ) -> SplitHeaderResult {
        viewport_frame(
            ctx,
            viewport,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        viewport_frame(
            ctx,
            viewport,
            vec![egui::Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        )
        .1
    }

    fn find_menu_action(ctx: &egui::Context, viewport: HeaderViewport, action: &str) -> egui::Rect {
        let labels = [
            "Split right",
            "Split below",
            "Reopen closed split",
            "Pane filters",
            "Show search",
            "Open in new window",
            "Open overlay",
            "Manage chat windows",
            "Close split",
            "Close other splits",
        ];
        for _ in 0..40 {
            let mut output = egui::FullOutput::default();
            for _ in 0..8 {
                output = viewport_frame(ctx, viewport, Vec::new()).0;
            }
            if let Some(rect) = text_rect(&output, action) {
                assert!(
                    egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.size).contains_rect(rect),
                    "{action} escaped viewport {:?}: {rect:?}",
                    viewport.size
                );
                return rect;
            }
            let position = labels
                .iter()
                .find_map(|label| text_rect(&output, label))
                .expect("an open menu must keep at least one action visible")
                .center();
            viewport_frame(
                ctx,
                viewport,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -24.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        panic!("{action} remained unreachable in {:?}", viewport.size);
    }

    #[test]
    fn split_menu_keeps_hidden_actions_reachable_at_narrow_widths() {
        for width in [60.0, 150.0, 210.0, 360.0] {
            for action in ["Show search", "Close other splits", "Close split"] {
                let ctx = egui::Context::default();
                let (_, _, close) = header_frame(&ctx, width, Vec::new());
                header_frame(&ctx, width, Vec::new());
                let show_close =
                    width - 16.0 >= HEADER_PAD_X * 2.0 + t::icon_btn_sm() * 2.0 + HEADER_CTRL_GAP;
                let menu = if show_close {
                    close - egui::vec2(t::icon_btn_sm() + HEADER_CTRL_GAP, 0.0)
                } else {
                    close
                };
                click(&ctx, width, menu, egui::PointerButton::Primary);
                let target = find_menu_action(&ctx, HeaderViewport::narrow_pane(width), action);
                let result = click(&ctx, width, target.center(), egui::PointerButton::Primary);
                match action {
                    "Show search" => assert!(result.toggle_search_clicked),
                    "Close other splits" => assert!(result.close_others_clicked),
                    _ => assert!(result.close_clicked),
                }
            }
        }
    }

    #[test]
    fn right_edge_menu_actions_remain_clickable_when_the_viewport_requires_scrolling() {
        for (size, pane_width, row_height) in [
            (egui::vec2(220.0, 200.0), 220.0, 18.0),
            (egui::vec2(220.0, 200.0), 220.0, 32.0),
            (egui::vec2(640.0, 400.0), 150.0, 18.0),
            (egui::vec2(1280.0, 720.0), 640.0, 18.0),
        ] {
            for action in [
                "Open in new window",
                "Open overlay",
                "Manage chat windows",
                "Show search",
                "Close split",
                "Close other splits",
            ] {
                let ctx = egui::Context::default();
                ctx.style_mut(|style| style.spacing.interact_size.y = row_height);
                let viewport = HeaderViewport::at_right_edge(size, pane_width);
                let (_, _, close) = viewport_frame(&ctx, viewport, Vec::new());
                viewport_frame(&ctx, viewport, Vec::new());
                let menu = close - egui::vec2(t::icon_btn_sm() + HEADER_CTRL_GAP, 0.0);
                click_in_viewport(&ctx, viewport, menu, egui::PointerButton::Primary);
                let target = find_menu_action(&ctx, viewport, action);
                let result = click_in_viewport(
                    &ctx,
                    viewport,
                    target.center(),
                    egui::PointerButton::Primary,
                );
                let selected = match action {
                    "Open in new window" => result.open_window_clicked,
                    "Open overlay" => result.open_overlay_clicked,
                    "Manage chat windows" => result.manage_windows_clicked,
                    "Show search" => result.toggle_search_clicked,
                    "Close split" => result.close_clicked,
                    "Close other splits" => result.close_others_clicked,
                    _ => unreachable!(),
                };
                assert!(
                    selected,
                    "{action} did not activate at {size:?}, row height {row_height}"
                );
                let count = [
                    result.open_window_clicked,
                    result.open_overlay_clicked,
                    result.manage_windows_clicked,
                    result.toggle_search_clicked,
                    result.close_clicked,
                    result.close_others_clicked,
                    result.filters_clicked,
                    result.reopen_clicked,
                    result.split.is_some(),
                ]
                .into_iter()
                .filter(|value| *value)
                .count();
                assert_eq!(count, 1, "{action} activated another header action");
                let (output, _, _) = viewport_frame(&ctx, viewport, Vec::new());
                assert!(
                    text_rect(&output, "Manage chat windows").is_none(),
                    "selecting an action must close the menu"
                );
            }
        }
    }

    #[test]
    fn flat_menu_width_follows_its_labels_in_wide_viewports() {
        let mut previous_width: Option<f32> = None;
        for screen_width in [640.0, 1280.0, 2560.0] {
            let ctx = egui::Context::default();
            let viewport = HeaderViewport::at_right_edge(egui::vec2(screen_width, 720.0), 320.0);
            let (_, _, close) = viewport_frame(&ctx, viewport, Vec::new());
            viewport_frame(&ctx, viewport, Vec::new());
            let menu = close - egui::vec2(t::icon_btn_sm() + HEADER_CTRL_GAP, 0.0);
            click_in_viewport(&ctx, viewport, menu, egui::PointerButton::Primary);
            let target = find_menu_action(&ctx, viewport, "Manage chat windows");
            let layer = ctx.layer_id_at(target.center()).expect("menu layer");
            let menu_rect = ctx.memory(|memory| memory.area_rect(layer.id).expect("menu area"));
            let (output, _, _) = viewport_frame(&ctx, viewport, Vec::new());
            let widest_label = [
                "Reopen closed split",
                "Open in new window",
                "Manage chat windows",
            ]
            .into_iter()
            .map(|label| {
                text_rect(&output, label)
                    .expect("visible menu label")
                    .width()
            })
            .fold(0.0, f32::max);
            assert!(menu_rect.width() >= widest_label);
            assert!(
                menu_rect.width() <= widest_label + 64.0,
                "menu width {} exceeds its label width {widest_label} at screen width {screen_width}",
                menu_rect.width()
            );
            assert!(
                egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.size).contains_rect(menu_rect),
                "menu escaped the viewport: {menu_rect:?}"
            );
            if let Some(previous) = previous_width {
                assert!(
                    (menu_rect.width() - previous).abs() <= 1.0,
                    "menu width changed with screen width: {previous} to {}",
                    menu_rect.width()
                );
            }
            previous_width = Some(menu_rect.width());
        }
    }

    #[test]
    fn title_context_menu_exposes_split_actions() {
        let ctx = egui::Context::default();
        let width = 210.0;
        let (_, _, close) = header_frame(&ctx, width, Vec::new());
        header_frame(&ctx, width, Vec::new());
        click(
            &ctx,
            width,
            egui::pos2(28.0, close.y),
            egui::PointerButton::Secondary,
        );
        let target = find_menu_action(
            &ctx,
            HeaderViewport::narrow_pane(width),
            "Close other splits",
        );
        let result = click(&ctx, width, target.center(), egui::PointerButton::Primary);
        assert!(result.close_others_clicked);
        assert!(!result.close_clicked);
    }
}
