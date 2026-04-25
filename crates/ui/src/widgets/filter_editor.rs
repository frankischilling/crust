//! Shared filter/highlight editor UI.
//!
//! Renders the inline per-row mode cycler + pattern editor + parse-error
//! indicator, and hosts the "Advanced expression editor" modal that's opened
//! from both the filter-records table and the highlight-rules table.
//!
//! The module exposes two main APIs:
//!
//! - [`render_pattern_cell`] - drop-in row helper that renders three
//!   grid cells of equal height: a mode cycler (`Aa` / `.*` / `ƒx`), a
//!   single-line pattern `TextEdit` (with a red `⚠` + tooltip on parse
//!   errors for expression mode), and an `Edit...` button that opens the
//!   modal.
//! - [`FilterEditorModal`] - the modal window state. Hold one per settings
//!   page (or app) and render it once per frame with
//!   [`FilterEditorModal::show`].

use egui::{
    text::LayoutJob, Align, Color32, FontId, Layout, RichText, TextFormat, TextStyle, Ui,
};

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

    pub fn cycle(self) -> Self {
        match self {
            EditorMode::Substring => EditorMode::Regex,
            EditorMode::Regex => EditorMode::Expression,
            EditorMode::Expression => EditorMode::Substring,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            EditorMode::Substring => "Aa",
            EditorMode::Regex => ".*",
            EditorMode::Expression => "ƒx",
        }
    }
    pub fn tooltip(self) -> &'static str {
        match self {
            EditorMode::Substring => {
                "Match mode: plain substring (case-insensitive).\nClick to cycle: Aa -> .* -> ƒx"
            }
            EditorMode::Regex => {
                "Match mode: regular expression.\nClick to cycle: .* -> ƒx -> Aa"
            }
            EditorMode::Expression => {
                "Match mode: filter DSL expression.\nClick to cycle: ƒx -> Aa -> .*"
            }
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
}

impl FilterEditorModal {
    pub fn open_highlight(&mut self, idx: usize, initial: &str) {
        self.open = true;
        self.owner = Some(FilterEditorOwner::HighlightRule(idx));
        self.draft = initial.to_owned();
        self.title = format!("Highlight rule #{}", idx + 1);
    }

    pub fn open_filter(&mut self, idx: usize, initial: &str, name: &str) {
        self.open = true;
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
        let mut cancel = false;
        let parse_res = parse_expression(&self.draft);

        let screen = ctx.screen_rect();
        let default_w = 560.0_f32.min((screen.width() - 32.0).max(360.0));
        let default_h = 380.0_f32.min((screen.height() - 32.0).max(280.0));
        let default_pos = egui::pos2(
            (screen.center().x - default_w * 0.5).max(8.0),
            (screen.center().y - default_h * 0.5).max(8.0),
        );

        // NOTE: `egui::Window`'s id is derived from the title string by
        // default, and our title changes per rule (`Highlight rule #3`
        // etc.). That wipes any resize/position state across opens, so we
        // pin a stable id and only use the title as the visible label.
        egui::Window::new(format!("Advanced expression editor - {}", self.title))
            .id(egui::Id::new("filter_editor_modal"))
            .collapsible(false)
            .resizable(true)
            .default_width(default_w)
            .default_height(default_h)
            .default_pos(default_pos)
            .min_width(360.0)
            .min_height(240.0)
            .show(ctx, |ui| {
                // Header description.
                ui.label(
                    RichText::new("Chatterino-style filter expression")
                        .font(t::small())
                        .strong()
                        .color(t::text_primary()),
                );
                ui.label(
                    RichText::new(
                        "Expressions return a boolean; identifiers like `author.subscriber` or \
                         `message.content` refer to the current message.",
                    )
                    .font(t::tiny())
                    .color(t::text_muted()),
                );
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                // Reserve space at the bottom for the status line + buttons. Use
                // `allocate_ui_with_layout` for a bounded main region; inner
                // widgets must not report min width/height over that rect, or
                // egui's `Resize` state grows the window (see notes on
                // `TextEdit` width and palette `ScrollArea` below).
                let bottom_reserved = 56.0_f32;
                let content_avail = (ui.available_height() - bottom_reserved).max(160.0);
                let content_width = ui.available_width();

                let side_by_side = content_width >= 520.0;

                ui.allocate_ui_with_layout(
                    egui::vec2(content_width, content_avail),
                    Layout::top_down(Align::Min),
                    |ui| {
                        if side_by_side {
                            let palette_ratio = 0.38_f32;
                            let total_w = ui.available_width();
                            let item_x = ui.spacing().item_spacing.x;
                            // `horizontal` inserts item_spacing between each child. Editor + separator +
                            // palette would exceed `total_w` if we only subtract 12 for the bar.
                            let between_extra = 12.0_f32 + 2.0 * item_x;
                            let palette_w = (total_w * palette_ratio).clamp(180.0, 240.0);
                            let editor_w = (total_w - palette_w - between_extra).max(200.0);

                            ui.horizontal_top(|ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(editor_w, content_avail),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        render_editor_column(ui, &mut self.draft, &parse_res);
                                    },
                                );
                                ui.separator();
                                ui.allocate_ui_with_layout(
                                    egui::vec2(palette_w, content_avail),
                                    Layout::top_down(Align::Min),
                                    |ui| {
                                        render_palette_column(ui, &mut self.draft);
                                    },
                                );
                            });
                        } else {
                            render_editor_column(ui, &mut self.draft, &parse_res);
                            ui.add_space(6.0);
                            ui.separator();
                            ui.add_space(2.0);
                            render_palette_column(ui, &mut self.draft);
                        }
                    },
                );

