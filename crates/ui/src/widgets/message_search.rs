use std::collections::VecDeque;

use egui::{
    Align, Button, CentralPanel, Color32, Context, Frame, Grid, Id, Key, Layout, RichText, Stroke,
    Ui, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
};
use regex::{Regex, RegexBuilder};

use crust_core::model::{ChannelId, ChatMessage};

use crate::theme as t;

const SEARCH_PANEL_MIN_WIDTH: f32 = 680.0;
const SEARCH_FIELD_MIN_WIDTH: f32 = 170.0;
const SEARCH_WINDOW_DEFAULT_WIDTH: f32 = 440.0;
const SEARCH_WINDOW_DEFAULT_HEIGHT: f32 = 220.0;
const SEARCH_LABEL_WIDTH: f32 = 72.0;

#[derive(Default, Clone)]
pub struct MessageSearchState {
    pub open: bool,
    pub username: String,
    pub keyword: String,
    pub regex: String,
    compiled_regex: Option<Regex>,
    compiled_pattern: String,
    regex_error: Option<String>,
    focus_keyword: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchMatchStats {
    pub matched: usize,
    pub total: usize,
}

impl MessageSearchState {
    pub fn request_open(&mut self) {
        self.open = true;
        self.focus_keyword = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.focus_keyword = false;
    }

    pub fn clear_filters(&mut self) {
        self.username.clear();
        self.keyword.clear();
        self.regex.clear();
        self.compiled_regex = None;
        self.compiled_pattern.clear();
        self.regex_error = None;
        self.focus_keyword = true;
    }

    pub fn is_filtering(&self) -> bool {
        !self.username.trim().is_empty()
            || !self.keyword.trim().is_empty()
            || !self.regex.trim().is_empty()
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
        let matched = messages.iter().filter(|msg| self.matches(msg)).count();
        SearchMatchStats {
            matched,
            total: messages.len(),
        }
    }

    pub fn matches(&self, msg: &ChatMessage) -> bool {
        let username = self.username.trim();
        if !username.is_empty() {
            let username = username.to_lowercase();
            let login = msg.sender.login.to_lowercase();
            let display = msg.sender.display_name.to_lowercase();
            if !login.contains(&username) && !display.contains(&username) {
                return false;
            }
        }

        let keyword = self.keyword.trim();
        if !keyword.is_empty() {
            let keyword = keyword.to_lowercase();
            if !msg.raw_text.to_lowercase().contains(&keyword) {
                return false;
            }
        }

        let regex = self.regex.trim();
        if regex.is_empty() {
            return true;
        }

        match &self.compiled_regex {
            Some(re) => re.is_match(&msg.raw_text),
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
    let mut height = 0.0;
    let stroke = if search.is_filtering() {
        Stroke::new(1.0, t::border_accent())
    } else {
        t::stroke_subtle()
    };
    Frame::new()
        .fill(t::bg_raised())
        .corner_radius(t::RADIUS)
        .inner_margin(egui::Margin::same(10))
        .stroke(stroke)
        .show(ui, |ui| {
            show_message_search_contents(ui, channel, messages, search, false, false);
            height = ui.min_rect().height();
        });
    height
}

pub fn show_message_search_window(
    ctx: &Context,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
    search: &mut MessageSearchState,
    always_on_top: bool,
) {
    let viewport_id = ViewportId::from_hash_of(("message_search", channel.as_str()));
    let title = format!("Search #{}", channel.display_name());
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
        let esc = child_ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
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
                    .id(Id::new("message_search_window").with(channel.as_str()))
                    .default_width(SEARCH_WINDOW_DEFAULT_WIDTH)
                    .min_width(320.0)
                    .resizable(true)
                    .collapsible(false)
                    .constrain(true)
                    .anchor(egui::Align2::CENTER_TOP, [0.0, 36.0])
                    .frame(
                        Frame::window(&child_ctx.style())
                            .fill(t::bg_dialog())
                            .corner_radius(t::RADIUS)
                            .stroke(Stroke::new(1.0, t::border_accent())),
                    )
                    .open(&mut open)
                    .show(child_ctx, |ui| {
                        ui.set_min_width(
                            SEARCH_WINDOW_DEFAULT_WIDTH.min(child_ctx.screen_rect().width() - 24.0),
                        );
                        show_message_search_contents(ui, channel, messages, search, true, true);
                    });
                search.open = open;
            }
            _ => {
                CentralPanel::default()
                    .frame(
                        Frame::new()
                            .fill(t::bg_dialog())
                            .stroke(Stroke::new(1.0, t::border_subtle()))
                            .inner_margin(egui::Margin::same(12)),
                    )
                    .show(child_ctx, |ui| {
                        show_message_search_contents(ui, channel, messages, search, true, true);
                    });
            }
        }
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

