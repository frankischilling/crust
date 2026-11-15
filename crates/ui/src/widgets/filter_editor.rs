//! Pattern controls and the advanced expression editor for chat rules.

use egui::{text::LayoutJob, Color32, FontId, RichText, TextFormat, TextStyle, Ui};

use crust_core::filters::parse as parse_expression;
use crust_core::highlight::{HighlightRule, HighlightRuleMode};
use crust_core::model::filters::{FilterMode, FilterRecord};

use crate::theme as t;

/// Tri-state toggle for the `mode` field (shared across filter + highlight).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorMode {
    Substring,
    Regex,
    Expression,
}

impl EditorMode {
    pub fn from_filter(m: &FilterMode) -> Self {
        match m {
            FilterMode::Substring => EditorMode::Substring,
            FilterMode::Regex => EditorMode::Regex,
            FilterMode::Expression => EditorMode::Expression,
        }
    }
    pub fn from_highlight(m: &HighlightRuleMode) -> Self {
        match m {
            HighlightRuleMode::Substring => EditorMode::Substring,
            HighlightRuleMode::Regex => EditorMode::Regex,
            HighlightRuleMode::Expression => EditorMode::Expression,
        }
    }
    pub fn to_filter(self) -> FilterMode {
        match self {
            EditorMode::Substring => FilterMode::Substring,
            EditorMode::Regex => FilterMode::Regex,
            EditorMode::Expression => FilterMode::Expression,
        }
    }
    pub fn to_highlight(self) -> HighlightRuleMode {
        match self {
            EditorMode::Substring => HighlightRuleMode::Substring,
            EditorMode::Regex => HighlightRuleMode::Regex,
            EditorMode::Expression => HighlightRuleMode::Expression,
        }
    }
}

/// Owner of the rule being edited - so the modal knows which list to write back to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterEditorOwner {
    HighlightRule(usize),
    FilterRecord(usize),
}

/// Holds live state for the advanced expression editor modal.
#[derive(Clone, Debug, Default)]
pub struct FilterEditorModal {
    pub open: bool,
    pub owner: Option<FilterEditorOwner>,
    pub draft: String,
    pub title: String,
    focus_requested: bool,
}

impl FilterEditorModal {
    pub fn open_highlight(&mut self, idx: usize, initial: &str) {
        self.open = true;
        self.focus_requested = true;
        self.owner = Some(FilterEditorOwner::HighlightRule(idx));
        self.draft = initial.to_owned();
        self.title = format!("Highlight rule #{}", idx + 1);
    }

    pub fn open_filter(&mut self, idx: usize, initial: &str, name: &str) {
        self.open = true;
        self.focus_requested = true;
        self.owner = Some(FilterEditorOwner::FilterRecord(idx));
        self.draft = initial.to_owned();
        self.title = if name.is_empty() {
            format!("Filter #{}", idx + 1)
        } else {
            format!("Filter: {}", name)
        };
    }

    pub fn close(&mut self) {
        self.open = false;
        self.focus_requested = false;
        self.owner = None;
        self.draft.clear();
    }