                ui.add_space(4.0);
                ui.separator();
                ui.horizontal(|ui| {
                    // Status line takes the left; buttons hug the right edge.
                    match &parse_res {
                        Ok(_) => {
                            ui.label(
                                RichText::new(" expression parses")
                                    .font(t::tiny())
                                    .color(t::green()),
                            );
                        }
                        Err(e) => {
                            let sp = e.span();
                            ui.label(
                                RichText::new(format!(
                                    " Parse error at line {} col {}: {}",
                                    sp.line, sp.col, e
                                ))
                                .font(t::tiny())
                                .color(t::red()),
                            );
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Cancel").color(t::text_primary()))
                                    .min_size(egui::vec2(72.0, 24.0)),
                            )
                            .clicked()
                        {
                            cancel = true;
                        }
                        let can_save = parse_res.is_ok();
                        let resp = ui
                            .add_enabled(
                                can_save,
                                egui::Button::new(
                                    RichText::new("Save").color(t::text_primary()).strong(),
                                )
                                .min_size(egui::vec2(72.0, 24.0)),
                            )
                            .on_disabled_hover_text("Fix the parse error to save");
                        if resp.clicked() {
                            save = true;
                        }
                    });
                });
            });

        let mut mutated = false;
        if save {
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
) {
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
        .desired_rows(8)
        .desired_width(w)
        .font(TextStyle::Monospace)
        .hint_text("e.g. author.subscriber && message.content contains \"gg\"")
        .layouter(&mut layouter);
    ui.add(te);
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
            "&&", "||", "!", "==", "!=", "<", "<=", ">", ">=", "contains", "startswith",
            "endswith", "match", "(", ")", "{", "}", ",", "r\"...\"", "ri\"...\"",
        ] {
            if chip_button(ui, op).clicked() {
                insert_at_end(draft, op);
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
    // Vertical-only: with auto_shrink×=[false,false], width was
    // `inner.max(content)` (no horizontal scroll), so wrapped chips nudged
    // the reported width past the `Resize` inner rect and re-triggered
    // `desired_size` growth every frame. Keep height filling; cap width.
    egui::ScrollArea::vertical()
        .id_salt("filter_editor_palette_scroll")
        .max_width(col_w)
        .auto_shrink([true, false])
        .show(ui, |ui| {
            for (group, ids) in identifier_palette() {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(group)
                        .font(t::tiny())
                        .color(t::text_muted()),
                );
                ui.horizontal_wrapped(|ui| {
                    for id in ids {
                        if chip_button(ui, id).clicked() {
                            insert_at_end(draft, id);
                        }
                    }
                });
            }
        });
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
        if err_end > err_start && err_start < byte_len {
            let end_clamped = err_end.min(byte_len);
            append(&mut job, &body[..err_start], &font_id, t::text_primary(), None);
            append(
                &mut job,
                &body[err_start..end_clamped],
                &font_id,
                t::text_primary(),
                Some(egui::Stroke::new(1.5, t::red())),
            );
            if end_clamped < byte_len {
                append(&mut job, &body[end_clamped..], &font_id, t::text_primary(), None);
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
            RichText::new(label).font(t::tiny()).color(t::text_primary()),
        )
        .min_size(egui::vec2(36.0, 20.0)),
    )
    .on_hover_text(format!("Insert `{label}` at cursor"))
}

