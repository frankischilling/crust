use std::collections::VecDeque;

use egui::{
    Align, Button, CentralPanel, Context, Frame, Id, Key, Layout, RichText, ScrollArea, Stroke, Ui,
    ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
};
use regex::{Regex, RegexBuilder};

use crust_core::model::{ChannelId, ChatMessage};
use crust_core::search::contains_ignore_case;

use crate::theme as t;

use super::chrome;

const SEARCH_PANEL_MIN_WIDTH: f32 = 680.0;
const SEARCH_FIELD_MIN_WIDTH: f32 = 170.0;
const SEARCH_WINDOW_DEFAULT_WIDTH: f32 = 440.0;
const SEARCH_WINDOW_DEFAULT_HEIGHT: f32 = 280.0;
const SEARCH_LABEL_WIDTH: f32 = 72.0;

#[derive(Default, Clone)]
pub struct MessageSearchState {
    pub open: bool,
    pub username: String,
    pub keyword: String,
    pub regex: String,
    pub local_history_exhausted: bool,
    request_load_more_local: bool,
    compiled_regex: Option<Regex>,
    compiled_pattern: String,
    regex_error: Option<String>,
    focus_keyword: bool,
    focused_control: Option<Id>,
    pane: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchMatchStats {
    pub matched: usize,
    pub total: usize,
}

impl MessageSearchState {
    pub fn settings(&self) -> crust_core::model::PaneSearchSettings {
        crust_core::model::PaneSearchSettings {
            open: self.open, username: self.username.clone(), keyword: self.keyword.clone(), regex: self.regex.clone(),
        }
    }

    pub fn from_settings(settings: &crust_core::model::PaneSearchSettings, pane: Option<u64>) -> Self {
        let mut search = Self {
            open: settings.open, username: settings.username.clone(), keyword: settings.keyword.clone(),
            regex: settings.regex.clone(), pane, ..Default::default()
        };
        search.ensure_regex();
        search
    }

    pub fn for_pane(pane: u64) -> Self {
        Self { pane: Some(pane), ..Default::default() }
    }

    pub fn set_pane(&mut self, pane: Option<u64>) {
        if self.pane != pane {
            self.pane = pane;
            self.focused_control = None;
        }
    }

    fn scope_id(&self, id: Id) -> Id {
        self.pane.map_or(id, |pane| id.with(pane))
    }

    pub(crate) fn field_id(&self, channel: &ChannelId, key: &str) -> Id {
        self.scope_id(search_field_id(channel, key))
    }

    pub fn request_open(&mut self) {
        self.open = true;
        self.focus_keyword = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.focus_keyword = false;
        self.focused_control = None;
    }

    pub(crate) fn owns_keyboard(&self, ctx: &Context) -> bool {
        self.open && self.focused_control.is_some_and(|id| {
            ctx.memory(|memory| {
                memory.has_focus(id)
                    || (memory.focused().is_none() && memory.had_focus_last_frame(id))
            }) && ctx.read_response(id).is_some()
        })
    }

    pub fn clear_filters(&mut self) {
        self.username.clear();
        self.keyword.clear();
        self.regex.clear();
        self.request_load_more_local = false;
        self.compiled_regex = None;
        self.compiled_pattern.clear();
        self.regex_error = None;
        self.focus_keyword = true;
    }

    pub fn request_load_more_local(&mut self) {
        self.request_load_more_local = true;
    }

    pub fn take_load_more_local_request(&mut self) -> bool {
        std::mem::take(&mut self.request_load_more_local)
    }

    pub fn is_filtering(&self) -> bool {
        self.open
            && (!self.username.trim().is_empty()
                || !self.keyword.trim().is_empty()
                || !self.regex.trim().is_empty())
    }

    pub fn regex_error(&self) -> Option<&str> {
        self.regex_error.as_deref()
    }

    pub fn active_filter_count(&self) -> usize {
        [
            !self.username.trim().is_empty(),
            !self.keyword.trim().is_empty(),
            !self.regex.trim().is_empty(),
        ]
        .into_iter()
        .filter(|active| *active)
        .count()
    }