    /// Render the modal if open. Writes back to the underlying rule/filter on
    /// Save; returns `true` if anything was mutated.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        highlight_rules: &mut [HighlightRule],
        highlight_bufs: &mut [String],
        filter_records: &mut [FilterRecord],
        filter_bufs: &mut [String],
    ) -> bool {
        if !self.open {
            return false;
        }

        let mut save = false;
        let mut cancel =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut open = self.open;
        let bounds = ctx.screen_rect().shrink(8.0);
        let style = ctx.style();
        let frame = egui::Frame::window(&style);
        let heading = egui::TextStyle::Heading.resolve(&style);
        let title_height = ctx
            .fonts(|fonts| fonts.row_height(&heading))
            .max(style.spacing.interact_size.y)
            + frame.inner_margin.sum().y
            + frame.stroke.width;
        let max_size = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
            .max(egui::Vec2::splat(1.0));
        let default_size = egui::vec2(620.0, 460.0).min(max_size);
        let request_focus = self.focus_requested;
        if request_focus {
            let id = expression_input_id();
            let mut input_state = egui::text_edit::TextEditState::default();
            input_state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(self.draft.chars().count()),
                )));
            egui::TextEdit::store_state(ctx, id, input_state);
            self.focus_requested = false;
        }
        egui::Window::new("Expression editor")
            .id(egui::Id::new("filter_editor_modal"))
            .open(&mut open)
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(true)
            .frame(frame)
            .constrain_to(bounds)
            .min_size(egui::vec2(260.0, 200.0).min(max_size))
            .max_size(max_size)
            .default_size(default_size)
            .default_pos((bounds.center() - default_size * 0.5).max(bounds.min))
            .show(ctx, |ui| {
                let content_height = (ui.available_height() - 40.0).max(1.0);
                let width = ui.available_width().max(1.0);
                egui::ScrollArea::both()
                    .id_salt("filter_editor_content")
                    .max_width(width)
                    .max_height(content_height)
                    .min_scrolled_width(0.0)
                    .min_scrolled_height(0.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(width);
                        ui.label(RichText::new(&self.title).font(t::small()).strong());
                        let before_edit = parse_expression(&self.draft);
                        let editor = render_editor_column(ui, &mut self.draft, &before_edit);
                        if request_focus {
                            editor.request_focus();
                        }
                        if let Err(error) = parse_expression(&self.draft) {
                            ui.label(
                                RichText::new(error.to_string())
                                    .font(t::tiny())
                                    .color(t::red()),
                            );
                        }
                        egui::CollapsingHeader::new("Operators and fields")
                            .id_salt("filter_editor_palette")
                            .show(ui, |ui| render_palette_column(ui, &mut self.draft));
                    });
                ui.separator();
                let current_parse = parse_expression(&self.draft);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(current_parse.is_ok(), egui::Button::new("Save"))
                        .on_disabled_hover_text("Fix the expression error before saving")
                        .clicked()
                    {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    match &current_parse {
                        Ok(_) => {
                            ui.label(
                                RichText::new("Valid expression")
                                    .font(t::tiny())
                                    .color(t::green()),
                            );
                        }
                        Err(error) => {
                            ui.label(
                                RichText::new("Invalid expression")
                                    .font(t::tiny())
                                    .color(t::red()),
                            )
                            .on_hover_text(error.to_string());
                        }
                    }
                });
            });
        ctx.move_to_top(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("filter_editor_modal"),
        ));
        cancel |= !open;

        let mut mutated = false;
        if save && !cancel {
            match self.owner {
                Some(FilterEditorOwner::HighlightRule(i)) => {
                    if let Some(rule) = highlight_rules.get_mut(i) {
                        rule.pattern = self.draft.clone();
                        rule.mode = HighlightRuleMode::Expression;
                        rule.is_regex = false;
                        mutated = true;
                    }
                    if let Some(buf) = highlight_bufs.get_mut(i) {
                        *buf = self.draft.clone();
                    }
                }
                Some(FilterEditorOwner::FilterRecord(i)) => {
                    if let Some(rec) = filter_records.get_mut(i) {
                        rec.pattern = self.draft.clone();
                        rec.mode = FilterMode::Expression;
                        rec.is_regex = false;
                        mutated = true;
                    }
                    if let Some(buf) = filter_bufs.get_mut(i) {
                        *buf = self.draft.clone();
                    }
                }
                None => {}
            }
            self.close();
        }
        if cancel {
            self.close();
        }
        mutated
    }
}

fn render_editor_column(
    ui: &mut Ui,
    draft: &mut String,
    parse_res: &Result<crust_core::filters::Expression, crust_core::filters::ParseError>,
) -> egui::Response {
    let w = ui.available_width().max(1.0);
    ui.set_max_width(w);
    ui.label(
        RichText::new("Expression")
            .font(t::tiny())
            .strong()
            .color(t::text_muted()),
    );
    let mut layouter = build_layouter(parse_res);
    // Use the column width, not +∞: infinity lets the TextEdit's galley
    // end up 1-2px wider than the `Resize` inner rect, and egui then does
    // `desired_size = max(desired, last_content_size)` every frame (Window
    // `Resize` with with_stroke=false), which grows the window until full-screen.
    let te = egui::TextEdit::multiline(draft)
        .id(expression_input_id())
        .desired_rows(8)
        .desired_width(w)
        .font(TextStyle::Monospace)
        .hint_text("e.g. author.subscriber && message.content contains \"gg\"")
        .layouter(&mut layouter);
    ui.add(te)
}