fn insert_at_end(buf: &mut String, chunk: &str) {
    if !buf.is_empty() && !buf.ends_with(char::is_whitespace) {
        buf.push(' ');
    }
    buf.push_str(chunk);
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

// Inline per-row helper

/// Uniform button height for every row button in the settings filter/highlight
/// tables. Having a shared constant keeps the Grid-managed row height constant
/// across cells regardless of mode.
pub const ROW_BTN_HEIGHT: f32 = 22.0;
/// Uniform mode cycler button size.
pub const MODE_BTN_SIZE: egui::Vec2 = egui::vec2(30.0, ROW_BTN_HEIGHT);
/// Uniform "Edit..." button size.
pub const EDIT_BTN_SIZE: egui::Vec2 = egui::vec2(44.0, ROW_BTN_HEIGHT);

/// Width of the inline pattern `TextEdit`. Kept wide even in compact mode so
/// expression-mode patterns get enough room; users can still expand by
/// clicking `Edit...`.
pub fn pattern_width(compact: bool) -> f32 {
    if compact {
        180.0
    } else {
        260.0
    }
}

/// Action returned by [`render_pattern_cell`] so the caller can react to
/// inline button presses without threading mutable state around.
#[derive(Default)]
pub struct PatternCellOutcome {
    /// User clicked the `Edit...` button for this row.
    pub open_modal: bool,
}

/// Render the inline pattern cell for a filter/highlight row.
///
/// Emits exactly **three** Grid cells, all at `ROW_BTN_HEIGHT` height, so the
/// enclosing Grid's row doesn't jump around when the user cycles modes:
///
/// 1. **Mode cycler** - compact button (`Aa` / `.*` / `ƒx`)
/// 2. **Pattern editor** - `TextEdit::singleline` in every mode; when mode is
///    expression and the text doesn't parse, a red `⚠` icon with a tooltip
///    carrying the parse error message is rendered inside the same cell,
///    to the right of the text input.
/// 3. **Edit...** - opens the advanced modal editor. Always rendered (also for
///    legacy modes), so clicking it from a substring/regex rule promotes it
///    to an expression via the modal.
pub fn render_pattern_cell(
    ui: &mut Ui,
    mode: &mut EditorMode,
    buf: &mut String,
    pattern_dest: &mut String,
    hint: &str,
    enabled: bool,
    compact: bool,
) -> PatternCellOutcome {
    let mut outcome = PatternCellOutcome::default();

    // Cell 1: mode cycler
    let (glyph_color, tooltip) = (
        match *mode {
            EditorMode::Substring => t::text_secondary(),
            EditorMode::Regex => t::link(),
            EditorMode::Expression => t::yellow(),
        },
        mode.tooltip(),
    );
    if ui
        .add(
            egui::Button::new(
                RichText::new(mode.glyph())
                    .font(t::tiny())
                    .color(glyph_color)
                    .strong(),
            )
            .min_size(MODE_BTN_SIZE),
        )
        .on_hover_text(tooltip)
        .clicked()
    {
        *mode = mode.cycle();
    }

    // Cell 2: inline pattern editor (single-line) + optional parse-error ⚠.
    //
    // We keep this single-line in every mode so the Grid row height stays
    // constant and the adjacent button columns don't jitter. Users who need a
    // multi-line view click `Edit...`.
    let field_w = pattern_width(compact);
    let text_color = if enabled {
        t::text_primary()
    } else {
        t::text_muted()
    };
    let expr_mode = matches!(mode, EditorMode::Expression);
    let parse_err = if expr_mode && !buf.trim().is_empty() {
        parse_expression(buf).err()
    } else {
        None
    };

    ui.horizontal(|ui| {
        let input_w = if parse_err.is_some() {
            field_w - 22.0
        } else {
            field_w
        };
        let te = if expr_mode {
            egui::TextEdit::singleline(buf)
                .desired_width(input_w)
                .min_size(egui::vec2(input_w, ROW_BTN_HEIGHT))
                .font(TextStyle::Monospace)
                .hint_text(hint)
                .text_color(text_color)
        } else {
            egui::TextEdit::singleline(buf)
                .desired_width(input_w)
                .min_size(egui::vec2(input_w, ROW_BTN_HEIGHT))
                .hint_text(hint)
                .text_color(text_color)
        };
        let resp = ui.add(te);
        if resp.changed() {
            *pattern_dest = buf.clone();
        }

        if let Some(err) = parse_err {
            let sp = err.span();
            let tip = format!(
                "Parse error at line {} col {} (byte {}..{}):\n{}\n\nClick Edit... for the full editor.",
                sp.line, sp.col, sp.start, sp.end, err
            );
            ui.add(
                egui::Label::new(
                    RichText::new("⚠")
                        .font(t::tiny())
                        .color(t::red())
                        .strong(),
                )
                .sense(egui::Sense::hover()),
            )
            .on_hover_text(tip);
        }
    });

    // Cell 3: advanced-editor button
    if ui
        .add(
            egui::Button::new(
                RichText::new("Edit...")
                    .font(t::tiny())
                    .color(t::text_secondary()),
            )
            .min_size(EDIT_BTN_SIZE),
        )
        .on_hover_text("Open advanced expression editor")
        .clicked()
    {
        outcome.open_modal = true;
    }

    outcome
}