    ui.horizontal(|ui| {
        ui.label(RichText::new("⌕").color(t::accent()).size(18.0));
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Search chat history")
                    .font(t::body())
                    .strong(),
            );
            ui.label(
                RichText::new(if detached {
                    "Detached search window for this channel"
                } else {
                    "Filter by username, keyword, or Rust regex"
                })
                .font(t::small())
                .color(t::text_muted()),
            );
        });

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !detached {
                if ui
                    .add(Button::new(RichText::new("Close").font(t::small())))
                    .on_hover_text("Close search (Esc)")
                    .clicked()
                {
                    search.close();
                }
            }
            if ui
                .add(Button::new(RichText::new("Clear").font(t::small())))
                .on_hover_text("Clear all filters")
                .clicked()
            {
                search.clear_filters();
            }
            stat_pill(
                ui,
                if search.active_filter_count() == 0 {
                    format!("{} / {}", stats.matched, stats.total)
                } else {
                    format!(
                        "{} / {} • {} filters",
                        stats.matched,
                        stats.total,
                        search.active_filter_count()
                    )
                },
            );
        });
    });

    ui.add_space(4.0);
    ui.separator();

    ui.add_space(2.0);

    if compact {
        Grid::new(Id::new("message_search_compact_grid").with(channel.as_str()))
            .num_columns(2)
            .min_col_width(SEARCH_LABEL_WIDTH)
            .spacing([10.0, 8.0])
            .striped(false)
            .show(ui, |ui| {
                search_field_row(
                    ui,
                    channel,
                    "Username",
                    "mod, chatter...",
                    "username",
                    &mut search.username,
                    false,
                    false,
                );
                search_field_row(
                    ui,
                    channel,
                    "Keyword",
                    "giveaway, timeout...",
                    "keyword",
                    &mut search.keyword,
                    true,
                    false,
                );
                search_field_row(
                    ui,
                    channel,
                    "Regex",
                    r"^!|https?://",
                    "regex",
                    &mut search.regex,
                    false,
                    true,
                );
            });
    } else {
        ui.columns(3, |cols| {
            cols[0].set_min_width(SEARCH_FIELD_MIN_WIDTH);
            cols[1].set_min_width(SEARCH_FIELD_MIN_WIDTH);
            cols[2].set_min_width(SEARCH_FIELD_MIN_WIDTH);
            search_field(
                &mut cols[0],
                "Username",
                "mod, chatter...",
                Id::new("message_search_username").with(channel.as_str()),
                &mut search.username,
                false,
                false,
            );
            search_field(
                &mut cols[1],
                "Keyword",
                "giveaway, timeout...",
                Id::new("message_search_keyword").with(channel.as_str()),
                &mut search.keyword,
                true,
                false,
            );
            search_field(
                &mut cols[2],
                "Regex",
                r"^!|https?://",
                Id::new("message_search_regex").with(channel.as_str()),
                &mut search.regex,
                false,
                true,
            );
        });
    }

    ui.add_space(2.0);
    if let Some(err) = search.regex_error() {
        ui.label(
            RichText::new(format!("Regex error: {err}"))
                .font(t::small())
                .color(Color32::from_rgb(220, 120, 120)),
        );
    } else {
        ui.label(
            RichText::new(
                "All filters are combined. Username and keyword ignore case. Press Esc to close.",
            )
            .font(t::small())
            .color(t::text_muted()),
        );
    }
}

fn stat_pill(ui: &mut Ui, text: String) {
    Frame::new()
        .fill(t::bg_surface())
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .stroke(Stroke::new(1.0, t::border_accent()))
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
            response.request_focus();
        }
    });
}

fn search_field_row(
    ui: &mut Ui,
    channel: &ChannelId,
    label: &str,
    hint: &str,
    key: &str,
    value: &mut String,
    request_focus: bool,
    code_editor: bool,
) {
    ui.label(
        RichText::new(label)
            .font(t::small())
            .color(t::text_secondary()),
    );
    let id = Id::new("message_search_row")
        .with(channel.as_str())
        .with(key);
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
        response.request_focus();
    }
    ui.end_row();
}