fn render_palette_column(ui: &mut Ui, draft: &mut String) {
    let col_w = ui.available_width().max(1.0);
    ui.set_max_width(col_w);
    ui.label(
        RichText::new("Operators")
            .font(t::tiny())
            .strong()
            .color(t::text_muted()),
    );
    ui.horizontal_wrapped(|ui| {
        for op in [
            "&&",
            "||",
            "!",
            "==",
            "!=",
            "<",
            "<=",
            ">",
            ">=",
            "contains",
            "startswith",
            "endswith",
            "match",
            "(",
            ")",
            "{",
            "}",
            ",",
            "r\"...\"",
            "ri\"...\"",
        ] {
            if chip_button(ui, op).clicked() {
                insert_at_cursor(ui.ctx(), draft, op);
            }
        }
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new("Identifiers")
            .font(t::tiny())
            .strong()
            .color(t::text_muted()),
    );

    for (group, ids) in identifier_palette() {
        ui.add_space(4.0);
        ui.label(RichText::new(group).font(t::tiny()).color(t::text_muted()));
        ui.horizontal_wrapped(|ui| {
            for id in ids {
                if chip_button(ui, id).clicked() {
                    insert_at_cursor(ui.ctx(), draft, id);
                }
            }
        });
    }
}

/// Build a syntax-highlight layouter that underlines the error span (if any).
fn build_layouter<'a>(
    parse_res: &'a Result<crust_core::filters::Expression, crust_core::filters::ParseError>,
) -> impl FnMut(&Ui, &str, f32) -> std::sync::Arc<egui::Galley> + 'a {
    move |ui: &Ui, text: &str, wrap_width: f32| {
        let mut job = LayoutJob::default();
        let font_id = FontId::monospace(13.0);
        let (err_start, err_end) = match parse_res {
            Err(e) => {
                let sp = e.span();
                (sp.start, sp.end.max(sp.start + 1))
            }
            Ok(_) => (0usize, 0usize),
        };
        let body = text;
        let byte_len = body.len();
        let mut start = err_start.min(byte_len);
        let mut end = err_end.min(byte_len);
        // TextEdit can ask for a layout after the text changed within this frame.
        // The previous parse span must remain safe for the new UTF-8 text.
        while start > 0 && !body.is_char_boundary(start) {
            start -= 1;
        }
        while end < byte_len && !body.is_char_boundary(end) {
            end += 1;
        }
        if end > start && start < byte_len {
            append(&mut job, &body[..start], &font_id, t::text_primary(), None);
            append(
                &mut job,
                &body[start..end],
                &font_id,
                t::text_primary(),
                Some(egui::Stroke::new(1.5_f32, t::red())),
            );
            if end < byte_len {
                append(&mut job, &body[end..], &font_id, t::text_primary(), None);
            }
        } else {
            append(&mut job, body, &font_id, t::text_primary(), None);
        }
        job.wrap.max_width = wrap_width;
        ui.fonts(|f| f.layout_job(job))
    }
}

fn append(
    job: &mut LayoutJob,
    text: &str,
    font: &FontId,
    color: Color32,
    underline: Option<egui::Stroke>,
) {
    let mut fmt = TextFormat::simple(font.clone(), color);
    if let Some(s) = underline {
        fmt.underline = s;
    }
    job.append(text, 0.0, fmt);
}

fn chip_button(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .font(t::tiny())
                .color(t::text_primary()),
        )
        .min_size(egui::vec2(36.0, 20.0)),
    )
    .on_hover_text(format!("Insert `{label}` at cursor"))
}

fn expression_input_id() -> egui::Id {
    egui::Id::new("filter_expression_input")
}