    pub fn stats(&mut self, messages: &VecDeque<ChatMessage>) -> SearchMatchStats {
        self.ensure_regex();
        let matched = if self.active_filter_count() == 0 {
            messages.len()
        } else {
            messages.iter().filter(|msg| self.matches(msg)).count()
        };
        SearchMatchStats {
            matched,
            total: messages.len(),
        }
    }

    pub fn matches(&self, msg: &ChatMessage) -> bool {
        let username = self.username.trim();
        if !username.is_empty()
            && !contains_ignore_case(&msg.sender.login, username)
            && !contains_ignore_case(&msg.sender.display_name, username)
        {
            return false;
        }

        let keyword = self.keyword.trim();
        let regex = self.regex.trim();
        if keyword.is_empty() && regex.is_empty() {
            return true;
        }
        let text = msg.plain_text();
        if !keyword.is_empty() && !contains_ignore_case(&text, keyword) {
            return false;
        }

        if regex.is_empty() {
            return true;
        }

        match &self.compiled_regex {
            Some(re) => re.is_match(&text),
            None => false,
        }
    }

    pub fn ensure_regex(&mut self) {
        let pattern = self.regex.trim();
        if pattern == self.compiled_pattern {
            return;
        }

        self.compiled_pattern = pattern.to_owned();
        self.compiled_regex = None;
        self.regex_error = None;

        if pattern.is_empty() {
            return;
        }

        match RegexBuilder::new(pattern).build() {
            Ok(re) => {
                self.compiled_regex = Some(re);
            }
            Err(err) => {
                self.regex_error = Some(err.to_string());
            }
        }
    }
}

pub fn should_use_search_window(width: f32) -> bool {
    width < SEARCH_PANEL_MIN_WIDTH
}

pub fn show_message_search_inline(
    ui: &mut Ui,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
    search: &mut MessageSearchState,
) -> f32 {
    let stroke = if search.is_filtering() {
        Stroke::new(1.0_f32, t::border_accent())
    } else {
        t::stroke_subtle()
    };
    chrome::card_frame()
        .fill(t::bg_raised())
        .corner_radius(t::RADIUS)
        .inner_margin(egui::Margin::same(10))
        .stroke(stroke)
        .show(ui, |ui| {
            show_message_search_contents(ui, channel, messages, search, false, false);
        })
        .response
        .rect
        .height()
}

pub fn show_message_search_window(
    ctx: &Context,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
    search: &mut MessageSearchState,
    always_on_top: bool,
) {
    let viewport_id = ViewportId::from_hash_of(search.scope_id(Id::new(("message_search", channel.as_str()))));
    let prefix = if channel.is_irc_query() { "@" } else if channel.is_kick() || channel.is_irc_server_tab() { "" } else { "#" };
    let title = format!("Search {prefix}{}", channel.display_name());
    let level = if always_on_top {
        egui::viewport::WindowLevel::AlwaysOnTop
    } else {
        egui::viewport::WindowLevel::Normal
    };
    let builder = ViewportBuilder::default()
        .with_title(title.clone())
        .with_inner_size([SEARCH_WINDOW_DEFAULT_WIDTH, SEARCH_WINDOW_DEFAULT_HEIGHT])
        .with_min_inner_size([360.0, 210.0])
        .with_resizable(true)
        .with_close_button(true)
        .with_active(true)
        .with_transparent(false)
        .with_window_level(level);

    ctx.show_viewport_immediate(viewport_id, builder, |child_ctx, class| {
        let owns_escape = !matches!(class, ViewportClass::Embedded)
            || search.owns_keyboard(child_ctx);
        if owns_escape && !child_ctx.is_context_menu_open()
            && child_ctx.input_mut(|input| crate::app::consume_exact_key(
                input, egui::Modifiers::CTRL, Key::F,
            ))
        {
            search.request_open();
        }
        let esc = owns_escape && !child_ctx.is_context_menu_open()
            && child_ctx.input_mut(|i| crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::Escape));
        let close_requested = child_ctx.input(|i| i.viewport().close_requested());
        if esc {
            search.close();
            if !matches!(class, ViewportClass::Embedded) {
                child_ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
        if close_requested {
            search.close();
        }

        if !search.open {
            return;
        }

        match class {
            ViewportClass::Embedded => {
                let mut open = search.open;
                egui::Window::new(title.clone())
                    .id(search.scope_id(Id::new("message_search_window").with(channel.as_str())))
                    .default_width(SEARCH_WINDOW_DEFAULT_WIDTH)
                    .default_height(SEARCH_WINDOW_DEFAULT_HEIGHT)
                    .min_width(320.0)
                    .resizable(true)
                    .collapsible(false)
                    .constrain(true)
                    .anchor(egui::Align2::CENTER_TOP, [0.0, 36.0])
                    .frame(
                        Frame::window(&child_ctx.style())
                            .fill(t::bg_dialog())
                            .corner_radius(t::RADIUS)
                            .stroke(Stroke::new(1.0_f32, t::border_accent())),
                    )
                    .open(&mut open)
                    .show(child_ctx, |ui| {
                        ui.set_min_width(
                            SEARCH_WINDOW_DEFAULT_WIDTH.min(child_ctx.screen_rect().width() - 24.0),
                        );
                        show_message_search_scroll_area(ui, channel, messages, search);
                    });
                search.open = open;
            }
            _ => show_message_search_native_panel(child_ctx, channel, messages, search),
        }
    });
}

fn show_message_search_native_panel(
    ctx: &Context,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
    search: &mut MessageSearchState,
) {
    CentralPanel::default()
        .frame(
            chrome::card_frame()
                .fill(t::bg_dialog())
                .stroke(Stroke::new(1.0_f32, t::border_subtle()))
                .inner_margin(egui::Margin::same(12)),
        )
        .show(ctx, |ui| {
            show_message_search_scroll_area(ui, channel, messages, search);
        });
}

fn show_message_search_scroll_area(
    ui: &mut Ui,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
    search: &mut MessageSearchState,
) {
    ScrollArea::vertical()
        .id_salt(search.scope_id(Id::new(("message_search_scroll", channel.as_str()))))
        .auto_shrink([false, false])
        .max_height(ui.available_height().max(0.0))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
            show_message_search_contents(ui, channel, messages, search, true, true);
        });
}