fn insert_at_cursor(ctx: &egui::Context, draft: &mut String, chunk: &str) {
    use egui::text::{CCursor, CCursorRange};
    let id = expression_input_id();
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let cursor = state
        .cursor
        .char_range()
        .unwrap_or_else(|| CCursorRange::one(CCursor::new(draft.chars().count())));
    let start_char = cursor
        .primary
        .index
        .min(cursor.secondary.index)
        .min(draft.chars().count());
    let end_char = cursor.primary.index.max(cursor.secondary.index);
    let byte_index = |index| {
        draft
            .char_indices()
            .nth(index)
            .map_or(draft.len(), |(byte, _)| byte)
    };
    let start = byte_index(start_char);
    let end = byte_index(end_char);
    let prefix = draft[..start]
        .chars()
        .next_back()
        .is_some_and(|ch| !ch.is_whitespace());
    let suffix = draft[end..]
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_whitespace());
    let inserted = format!(
        "{}{}{}",
        if prefix { " " } else { "" },
        chunk,
        if suffix { " " } else { "" }
    );
    draft.replace_range(start..end, &inserted);
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(
            start_char + inserted.chars().count(),
        ))));
    egui::TextEdit::store_state(ctx, id, state);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

fn identifier_palette() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        (
            "author",
            &[
                "author.name",
                "author.login",
                "author.user_id",
                "author.subbed",
                "author.subscriber",
                "author.sub_length",
                "author.badges",
                "author.color",
                "author.no_color",
            ],
        ),
        (
            "channel",
            &["channel.name", "channel.live", "channel.watching"],
        ),
        ("message", &["message.content", "message.length"]),
        (
            "flags",
            &[
                "flags.action",
                "flags.announcement",
                "flags.highlighted",
                "flags.first_message",
                "flags.sub_message",
                "flags.system_message",
                "flags.reward_message",
                "flags.reply",
                "flags.whisper",
                "flags.pinned",
                "flags.mention",
                "flags.self",
                "flags.history",
            ],
        ),
        ("has", &["has.link", "has.emote", "has.mention"]),
        ("reward", &["reward.title", "reward.cost", "reward.id"]),
    ]
}

/// Actions requested by the inline pattern editor.
#[derive(Default)]
pub struct PatternCellOutcome {
    pub open_modal: bool,
}