fn show_message_search_contents(
    ui: &mut Ui,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
    search: &mut MessageSearchState,
    prefer_compact: bool,
    detached: bool,
) {
    search.ensure_regex();
    let stats = search.stats(messages);
    let compact = prefer_compact || ui.available_width() < 560.0;

    ui.spacing_mut().item_spacing = egui::vec2(10.0, 8.0);

    show_message_search_header(ui, search, stats, compact, detached);

    ui.add_space(4.0);
    ui.separator();

    ui.add_space(2.0);

    let focus_keyword = std::mem::take(&mut search.focus_keyword);
    let username_id = search.field_id(channel, "username");
    let keyword_id = search.field_id(channel, "keyword");
    let regex_id = search.field_id(channel, "regex");
    if compact {
        search_field_row(
            ui, username_id, "Username", "mod, chatter...",
            &mut search.username, false, false,
        );
        search_field_row(
            ui, keyword_id, "Keyword", "giveaway, timeout...",
            &mut search.keyword, focus_keyword, false,
        );
        search_field_row(
            ui, regex_id, "Regex", r"^!|https?://",
            &mut search.regex, false, true,
        );
    } else {
        ui.columns(3, |cols| {
            cols[0].set_min_width(SEARCH_FIELD_MIN_WIDTH);
            cols[1].set_min_width(SEARCH_FIELD_MIN_WIDTH);
            cols[2].set_min_width(SEARCH_FIELD_MIN_WIDTH);
            search_field(
                &mut cols[0],
                "Username",
                "mod, chatter...",
                username_id,
                &mut search.username,
                false,
                false,
            );
            search_field(
                &mut cols[1],
                "Keyword",
                "giveaway, timeout...",
                keyword_id,
                &mut search.keyword,
                focus_keyword,
                false,
            );
            search_field(
                &mut cols[2],
                "Regex",
                r"^!|https?://",
                regex_id,
                &mut search.regex,
                false,
                true,
            );
        });
    }

    search.ensure_regex();
    ui.add_space(2.0);
    if let Some(err) = search.regex_error() {
        ui.label(
            RichText::new(format!("Regex error: {err}"))
                .font(t::small())
                .color(t::red()),
        );
    } else {
        ui.label(
            RichText::new(
                "Username and keyword ignore case. Messages must match every active filter.",
            )
            .font(t::small())
            .color(t::text_secondary()),
        );
    }
    search.focused_control = ui.ctx().memory(|memory| memory.focused()).filter(|id| {
        ui.ctx().read_response(*id).is_some_and(|response| {
            response.layer_id == ui.layer_id() && ui.min_rect().contains_rect(response.rect)
        })
    });
}

fn show_message_search_header(
    ui: &mut Ui,
    search: &mut MessageSearchState,
    stats: SearchMatchStats,
    compact: bool,
    detached: bool,
) {
    let title = RichText::new("Search chat history").font(t::body()).strong().color(t::text_primary());
    let stat_text = if search.active_filter_count() == 0 {
        format!("{} / {}", stats.matched, stats.total)
    } else {
        format!("{} / {} • {} filters", stats.matched, stats.total, search.active_filter_count())
    };
    let text_width = |text: RichText| {
        egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Extend),
            f32::INFINITY, egui::FontSelection::Default).size().x.ceil()
    };
    let button_width = |label: &str| {
        (text_width(RichText::new(label).font(t::small())) + 2.0 * ui.spacing().button_padding.x)
            .max(ui.spacing().interact_size.x)
    };
    let close_width = if detached { 0.0 } else { button_width("Close") };
    let load_label = if search.local_history_exhausted { "No older logs" } else { "Load older logs" };
    let gap = ui.spacing().item_spacing.x;
    let actions_width = text_width(RichText::new(&stat_text).font(t::tiny())) + 14.0
        + button_width("Clear") + button_width(load_label) + close_width
        + gap * if detached { 2.0 } else { 3.0 };
    let wrap_actions = compact || text_width(title.clone()) + gap + actions_width > ui.available_width();
    let reserved_width = if wrap_actions { close_width } else { actions_width };
    let title_width = (ui.available_width() - reserved_width
        - if reserved_width > 0.0 { gap } else { 0.0 }).max(0.0);

    ui.horizontal(|ui| {
        // Reserve the action strip before laying out the title. Measuring only
        // text avoids a hidden sizing pass that could disturb an active field.
        ui.allocate_ui_with_layout(egui::vec2(title_width, ui.spacing().interact_size.y),
            Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(title_width);
                ui.add(egui::Label::new(title).truncate());
            });
        if !wrap_actions {
            show_message_search_actions(ui, search, &stat_text);
        }
        if !detached && ui.add(Button::new(RichText::new("Close").font(t::small())))
            .on_hover_text("Close search (Esc)").clicked()
        {
            search.close();
        }
    });
    if wrap_actions {
        ui.horizontal_wrapped(|ui| show_message_search_actions(ui, search, &stat_text));
    }
}

fn show_message_search_actions(ui: &mut Ui, search: &mut MessageSearchState, stat_text: &str) {
    stat_pill(ui, stat_text);
    if ui.add(Button::new(RichText::new("Clear").font(t::small())))
        .on_hover_text("Clear all filters").clicked()
    {
        search.clear_filters();
    }
    if ui.add_enabled(!search.local_history_exhausted, Button::new(
        RichText::new(if search.local_history_exhausted { "No older logs" } else { "Load older logs" }).font(t::small()),
    )).on_hover_text("Search earlier messages saved on this computer").clicked()
    {
        search.request_load_more_local();
    }
}

fn stat_pill(ui: &mut Ui, text: &str) {
    Frame::new()
        .fill(t::bg_surface())
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .stroke(Stroke::new(1.0_f32, t::border_accent()))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(t::tiny())
                    .color(t::text_secondary()),
            );
        });
}

fn search_field(
    ui: &mut Ui,
    label: &str,
    hint: &str,
    id: Id,
    value: &mut String,
    request_focus: bool,
    code_editor: bool,
) {
    ui.vertical(|ui| {
        ui.set_width(ui.available_width().max(SEARCH_FIELD_MIN_WIDTH));
        ui.label(
            RichText::new(label)
                .font(t::small())
                .color(t::text_secondary()),
        );
        let mut edit = egui::TextEdit::singleline(value)
            .id(id)
            .hint_text(hint)
            .margin(egui::Margin::symmetric(6, 5))
            .desired_width(f32::INFINITY);
        if code_editor {
            edit = edit.code_editor();
        }
        let response = ui.add_sized([ui.available_width(), 28.0], edit);
        if request_focus {
            focus_search_field(ui.ctx(), &response, value);
        }
    });
}