/// Edit a rule's match mode and pattern without requiring a table layout.
pub fn render_pattern_editor(
    ui: &mut Ui,
    mode: &mut EditorMode,
    buf: &mut String,
    pattern_dest: &mut String,
    hint: &str,
    enabled: bool,
) -> PatternCellOutcome {
    let mut outcome = PatternCellOutcome::default();
    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("rule_match_mode")
            .selected_text(match mode {
                EditorMode::Substring => "Text",
                EditorMode::Regex => "Regex",
                EditorMode::Expression => "Expression",
            })
            .width(110.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(mode, EditorMode::Substring, "Text");
                ui.selectable_value(mode, EditorMode::Regex, "Regex");
                ui.selectable_value(mode, EditorMode::Expression, "Expression");
            });
        if ui.button("Advanced editor").clicked() {
            outcome.open_modal = true;
        }
    });
    let input = egui::TextEdit::singleline(buf)
        .id_salt("rule_pattern")
        .desired_width(f32::INFINITY)
        .hint_text(hint)
        .text_color(if enabled {
            t::text_primary()
        } else {
            t::text_muted()
        });
    let response = if matches!(mode, EditorMode::Expression) {
        ui.add(input.font(TextStyle::Monospace))
    } else {
        ui.add(input)
    };
    if response.changed() {
        *pattern_dest = buf.clone();
    }
    let error = match mode {
        EditorMode::Expression => parse_expression(buf).err().map(|error| error.to_string()),
        EditorMode::Regex => regex::Regex::new(buf).err().map(|error| error.to_string()),
        EditorMode::Substring => None,
    };
    if let Some(error) = error {
        ui.label(RichText::new(error).font(t::tiny()).color(t::red()));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EditorHarness {
        ctx: egui::Context,
        editor: FilterEditorModal,
        rules: Vec<HighlightRule>,
        buffers: Vec<String>,
        time: f64,
    }

    impl EditorHarness {
        fn new(draft: &str) -> Self {
            let mut editor = FilterEditorModal::default();
            editor.open_highlight(0, draft);
            Self {
                ctx: egui::Context::default(),
                editor,
                rules: vec![HighlightRule::new("original")],
                buffers: vec!["original".to_owned()],
                time: 0.0,
            }
        }

        fn frame(&mut self, size: egui::Vec2, events: Vec<egui::Event>) -> egui::FullOutput {
            self.time += 0.1;
            self.ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    time: Some(self.time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    self.editor
                        .show(ctx, &mut self.rules, &mut self.buffers, &mut [], &mut []);
                },
            )
        }

        fn click(&mut self, size: egui::Vec2, pos: egui::Pos2) {
            for pressed in [true, false] {
                self.frame(
                    size,
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
            }
        }
    }

    fn text_rect(output: &egui::FullOutput, wanted: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            let egui::epaint::Shape::Text(text) = &shape.shape else {
                return None;
            };
            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
            (text.galley.text() == wanted && shape.clip_rect.contains_rect(rect)).then_some(rect)
        })
    }

    fn replace_text(text: &str) -> Vec<egui::Event> {
        let select_all = egui::Modifiers {
            ctrl: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        vec![
            egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: select_all,
            },
            egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: select_all,
            },
            egui::Event::Text(text.to_owned()),
        ]
    }

    #[test]
    fn expression_editor_keeps_save_cancel_and_close_inside_small_viewports() {
        let mut harness = EditorHarness::new("author.subscriber");
        let mut output = egui::FullOutput::default();
        for size in [
            egui::vec2(900.0, 700.0),
            egui::vec2(320.0, 300.0),
            egui::vec2(320.0, 220.0),
        ] {
            for _ in 0..4 {
                output = harness.frame(size, Vec::new());
            }
            let window = harness
                .ctx
                .memory(|memory| memory.area_rect(egui::Id::new("filter_editor_modal")))
                .unwrap();
            assert!(
                egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(window),
                "{window:?} exceeds {size:?}"
            );
            assert!(text_rect(&output, "Save").is_some());
            assert!(text_rect(&output, "Cancel").is_some());
            assert!(text_rect(&output, "Expression editor").is_some());
        }
        harness.click(
            egui::vec2(320.0, 220.0),
            text_rect(&output, "Save").unwrap().center(),
        );
        assert!(!harness.editor.open);
        assert_eq!(harness.rules[0].pattern, "author.subscriber");
        assert_eq!(harness.rules[0].mode, HighlightRuleMode::Expression);
        assert_eq!(harness.buffers[0], harness.rules[0].pattern);
    }

    #[test]
    fn expression_validation_uses_the_current_edit_and_escape_discards_it() {
        let mut harness = EditorHarness::new("author.subscriber &&");
        let size = egui::vec2(620.0, 500.0);
        for _ in 0..4 {
            harness.frame(size, Vec::new());
        }
        let output = harness.frame(size, replace_text("author.subscriber"));
        assert_eq!(harness.editor.draft, "author.subscriber");
        assert!(text_rect(&output, "Valid expression").is_some());
        let output = harness.frame(size, replace_text("author.subscriber &&"));
        assert!(text_rect(&output, "Invalid expression").is_some());
        harness.click(size, text_rect(&output, "Save").unwrap().center());
        assert!(harness.editor.open, "an invalid expression cannot be saved");
        harness.frame(
            size,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!harness.editor.open);
        assert_eq!(harness.rules[0].pattern, "original");
        assert_eq!(harness.buffers[0], "original");
    }

    #[test]
    fn expression_palette_replaces_the_selection_at_unicode_character_boundaries() {
        let mut harness = EditorHarness::new("é && has.link");
        harness.frame(egui::vec2(700.0, 550.0), Vec::new());
        let mut state = egui::TextEdit::load_state(&harness.ctx, expression_input_id()).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(1),
            )));
        egui::TextEdit::store_state(&harness.ctx, expression_input_id(), state);
        insert_at_cursor(&harness.ctx, &mut harness.editor.draft, "author.subscriber");
        assert_eq!(harness.editor.draft, "author.subscriber && has.link");
        let state = egui::TextEdit::load_state(&harness.ctx, expression_input_id()).unwrap();
        assert_eq!(
            state.cursor.char_range().unwrap().primary.index,
            "author.subscriber".len()
        );
        assert!(harness
            .ctx
            .memory(|memory| memory.has_focus(expression_input_id())));
    }

    #[test]
    fn stale_parse_spans_cannot_split_new_unicode_text() {
        let parsed = parse_expression("author.name ==");
        assert!(parsed.is_err());
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut layouter = build_layouter(&parsed);
                for text in [
                    "🦀".repeat(64),
                    "é".repeat(32),
                    "x".to_owned(),
                    String::new(),
                ] {
                    assert_eq!(layouter(ui, &text, 240.0).text(), text);
                }
            });
        });
    }
}