fn search_field_id(channel: &ChannelId, key: &str) -> Id {
    Id::new("message_search_field").with(channel.as_str()).with(key)
}

fn focus_search_field(ctx: &Context, response: &egui::Response, value: &str) {
    response.request_focus();
    // TextEdit returns its inner text rectangle. Scroll the registered outer
    // bounds so the field's frame and padding are visible too.
    let field = ctx.read_response(response.id).unwrap_or_else(|| response.clone());
    field.scroll_to_me(None);
    if let Some(mut state) = egui::TextEdit::load_state(ctx, response.id) {
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(value.chars().count()),
        )));
        egui::TextEdit::store_state(ctx, response.id, state);
    }
}

fn search_field_row(
    ui: &mut Ui,
    id: Id,
    label: &str,
    hint: &str,
    value: &mut String,
    request_focus: bool,
    code_editor: bool,
) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [SEARCH_LABEL_WIDTH, 28.0],
            egui::Label::new(RichText::new(label).font(t::small()).color(t::text_secondary())),
        );
        let mut edit = egui::TextEdit::singleline(value)
            .id(id)
            .hint_text(hint)
            .margin(egui::Margin::symmetric(6, 5))
            .desired_width(f32::INFINITY);
        if code_editor {
            edit = edit.code_editor();
        }
        let response = ui.add_sized([ui.available_width(), 28.0], edit);
        if request_focus {
            focus_search_field(ui.ctx(), &response, value);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &Context, search: &mut MessageSearchState, width: f32, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            CentralPanel::default().show(ctx, |ui| {
                show_message_search_contents(ui, &ChannelId::new("rustlang"), &VecDeque::new(), search, false, false);
            });
        });
    }

    #[test]
    fn inline_search_reserves_its_border_and_padding() {
        for width in [680.0, 900.0, 1200.0] {
            let ctx = Context::default();
            let mut search = MessageSearchState::default();
            search.request_open();
            let _ = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO, egui::vec2(width, 600.0),
                )),
                ..Default::default()
            }, |ctx| {
                CentralPanel::default().show(ctx, |ui| {
                    let mut search_ui = ui.new_child(egui::UiBuilder::new()
                        .max_rect(ui.available_rect_before_wrap())
                        .layout(Layout::top_down(Align::LEFT)));
                    let reserved = show_message_search_inline(
                        &mut search_ui, &ChannelId::new("example"),
                        &VecDeque::new(), &mut search,
                    );
                    let painted = search_ui.min_rect().height();
                    assert!(reserved >= painted - 0.1,
                        "search reserves {reserved}px but occupies {painted}px at {width}px width");
                });
            });
        }
    }

    #[test]
    fn short_search_windows_scroll_to_all_fields_without_expanding() {
        let ctx = Context::default();
        let channel = ChannelId::new("example");
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(360.0, 210.0));
        let mut search = MessageSearchState::default();
        search.request_open();
        search.regex = format!("({}", "x".repeat(400));
        for pass in 0..5 {
            let events = if pass == 2 {
                vec![egui::Event::PointerMoved(screen.center()), egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -80.0),
                    modifiers: egui::Modifiers::NONE,
                }]
            } else {
                Vec::new()
            };
            let _ = ctx.run(egui::RawInput {
                screen_rect: Some(screen), events, time: Some(pass as f64 * 0.1),
                ..Default::default()
            }, |ctx| {
                CentralPanel::default().show(ctx, |ui| {
                    show_message_search_scroll_area(ui, &channel, &VecDeque::new(), &mut search);
                    assert!(screen.expand(0.5).contains_rect(ui.min_rect()),
                        "search expanded beyond its window: {:?}", ui.min_rect());
                });
            });
        }
        assert!(search.regex_error().is_some());
        let field = ctx.read_response(search_field_id(&channel, "regex")).unwrap();
        assert!(screen.contains_rect(field.rect), "regex field cannot be reached by scrolling: {:?}", field.rect);
    }

    #[test]
    fn compact_native_search_keeps_help_reachable_and_controls_clickable() {
        fn find_text(shape: &egui::epaint::Shape, clip: egui::Rect, needle: &str)
            -> Option<(egui::Rect, egui::Rect)> {
            match shape {
                egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find_text(shape, clip, needle)),
                egui::epaint::Shape::Text(shape) if shape.galley.text().contains(needle) => {
                    Some((egui::Rect::from_min_size(shape.pos, shape.galley.size()), clip))
                }
                _ => None,
            }
        }
        let text_rect = |output: &egui::FullOutput, needle: &str| {
            output.shapes.iter().find_map(|shape| find_text(&shape.shape, shape.clip_rect, needle))
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        for size in [egui::vec2(SEARCH_WINDOW_DEFAULT_WIDTH, SEARCH_WINDOW_DEFAULT_HEIGHT), egui::vec2(360.0, 210.0)] {
            for pixels_per_point in [1.0, 1.25] {
                let ctx = Context::default();
                ctx.style_mut(|style| {
                    style.animation_time = 0.0;
                    style.scroll_animation = egui::style::ScrollAnimation::none();
                });
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                let channel = ChannelId::new("example");
                let mut search = MessageSearchState::for_pane(71);
                search.username = "viewer".into();
                search.keyword = "sample".into();
                search.regex = "message".into();
                search.request_open();
                let render = |search: &mut MessageSearchState, events| {
                    let mut input = egui::RawInput {
                        screen_rect: Some(screen), events,
                        time: Some(ctx.cumulative_pass_nr() as f64 * 0.2),
                        ..Default::default()
                    };
                    input.viewports.get_mut(&ViewportId::ROOT).unwrap().native_pixels_per_point = Some(pixels_per_point);
                    ctx.run(input, |ctx| show_message_search_native_panel(ctx, &channel, &VecDeque::new(), search))
                };
                let mut output = egui::FullOutput::default();
                for _ in 0..3 { output = render(&mut search, Vec::new()); }
                assert_eq!(ctx.pixels_per_point(), pixels_per_point);
                if size.y == SEARCH_WINDOW_DEFAULT_HEIGHT {
                    let (help, clip) = text_rect(&output, "Username and keyword ignore case.");
                    assert!(clip.expand(0.5).contains_rect(help),
                        "default native popup clipped its help at {pixels_per_point}x: {help:?} in {clip:?}");
                }

                for delta in [-600.0, 600.0] {
                    let _ = render(&mut search, vec![egui::Event::PointerMoved(screen.center()), egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, delta), modifiers: egui::Modifiers::NONE,
                    }]);
                    for _ in 0..60 {
                        output = render(&mut search, Vec::new());
                        if ctx.input(|input| input.time_since_last_scroll() > 0.0) { break; }
                    }
                    let expected = if delta < 0.0 { "Username and keyword ignore case." } else { "Load older logs" };
                    let (rect, clip) = text_rect(&output, expected);
                    assert!(screen.contains_rect(rect) && clip.expand(0.5).contains_rect(rect),
                        "{expected} cannot be reached at {size:?}/{pixels_per_point}x: {rect:?} in {clip:?}");
                }
                let clear = text_rect(&output, "Clear").0.center();
                for pressed in [true, false] {
                    let _ = render(&mut search, vec![egui::Event::PointerMoved(clear), egui::Event::PointerButton {
                        pos: clear, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
                    }]);
                }
                assert!(search.username.is_empty() && search.keyword.is_empty() && search.regex.is_empty());
                for _ in 0..2 { let _ = render(&mut search, Vec::new()); }
                assert!(ctx.memory(|memory| memory.has_focus(search.field_id(&channel, "keyword"))));
            }
        }
    }

    #[test]
    fn closing_search_disables_filters_and_reopening_restores_them() {
        let mut search = MessageSearchState::default();
        search.request_open();
        search.keyword = "needle".to_owned();
        assert!(search.is_filtering());
        search.close();
        assert!(!search.is_filtering());
        assert_eq!(search.keyword, "needle");
        search.request_open();
        assert!(search.is_filtering());
    }

    #[test]
    fn search_header_controls_remain_visible_and_do_not_overlap_at_narrow_widths() {
        fn collect_text(shape: &egui::epaint::Shape, clip: egui::Rect,
            text: &mut Vec<(String, egui::Rect, egui::Rect)>) {
            match shape {
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes { collect_text(shape, clip, text); }
                }
                egui::epaint::Shape::Text(shape) => text.push((shape.galley.text().to_owned(),
                    egui::Rect::from_min_size(shape.pos, shape.galley.size()), clip)),
                _ => {}
            }
        }

        for width in [360.0, 480.0, 640.0] {
            for pixels_per_point in [1.0, 1.875] {
                let ctx = Context::default();
                let mut search = MessageSearchState::default();
                search.request_open();
                search.username = "viewer".into();
                search.keyword = "sample".into();
                search.regex = "offline|Preview".into();
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0));
                let mut input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
                // Supply monitor scaling as viewport input. Changing zoom before
                // the first pass would rescale egui's default 10,000-point screen
                // and override this test's requested logical window size.
                input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().native_pixels_per_point = Some(pixels_per_point);
                let output = ctx.run(input, |ctx| {
                    CentralPanel::default().show(ctx, |ui| {
                        show_message_search_inline(ui, &ChannelId::new("rustlang"), &VecDeque::new(), &mut search);
                    });
                });
                assert_eq!(ctx.input(|input| input.screen_rect), screen);
                assert_eq!(ctx.pixels_per_point(), pixels_per_point);
                let mut text = Vec::new();
                for shape in &output.shapes { collect_text(&shape.shape, shape.clip_rect, &mut text); }
                let fields_top = text.iter().find(|(text, _, _)| text == "Username").unwrap().1.top();
                let mut header: Vec<_> = text.into_iter().filter(|(_, rect, _)| rect.bottom() < fields_top).collect();
                for expected in ["Search chat history", "Close", "Clear", "Load older logs", "0 / 0 • 3 filters"] {
                    assert!(header.iter().any(|(text, _, _)| text == expected), "missing {expected} at {width}px");
                }
                for (text, rect, clip) in &mut header {
                    assert!(clip.expand(0.5).contains_rect(*rect), "clipped {text} at {width}px: {rect:?} in {clip:?}");
                    let padding = if matches!(text.as_str(), "Close" | "Clear" | "Load older logs") {
                        ctx.style().spacing.button_padding
                    } else if text.contains("filters") {
                        egui::vec2(6.0, 3.0)
                    } else {
                        egui::Vec2::ZERO
                    };
                    *rect = rect.expand2(padding);
                    assert!(screen.contains_rect(*rect), "header overflow at {width}px: {text} {rect:?}");
                }
                for (index, (text, rect, _)) in header.iter().enumerate() {
                    for (other, other_rect, _) in &header[index + 1..] {
                        assert!(!rect.intersects(*other_rect),
                            "overlapping header at {width}px/{pixels_per_point}x: {text} {rect:?} and {other} {other_rect:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn reopening_search_selects_the_existing_keyword() {
        let ctx = Context::default();
        let mut search = MessageSearchState::default();
        search.keyword = "café 🙂".into();
        search.request_open();
        frame(&ctx, &mut search, 1000.0, Vec::new());
        frame(&ctx, &mut search, 1000.0, vec![egui::Event::Text("replacement".into())]);
        assert_eq!(search.keyword, "replacement");
        search.request_open();
        frame(&ctx, &mut search, 420.0, Vec::new());
        frame(&ctx, &mut search, 420.0, vec![egui::Event::Text("updated".into())]);
        assert_eq!(search.keyword, "updated");
    }

    #[test]
    fn reopening_short_search_scrolls_the_keyword_into_view() {
        for embedded in [false, true] {
            let ctx = Context::default();
            ctx.style_mut(|style| {
                style.animation_time = 0.0;
                style.scroll_animation = egui::style::ScrollAnimation::none();
            });
            let channel = ChannelId::new("example");
            let keyword_id = search_field_id(&channel, "keyword");
            let window_id = Id::new("message_search_window").with(channel.as_str());
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO,
                if embedded { egui::vec2(640.0, 480.0) } else { egui::vec2(360.0, 210.0) });
            let mut search = MessageSearchState::default();
            search.keyword = "café 🙂".into();
            // Keep enough error text below the fields to scroll the keyword
            // away even when the popup's initial height fits the normal form.
            search.regex = format!("({}", "x".repeat(1600));
            search.request_open();
            let frame = |search: &mut MessageSearchState, events| {
                let mut field = None;
                let _ = ctx.run(egui::RawInput {
                    screen_rect: Some(screen), events,
                    time: Some(ctx.cumulative_pass_nr() as f64 * 0.2),
                    ..Default::default()
                }, |ctx| {
                    if embedded {
                        show_message_search_window(ctx, &channel, &VecDeque::new(), search, false);
                    } else {
                        CentralPanel::default().show(ctx, |ui| {
                            show_message_search_scroll_area(ui, &channel, &VecDeque::new(), search);
                        });
                    }
                    // Capture the current pass before egui rotates its widget
                    // history at the end of the frame.
                    field = ctx.read_response(keyword_id);
                });
                field.unwrap()
            };
            for _ in 0..3 { let _ = frame(&mut search, Vec::new()); }
            let pointer = if embedded {
                ctx.memory(|memory| memory.area_rect(window_id)).unwrap().center()
            } else {
                screen.center()
            };
            let _ = frame(&mut search, vec![egui::Event::PointerMoved(pointer), egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -600.0),
                modifiers: egui::Modifiers::NONE,
            }]);
            // Finish the wheel gesture before refocusing. Its smoothed input
            // would otherwise keep scrolling after the focus request.
            for _ in 0..60 {
                let _ = frame(&mut search, Vec::new());
                if ctx.input(|input| input.time_since_last_scroll() > 0.0) {
                    break;
                }
            }
            assert!(ctx.input(|input| input.time_since_last_scroll() > 0.0),
                "the wheel gesture must finish before refocusing (embedded={embedded})");
            let field = frame(&mut search, Vec::new());
            assert!(!field.interact_rect.contains_rect(field.rect),
                "the test must scroll the keyword out of view (embedded={embedded})");
            search.request_open();
            for _ in 0..2 { let _ = frame(&mut search, Vec::new()); }
            let field = frame(&mut search, Vec::new());
            assert!(field.interact_rect.contains_rect(field.rect),
                "the requested keyword must be fully visible (embedded={embedded}): {:?} in {:?}", field.rect, field.interact_rect);
            assert!(ctx.memory(|memory| memory.has_focus(keyword_id)));
            let _ = frame(&mut search, vec![egui::Event::Text("replacement".into())]);
            assert_eq!(search.keyword, "replacement");
        }
    }

    #[test]
    fn search_fields_keep_focus_while_typing_and_resizing() {
        let ctx = Context::default();
        let channel = ChannelId::new("rustlang");
        let mut search = MessageSearchState::default();
        search.request_open();
        frame(&ctx, &mut search, 1000.0, Vec::new());
        assert!(ctx.memory(|m| m.has_focus(search_field_id(&channel, "keyword"))));

        for (key, first, second) in [("username", "alice", "bob"), ("regex", "^test", "$" )] {
            ctx.memory_mut(|m| m.request_focus(search_field_id(&channel, key)));
            frame(&ctx, &mut search, 1000.0, vec![egui::Event::Text(first.to_owned())]);
            assert!(ctx.memory(|m| m.has_focus(search_field_id(&channel, key))),
                "focus lost before resize in {key}: {:?}; values = {:?}, {:?}, {:?}",
                ctx.memory(|m| m.focused()), search.username, search.keyword, search.regex);
            frame(&ctx, &mut search, 420.0, vec![egui::Event::Text(second.to_owned())]);
            assert!(ctx.memory(|m| m.has_focus(search_field_id(&channel, key))),
                "focus lost after resize in {key}: {:?}; values = {:?}, {:?}, {:?}",
                ctx.memory(|m| m.focused()), search.username, search.keyword, search.regex);
        }
        assert_eq!(search.username, "alicebob");
        assert_eq!(search.regex, "^test$");
        assert!(search.keyword.is_empty());
        assert_eq!(search.compiled_pattern, "^test$");

        search.clear_filters();
        frame(&ctx, &mut search, 420.0, Vec::new());
        assert!(ctx.memory(|m| m.has_focus(search_field_id(&channel, "keyword"))));
    }
}
