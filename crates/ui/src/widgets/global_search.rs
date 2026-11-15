use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crust_core::events::AppEvent;
use crust_core::search::{matches, parse, ParseOutcome, Predicate};
use crust_core::{ChannelId, ChannelState, ChatMessage, MessageId};

/// Maximum number of hits shown at once. Older hits are dropped.
pub const MAX_HITS: usize = 5000;

/// One search result row.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub channel: ChannelId,
    pub message: ChatMessage,
}

#[derive(PartialEq)]
struct SearchLayout {
    width: f32,
    pixels_per_point: f32,
    small_font: egui::FontId,
    tiny_font: egui::FontId,
    style: Arc<egui::Style>,
}

struct SearchRowHeight {
    signature: u64,
    stride: f32,
    generation: u64,
}

#[derive(Default)]
struct SearchLayoutCache {
    layout: Option<SearchLayout>,
    fonts: Option<egui::FontDefinitions>,
    rows: HashMap<egui::Id, SearchRowHeight>,
    generation: u64,
    rendered_rows: usize,
    measured_rows: usize,
    content_height: f32,
}

impl SearchLayoutCache {
    fn prepare(&mut self, ui: &egui::Ui) {
        let layout = SearchLayout {
            width: ui.available_width(),
            pixels_per_point: ui.ctx().pixels_per_point(),
            small_font: t::small(),
            tiny_font: t::tiny(),
            style: ui.style().clone(),
        };
        let fonts_changed = ui.ctx().fonts(|fonts| {
            let font_cache = fonts.lock();
            let definitions = font_cache.fonts.definitions();
            let unchanged = self.fonts.as_ref().is_some_and(|cached| {
                cached.families == definitions.families
                    && cached.font_data.len() == definitions.font_data.len()
                    && cached.font_data.iter().zip(&definitions.font_data).all(
                        |((left_name, left_font), (right_name, right_font))| {
                            left_name == right_name && Arc::ptr_eq(left_font, right_font)
                        },
                    )
            });
            if !unchanged {
                self.fonts = Some(definitions.clone());
            }
            !unchanged
        });
        if fonts_changed || self.layout.as_ref() != Some(&layout) {
            self.rows.clear();
            self.layout = Some(layout);
        }
        self.generation = self.generation.wrapping_add(1);
        self.rendered_rows = 0;
        self.measured_rows = 0;
    }
}

/// Persistent state for the global search popup.
#[derive(Default)]
pub struct GlobalSearchState {
    pub open: bool,
    pub input: String,
    pub regex_error: Option<String>,
    pub results: Vec<SearchHit>,
    pub selected_idx: usize,
    pub focus_input: bool,

    last_parsed_input: String,
    last_seen_fingerprint: Option<u64>,
    parsed: Vec<Predicate>,
    scroll_to_selected: bool,
    focused_control: Option<Id>,
    focused_result: Option<Id>,
    layout_cache: SearchLayoutCache,
    pub load_older_pending: HashMap<ChannelId, std::time::Instant>,
}

impl GlobalSearchState {
    pub fn request_open(&mut self) {
        self.open = true;
        self.focus_input = true;
        self.invalidate();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.focus_input = false;
        self.focused_control = None;
        self.focused_result = None;
    }

    pub(crate) fn owns_keyboard(&self, ctx: &Context) -> bool {
        self.open && self.focused_control.is_some_and(|id| {
            ctx.memory(|memory| {
                memory.has_focus(id)
                    || (memory.focused().is_none() && memory.had_focus_last_frame(id))
            }) && ctx.read_response(id).is_some()
        })
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.regex_error = None;
        self.results.clear();
        self.layout_cache = SearchLayoutCache::default();
        self.selected_idx = 0;
        self.last_parsed_input.clear();
        self.parsed.clear();
        self.focus_input = true;
    }

    pub fn invalidate(&mut self) {
        self.last_seen_fingerprint = None;
    }

    /// In-place message edits need a refresh even when the buffer size is unchanged.
    pub fn on_event(&mut self, event: &AppEvent) {
        if matches!(
            event,
            AppEvent::ChannelJoined { .. }
                | AppEvent::ChannelParted { .. }
                | AppEvent::ChannelRedirected { .. }
                | AppEvent::MessageReceived { .. }
                | AppEvent::MessageDeleted { .. }
                | AppEvent::EmoteCatalogUpdated { .. }
                | AppEvent::HistoryLoaded { .. }
                | AppEvent::HistoryModerationLoaded { .. }
                | AppEvent::MentionsLoaded { .. }
                | AppEvent::UserMessagesCleared { .. }
                | AppEvent::ClearUserMessagesLocally { .. }
                | AppEvent::ChannelMessagesCleared { .. }
                | AppEvent::ChannelChatCleared { .. }
                | AppEvent::SharedChannelResolved { .. }
                | AppEvent::NicknamesUpdated { .. }
                | AppEvent::SenderCosmeticsUpdated { .. }
        ) {
            self.invalidate();
        }
    }
}

/// Track channel identity and buffer rotation without scanning every message.
/// In-place changes are invalidated by [`GlobalSearchState::on_event`].
pub fn fingerprint(channels: &HashMap<ChannelId, ChannelState>) -> u64 {
    let mut fingerprint = channels.len() as u64;
    for (id, state) in channels {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hash);
        state.messages.len().hash(&mut hash);
        state
            .messages
            .front()
            .map(|message| message.id)
            .hash(&mut hash);
        state
            .messages
            .back()
            .map(|message| message.id)
            .hash(&mut hash);
        fingerprint ^= hash.finish();
    }
    fingerprint
}

/// Re-run parser + filter if either the input or the buffer fingerprint changed.
/// Returns `true` if results were rebuilt.
pub fn refresh_if_stale(
    state: &mut GlobalSearchState,
    channels: &HashMap<ChannelId, ChannelState>,
) -> bool {
    // Drop pending load-older markers whose 10s freshness window has elapsed,
    // or whose channel is no longer open.
    let now = std::time::Instant::now();
    state.load_older_pending.retain(|ch, inserted_at| {
        let fresh = now.duration_since(*inserted_at) < std::time::Duration::from_secs(10);
        fresh && channels.contains_key(ch)
    });

    let fp = fingerprint(channels);
    let input_changed = state.input != state.last_parsed_input;
    let buffer_changed = Some(fp) != state.last_seen_fingerprint;
    if !input_changed && !buffer_changed {
        return false;
    }

    if input_changed {
        let ParseOutcome {
            predicates,
            regex_error,
        } = parse(&state.input);
        state.parsed = predicates;
        state.regex_error = regex_error;
        state.last_parsed_input = state.input.clone();
    }
    let selected = if input_changed {
        None
    } else {
        state
            .results
            .get(state.selected_idx)
            .map(|hit| (hit.channel.clone(), hit.message.id))
    };
    state.last_seen_fingerprint = Some(fp);
    state.results = run_search(&state.parsed, channels);
    group_results_by_channel(&mut state.results);
    state.selected_idx = selected
        .and_then(|(channel, id)| {
            state
                .results
                .iter()
                .position(|hit| hit.channel == channel && hit.message.id == id)
        })
        .unwrap_or(0);
    state.scroll_to_selected |= input_changed;
    true
}

/// Filter every channel's buffer with the compiled predicate list.
///
/// Returns up to [`MAX_HITS`] newest-first hits. Empty `preds` returns no hits
/// (empty query = no results, not all-messages).
pub fn run_search(
    preds: &[Predicate],
    channels: &HashMap<ChannelId, ChannelState>,
) -> Vec<SearchHit> {
    if preds.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(&ChannelId, &ChatMessage)> = channels
        .iter()
        .flat_map(|(id, st)| {
            st.messages.iter().filter_map(move |m| {
                if matches(preds, m, id) {
                    Some((id, m))
                } else {
                    None
                }
            })
        })
        .collect();
    let newest_first = |a: &(&ChannelId, &ChatMessage), b: &(&ChannelId, &ChatMessage)| {
        b.1.timestamp
            .cmp(&a.1.timestamp)
            .then_with(|| a.0.as_str().cmp(b.0.as_str()))
            .then_with(|| b.1.id.0.cmp(&a.1.id.0))
    };
    if hits.len() > MAX_HITS {
        hits.select_nth_unstable_by(MAX_HITS, newest_first);
        hits.truncate(MAX_HITS);
    }
    hits.sort_unstable_by(newest_first);
    hits.into_iter()
        .map(|(channel, message)| SearchHit {
            channel: channel.clone(),
            message: message.clone(),
        })
        .collect()
}

fn group_results_by_channel(hits: &mut [SearchHit]) {
    let mut order = HashMap::new();
    for hit in hits.iter() {
        let next = order.len();
        order.entry(hit.channel.clone()).or_insert(next);
    }
    hits.sort_by_key(|hit| order[&hit.channel]);
}

use egui::{
    Align, Button, CentralPanel, Context, Frame, Id, Key, RichText, ScrollArea, Stroke, Ui,
    ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
};

use crate::image_cache::UiImageCache;
use crate::theme as t;

use super::chrome;
use super::name_paint::show_sender_name_with_sense;

const WINDOW_DEFAULT_WIDTH: f32 = 520.0;
const WINDOW_DEFAULT_HEIGHT: f32 = 380.0;
const WINDOW_MIN_WIDTH: f32 = 360.0;
const WINDOW_MIN_HEIGHT: f32 = 240.0;

/// Output of one render passthe caller applies these to app state.
#[derive(Default)]
pub struct GlobalSearchOutput {
    pub load_older_requests: Vec<ChannelId>,
    pub jump_to: Option<(ChannelId, MessageId)>,
    pub close_requested: bool,
}

/// Render the global search viewport. Caller must have already called
/// [`refresh_if_stale`] with the current channel map for this frame.
pub fn show_global_search_window(
    ctx: &Context,
    channels: &HashMap<ChannelId, ChannelState>,
    state: &mut GlobalSearchState,
    always_on_top: bool,
    images: &UiImageCache,
    animations_when_focused: bool,
) -> GlobalSearchOutput {
    let mut output = GlobalSearchOutput::default();
    let viewport_id = ViewportId::from_hash_of("global_search");
    let title = "Search all channels";
    let level = if always_on_top {
        egui::viewport::WindowLevel::AlwaysOnTop
    } else {
        egui::viewport::WindowLevel::Normal
    };
    let builder = ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([WINDOW_DEFAULT_WIDTH, WINDOW_DEFAULT_HEIGHT])
        .with_min_inner_size([WINDOW_MIN_WIDTH, WINDOW_MIN_HEIGHT])
        .with_resizable(true)
        .with_close_button(true)
        .with_active(true)
        .with_window_level(level);

    ctx.show_viewport_immediate(viewport_id, builder, |child_ctx, class| {
        let owns_escape = !matches!(class, ViewportClass::Embedded) || state.owns_keyboard(child_ctx);
        let esc = owns_escape && !child_ctx.is_context_menu_open()
            && child_ctx.input_mut(|i| crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::Escape));
        let close_requested = child_ctx.input(|i| i.viewport().close_requested());
        if esc {
            state.close();
            output.close_requested = true;
            if !matches!(class, ViewportClass::Embedded) {
                child_ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
        if close_requested {
            state.close();
            output.close_requested = true;
        }
        if !state.open {
            return;
        }
        let animate = !animations_when_focused || child_ctx.input(|input| input.focused);
        match class {
            ViewportClass::Embedded => {
                let mut open = state.open;
                egui::Window::new(title)
                    .id(Id::new("global_search_window"))
                    .default_width(WINDOW_DEFAULT_WIDTH)
                    .min_width(WINDOW_MIN_WIDTH)
                    .resizable(true)
                    .collapsible(false)
                    .constrain(true)
                    .anchor(egui::Align2::CENTER_TOP, [0.0, 48.0])
                    .frame(
                        Frame::window(&child_ctx.style())
                            .fill(t::bg_dialog())
                            .corner_radius(t::RADIUS)
                            .stroke(Stroke::new(1.0_f32, t::border_accent())),
                    )
                    .open(&mut open)
                    .show(child_ctx, |ui| {
                        render_body(ui, channels, state, &mut output, images, animate);
                    });
                state.open &= open;
            }
            _ => {
                CentralPanel::default()
                    .frame(
                        chrome::card_frame()
                            .fill(t::bg_dialog())
                            .stroke(Stroke::new(1.0_f32, t::border_subtle()))
                            .inner_margin(egui::Margin::same(12)),
                    )
                    .show(child_ctx, |ui| {
                        render_body(ui, channels, state, &mut output, images, animate);
                    });
            }
        }
    });
    output
}

fn render_body(
    ui: &mut Ui,
    channels: &HashMap<ChannelId, ChannelState>,
    state: &mut GlobalSearchState,
    output: &mut GlobalSearchOutput,
    images: &UiImageCache,
    animate: bool,
) {
    handle_keyboard(ui.ctx(), state, output);
    render_header(ui, state, output);
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    render_input(ui, state);
    if let Some(err) = &state.regex_error {
        ui.label(
            RichText::new(format!("Regex error: {err}"))
                .font(t::small())
                .color(t::red()),
        );
    }
    ui.add_space(6.0);
    render_results(ui, channels, state, output, images, animate);
    state.focused_control = ui.ctx().memory(|memory| memory.focused()).filter(|id| {
        ui.ctx().read_response(*id).is_some_and(|response| {
            response.layer_id == ui.layer_id() && ui.min_rect().contains_rect(response.rect)
        })
    });
}

fn render_header(ui: &mut Ui, state: &mut GlobalSearchState, output: &mut GlobalSearchOutput) {
    chrome::dialog_header(ui, "Search all channels", None);
    ui.add(
        egui::Label::new(
            RichText::new(
                "Predicates: from: in: has: is: regex: badge: subtier:  |  !pred negates",
            )
            .font(t::small())
            .color(t::text_muted()),
        )
        .wrap(),
    );
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(Button::new(RichText::new("Close").font(t::small())))
            .on_hover_text("Close (Esc)")
            .clicked()
        {
            state.close();
            output.close_requested = true;
        }
        if ui
            .add(Button::new(RichText::new("Clear").font(t::small())))
            .on_hover_text("Clear input and results")
            .clicked()
        {
            state.clear();
        }
        let channel_count = state
            .results
            .chunk_by(|left, right| left.channel == right.channel)
            .count();
        let pill_text = if state.results.is_empty() {
            "no hits".to_string()
        } else {
            format!("{} hits • {} channels", state.results.len(), channel_count)
        };
        stat_pill(ui, pill_text);
    });
}

fn render_input(ui: &mut Ui, state: &mut GlobalSearchState) {
    let edit = egui::TextEdit::singleline(&mut state.input)
        .id(Id::new("global_search_input"))
        .hint_text("type query, e.g. from:angel has:link regex:\"^!\"")
        .desired_width(f32::INFINITY)
        .margin(egui::Margin::symmetric(8, 6));
    let response = ui.add_sized([ui.available_width(), 30.0], edit);
    if state.focus_input {
        response.request_focus();
        if let Some(mut editor) = egui::TextEdit::load_state(ui.ctx(), response.id) {
            editor.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(state.input.chars().count()),
            )));
            egui::TextEdit::store_state(ui.ctx(), response.id, editor);
        }
        state.focus_input = false;
    }
}

fn render_results(
    ui: &mut Ui,
    channels: &HashMap<ChannelId, ChannelState>,
    state: &mut GlobalSearchState,
    output: &mut GlobalSearchOutput,
    images: &UiImageCache,
    animate: bool,
) {
    let hit_count = state.results.len();
    let scroll_to_selected = std::mem::take(&mut state.scroll_to_selected);
    let focused = ui.ctx().memory(|memory| memory.focused());
    state.focused_result = None;
    let mut cache = std::mem::take(&mut state.layout_cache);

    let scroll = ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            cache.prepare(ui);
            if state.results.is_empty() {
                cache.rows.clear();
                ui.add_space(12.0);
                ui.label(
                    RichText::new(if state.input.trim().is_empty() {
                        "Type a query to search."
                    } else {
                        "No matches."
                    })
                    .font(t::small())
                    .color(t::text_muted()),
                );
                if !state.input.trim().is_empty() {
                    let mut ordered: Vec<_> = channels.keys().collect();
                    ordered.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                    for channel in ordered {
                        render_group_header(ui, channel, 0, channels[channel].local_history_exhausted, &mut state.load_older_pending, output);
                    }
                }
                return;
            }

            let mut running_idx = 0usize;
            let origin_y = ui.next_widget_position().y;
            let mut skipped_stride = 0.0;
            for hits_in_group in state.results.chunk_by(|a, b| a.channel == b.channel) {
                skip_measured_rows(ui, skipped_stride);
                skipped_stride = 0.0;
                let channel = &hits_in_group[0].channel;
                let group_len = hits_in_group.len();
                render_group_header(
                    ui,
                    channel,
                    group_len,
                    channels.get(channel).is_some_and(|state| state.local_history_exhausted),
                    &mut state.load_older_pending,
                    output,
                );
                for hit in hits_in_group {
                    let selected = running_idx == state.selected_idx;
                    let row_id = search_hit_id(hit);
                    let signature = search_hit_signature(hit);
                    let cached_stride = cache.rows.get_mut(&row_id)
                        .filter(|row| row.signature == signature)
                        .map(|row| {
                            row.generation = cache.generation;
                            row.stride
                        });
                    let top = ui.next_widget_position().y - origin_y + skipped_stride;
                    let force_render = (selected && scroll_to_selected) || focused == Some(row_id);
                    if !force_render && cached_stride.is_some_and(|stride| {
                        top + stride < viewport.min.y || top > viewport.max.y
                    }) {
                        skipped_stride += cached_stride.unwrap_or_default();
                        running_idx += 1;
                        continue;
                    }
                    skip_measured_rows(ui, skipped_stride);
                    skipped_stride = 0.0;
                    let top = ui.next_widget_position().y;
                    let response = render_hit_row(ui, hit, selected, images, animate, |hit_ref| {
                        output.jump_to = Some((hit_ref.channel.clone(), hit_ref.message.id));
                    });
                    cache.rendered_rows += 1;
                    cache.measured_rows += usize::from(cached_stride.is_none());
                    cache.rows.insert(row_id, SearchRowHeight {
                        signature,
                        stride: ui.next_widget_position().y - top,
                        generation: cache.generation,
                    });
                    if selected && scroll_to_selected {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    if response.has_focus() {
                        state.focused_result = Some(response.id);
                        state.selected_idx = running_idx;
                    }
                    running_idx += 1;
                }
                skip_measured_rows(ui, skipped_stride);
                skipped_stride = 0.0;
                ui.add_space(4.0);
            }
        });
    cache.rows.retain(|_, row| row.generation == cache.generation);
    cache.content_height = scroll.content_size.y;
    state.layout_cache = cache;

    if hit_count > 0 && state.selected_idx >= hit_count {
        state.selected_idx = hit_count - 1;
    }
}

fn skip_measured_rows(ui: &mut Ui, stride: f32) {
    if stride > 0.0 {
        ui.allocate_space(egui::vec2(
            0.0,
            (stride - ui.spacing().item_spacing.y).max(0.0),
        ));
    }
}

fn search_hit_signature(hit: &SearchHit) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let text = hit.message.plain_text();
    let text = text.trim();
    let cutoff = text.char_indices().nth(160).map(|(index, _)| index);
    text[..cutoff.unwrap_or(text.len())].hash(&mut hash);
    cutoff.is_some().hash(&mut hash);
    hit.message.timestamp.hash(&mut hash);
    hit.message.sender.login.hash(&mut hash);
    hit.message.sender.display_name.hash(&mut hash);
    hash.finish()
}

fn render_group_header(
    ui: &mut Ui,
    channel: &ChannelId,
    hit_count: usize,
    exhausted: bool,
    load_older_pending: &mut HashMap<ChannelId, std::time::Instant>,
    output: &mut GlobalSearchOutput,
) -> egui::Response {
    let pending = load_older_pending
        .get(channel)
        .map(|ts| ts.elapsed() < std::time::Duration::from_secs(10))
        .unwrap_or(false);
    if !pending {
        load_older_pending.remove(channel);
    }
    ui.push_id(("global_search_group", channel.as_str()), |ui| {
        ui.horizontal_top(|ui| {
        let label = if exhausted { "No older logs" } else if pending { "Loading..." } else { "Load older" };
        let button_width = ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(label.to_owned(), t::tiny(), t::text_primary())
                .size()
                .x
        }) + 2.0 * ui.spacing().button_padding.x;
        let heading_width =
            (ui.available_width() - button_width - ui.spacing().item_spacing.x).max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(heading_width, 0.0),
            egui::Layout::top_down(Align::Min),
            |ui| {
                ui.set_width(heading_width);
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("#{}  ({} hits)", channel.display_name(), hit_count))
                            .font(t::small())
                            .color(t::text_secondary()),
                    )
                    .wrap(),
                );
            },
        );
        let btn = ui.add_enabled_ui(!pending && !exhausted, |ui| {
            load_older_button(ui, channel, label)
        }).inner;
        if btn.clicked() {
            load_older_pending.insert(channel.clone(), std::time::Instant::now());
            output.load_older_requests.push(channel.clone());
        }
        btn
        }).inner
    }).inner
}

fn load_older_button(ui: &mut Ui, channel: &ChannelId, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(), t::tiny(), egui::Color32::PLACEHOLDER,
    );
    let mut size = galley.size() + ui.spacing().button_padding * 2.0;
    size.y = size.y.max(ui.spacing().interact_size.y);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let response = ui.interact(
        rect, Id::new("global_search_load_older").with(channel), egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        ui.painter().rect(
            rect.expand(visuals.expansion), visuals.corner_radius,
            visuals.weak_bg_fill, visuals.bg_stroke, egui::StrokeKind::Inside,
        );
        ui.painter().galley(rect.center() - galley.size() * 0.5, galley, visuals.text_color());
    }
    response
}

fn render_hit_row(
    ui: &mut Ui,
    hit: &SearchHit,
    selected: bool,
    images: &UiImageCache,
    animate: bool,
    mut on_jump: impl FnMut(&SearchHit),
) -> egui::Response {
    let plain_text = hit.message.plain_text();
    let bg = if selected {
        t::bg_raised()
    } else {
        t::bg_surface()
    };
    let frame = Frame::new()
        .fill(bg)
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(6, 3));
    let resp = frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let content_width = ui.available_width();
            let color = hit
                .message
                .sender
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or_else(t::text_secondary);
            let display_name = if hit.message.sender.display_name.trim().is_empty() {
                &hit.message.sender.login
            } else {
                &hit.message.sender.display_name
            };
            ui.allocate_ui_with_layout(
                egui::vec2(content_width, 0.0),
                egui::Layout::left_to_right(Align::Center).with_main_wrap(true),
                |ui| {
                    ui.set_max_width(content_width);
                    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                    ui.style_mut().override_font_id = Some(t::small());
                    ui.add(
                        egui::Label::new(
                            RichText::new(hit.message.timestamp.format("%H:%M ").to_string())
                                .font(t::tiny())
                                .color(t::text_muted()),
                        )
                        .selectable(false),
                    );
                    show_sender_name_with_sense(
                        ui,
                        display_name,
                        color,
                        hit.message.sender.name_paint.as_ref(),
                        images,
                        animate,
                        egui::Sense::hover(),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                " {}",
                                truncate(plain_text.trim(), 160)
                            ))
                            .font(t::small())
                            .color(t::text_primary()),
                        )
                        .selectable(false)
                        .wrap(),
                    );
                },
            );
        })
        .response;
    let resp = ui.interact(resp.rect, search_hit_id(hit), egui::Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            format!("{}: {}", hit.message.sender.login, plain_text),
        )
    });
    if resp.clicked() {
        resp.request_focus();
        on_jump(hit);
    }
    resp
}

fn search_hit_id(hit: &SearchHit) -> Id {
    Id::new("global_search_result").with((&hit.channel, hit.message.id))
}

fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    crate::theme::parse_hex_rgb(s)
}

fn truncate(s: &str, max: usize) -> String {
    if let Some((cutoff, _)) = s.char_indices().nth(max) {
        let mut out = String::with_capacity(cutoff + 3);
        out.push_str(&s[..cutoff]);
        out.push_str("...");
        out
    } else {
        s.to_owned()
    }
}

fn handle_keyboard(ctx: &Context, state: &mut GlobalSearchState, output: &mut GlobalSearchOutput) {
    if ctx.is_context_menu_open() {
        return;
    }
    if state.owns_keyboard(ctx)
        && ctx.input_mut(|input| crate::app::consume_exact_key(input, egui::Modifiers::CTRL, Key::F))
    {
        state.focus_input = true;
    }
    if state.results.is_empty() {
        return;
    }
    let max = state.results.len() - 1;
    let previous = state.selected_idx;
    let editing = ctx.memory(|memory| memory.has_focus(Id::new("global_search_input")));
    let result_focused = state.focused_result
        .is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)));
    if !editing && !result_focused {
        return;
    }
    ctx.input_mut(|i| {
        if crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::ArrowDown) {
            state.selected_idx = (state.selected_idx + 1).min(max);
        }
        if crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::ArrowUp) {
            state.selected_idx = state.selected_idx.saturating_sub(1);
        }
        if crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::PageDown) {
            state.selected_idx = (state.selected_idx + 10).min(max);
        }
        if crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::PageUp) {
            state.selected_idx = state.selected_idx.saturating_sub(10);
        }
        if !editing && crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::Home) {
            state.selected_idx = 0;
        }
        if !editing && crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::End) {
            state.selected_idx = max;
        }
        if crate::app::consume_exact_key(i, egui::Modifiers::NONE, Key::Enter) {
            if let Some(hit) = state.results.get(state.selected_idx) {
                output.jump_to = Some((hit.channel.clone(), hit.message.id));
            }
        }
    });
    state.scroll_to_selected |= state.selected_idx != previous;
    if result_focused && state.selected_idx != previous {
        if let Some(hit) = state.results.get(state.selected_idx) {
            ctx.memory_mut(|memory| memory.request_focus(search_hit_id(hit)));
        }
    }
}

fn stat_pill(ui: &mut Ui, text: String) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use crust_core::model::{MessageFlags, MsgKind, Sender, UserId};
    use smallvec::SmallVec;
    use std::collections::VecDeque;

    fn mk_channel(name: &str) -> (ChannelId, ChannelState) {
        let id = ChannelId::new(name);
        let mut st = ChannelState::new(id.clone());
        st.messages = VecDeque::new();
        (id, st)
    }

    fn mk_msg(id: u64, login: &str, text: &str, secs_ago: i64) -> ChatMessage {
        ChatMessage {
            id: MessageId(id),
            server_id: None,
            timestamp: Utc::now() - Duration::seconds(secs_ago),
            channel: ChannelId::new("testchannel"),
            sender: Sender {
                user_id: UserId("1".to_string()),
                login: login.into(),
                display_name: login.into(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: vec![],
            },
            raw_text: text.into(),
            spans: SmallVec::new(),
            twitch_emotes: vec![],
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    fn test_name_paint() -> crust_core::model::SenderNamePaint {
        crust_core::model::SenderNamePaint {
            function: "linear-gradient".to_owned(),
            angle: Some(90.0),
            stops: vec![
                crust_core::model::SenderNamePaintStop {
                    at: 0.0,
                    color: "#ff0000".to_owned(),
                },
                crust_core::model::SenderNamePaintStop {
                    at: 1.0,
                    color: "#0000ff".to_owned(),
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn narrow_results_wrap_and_the_last_line_remains_clickable() {
        for width in [240.0, 360.0, 480.0] {
            let ctx = Context::default();
            let images = UiImageCache::default();
            let mut hit = SearchHit {
                channel: ChannelId::new("channel"),
                message: mk_msg(9, "a_long_viewer_name", "A narrow result must preserve readable text, including café and a long URL: https://example.com/a/very/long/path/that/still/belongs/to/the/same/message", 0),
            };
            hit.message.sender.name_paint = Some(test_name_paint());
            let mut clicked = false;
            let mut row_rect = egui::Rect::NOTHING;
            let mut shapes = Vec::new();
            for _ in 0..3 {
                shapes = ctx
                    .run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 400.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                let available = ui.available_rect_before_wrap();
                                row_rect = render_hit_row(ui, &hit, true, &images, true, |_| {
                                    clicked = true
                                })
                                .rect;
                                assert!(
                                    row_rect.right() <= available.right() + 0.5,
                                    "{row_rect:?} exceeds {available:?}"
                                );
                            });
                        },
                    )
                    .shapes;
            }
            let text = shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text)
                        if text.galley.text().contains("A narrow result") =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
                .unwrap();
            assert!(super::super::name_paint::cached_name_count(&ctx) > 0);
            assert!(text.galley.rows.len() > 1);
            let text_bounds = text.galley.rect.translate(text.pos.to_vec2());
            assert!(
                text_bounds.right() <= row_rect.right() + 0.5,
                "snippet {text_bounds:?} exceeds result {row_rect:?} at width {width}; rows={:?}",
                text.galley
                    .rows
                    .iter()
                    .map(|row| row.rect)
                    .collect::<Vec<_>>()
            );
            let position = egui::pos2(row_rect.center().x, row_rect.bottom() - 5.0);
            for pressed in [true, false] {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 400.0),
                        )),
                        events: vec![
                            egui::Event::PointerMoved(position),
                            egui::Event::PointerButton {
                                pos: position,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_hit_row(ui, &hit, true, &images, true, |_| clicked = true);
                        });
                    },
                );
            }
            assert!(
                clicked,
                "The wrapped row should remain clickable at width {width}"
            );
        }
    }

    #[test]
    fn narrow_search_header_keeps_help_and_load_older_inside_the_viewport() {
        for width in [240.0, 320.0, 480.0] {
            let ctx = Context::default();
            let mut search = GlobalSearchState::default();
            let channel = ChannelId::new("a_very_long_channel_name");
            let mut output = GlobalSearchOutput::default();
            let mut pending = HashMap::new();
            let mut shapes = Vec::new();
            for _ in 0..3 {
                shapes = ctx
                    .run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 400.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                render_header(ui, &mut search, &mut output);
                                render_group_header(ui, &channel, 1500, false, &mut pending, &mut output);
                            });
                        },
                    )
                    .shapes;
            }
            let mut load_position = None;
            for shape in &shapes {
                if let egui::epaint::Shape::Text(text) = &shape.shape {
                    assert!(
                        text.pos.x + text.galley.size().x <= width - 7.0,
                        "Text at width {width} escaped the viewport: {}",
                        text.galley.text()
                    );
                    if text.galley.text() == "Load older" {
                        load_position = Some(text.pos + text.galley.size() * 0.5);
                    }
                    if text.galley.text().starts_with("#a_very_long_channel_name") {
                        assert!(
                            text.pos.x <= 9.0,
                            "Channel headings should stay left-aligned"
                        );
                    }
                }
            }
            let position = load_position.expect("The history button should remain visible");
            for pressed in [true, false] {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 400.0),
                        )),
                        events: vec![
                            egui::Event::PointerMoved(position),
                            egui::Event::PointerButton {
                                pos: position,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_header(ui, &mut search, &mut output);
                            render_group_header(ui, &channel, 1500, false, &mut pending, &mut output);
                        });
                    },
                );
            }
            assert_eq!(output.load_older_requests, [channel]);
        }
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let mut channels: HashMap<ChannelId, ChannelState> = HashMap::new();
        let (id, mut st) = mk_channel("foo");
        st.messages.push_back(mk_msg(1, "alice", "hi", 10));
        channels.insert(id, st);
        let hits = run_search(&[], &channels);
        assert!(hits.is_empty());
    }

    #[test]
    fn from_plus_substring_filters_across_channels() {
        let mut channels: HashMap<ChannelId, ChannelState> = HashMap::new();
        let (id1, mut c1) = mk_channel("a");
        let (id2, mut c2) = mk_channel("b");
        c1.messages.push_back(mk_msg(1, "alice", "hello world", 20));
        c1.messages.push_back(mk_msg(2, "bob", "hello there", 10));
        c2.messages
            .push_back(mk_msg(3, "alice", "no match here", 5));
        c2.messages.push_back(mk_msg(4, "alice", "hello again", 15));
        channels.insert(id1, c1);
        channels.insert(id2, c2);

        let ParseOutcome { predicates, .. } = parse("from:alice hello");
        let hits = run_search(&predicates, &channels);
        assert_eq!(hits.len(), 2);
        // newest-first ordering: secs_ago 15 hit comes before secs_ago 20 hit
        assert_eq!(hits[0].message.id, MessageId(4));
        assert_eq!(hits[1].message.id, MessageId(1));
    }

    #[test]
    fn fingerprint_changes_when_message_added() {
        let mut channels: HashMap<ChannelId, ChannelState> = HashMap::new();
        let (id, st) = mk_channel("foo");
        channels.insert(id.clone(), st);
        let fp0 = fingerprint(&channels);
        channels
            .get_mut(&id)
            .unwrap()
            .messages
            .push_back(mk_msg(1, "a", "b", 0));
        let fp1 = fingerprint(&channels);
        assert_ne!(fp0, fp1);
    }

    #[test]
    fn embedded_close_button_keeps_the_search_window_closed() {
        let ctx = Context::default();
        let channels = HashMap::new();
        let images = UiImageCache::default();
        let mut search = GlobalSearchState::default();
        search.request_open();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let mut shapes = Vec::new();
        for _ in 0..2 {
            shapes = ctx.run(input(), |ctx| {
                show_global_search_window(ctx, &channels, &mut search, false, &images, true);
            }).shapes;
        }
        let close_position = shapes.iter().find_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == "Close" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        }).expect("Close button should be visible in the embedded search window");
        let mut close_requested = false;
        for pressed in [true, false] {
            let mut frame_input = input();
            frame_input.events = vec![
                egui::Event::PointerMoved(close_position),
                egui::Event::PointerButton {
                    pos: close_position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let _ = ctx.run(frame_input, |ctx| {
                close_requested |= show_global_search_window(
                    ctx, &channels, &mut search, false, &images, true,
                ).close_requested;
            });
        }
        assert!(close_requested);
        assert!(!search.open);
    }

    #[test]
    fn full_buffer_rotation_refreshes_search_results() {
        let (channel, mut messages) = mk_channel("foo");
        for id in 0..1500 {
            messages.push_message(mk_msg(id, "alice", "needle", 1500 - id as i64));
        }
        let mut channels = HashMap::from([(channel.clone(), messages)]);
        let mut search = GlobalSearchState {
            input: "needle".to_owned(),
            ..Default::default()
        };
        assert!(refresh_if_stale(&mut search, &channels));
        assert!(!refresh_if_stale(&mut search, &channels));
        channels.get_mut(&channel).unwrap().push_message(mk_msg(1500, "alice", "needle", 0));
        assert!(refresh_if_stale(&mut search, &channels));
        assert_eq!(search.results.len(), 1500);
        assert!(search.results.iter().any(|hit| hit.message.id == MessageId(1500)));
        assert!(!search.results.iter().any(|hit| hit.message.id == MessageId(0)));
    }

    #[test]
    fn message_edits_refresh_filters_without_changing_buffer_length() {
        let (channel, mut messages) = mk_channel("foo");
        let mut message = mk_msg(1, "alice", "needle", 0);
        message.channel = channel.clone();
        message.server_id = Some("server-id".to_owned());
        messages.push_message(message);
        let mut channels = HashMap::from([(channel.clone(), messages)]);
        let mut search = GlobalSearchState {
            input: "is:deleted".to_owned(),
            ..Default::default()
        };
        refresh_if_stale(&mut search, &channels);
        assert!(search.results.is_empty());
        channels.get_mut(&channel).unwrap().delete_message("server-id");
        search.on_event(&AppEvent::MessageDeleted {
            channel,
            server_id: "server-id".to_owned(),
        });
        assert!(refresh_if_stale(&mut search, &channels));
        assert_eq!(search.results.len(), 1);
        assert!(search.results[0].message.flags.is_deleted);
        assert!(!refresh_if_stale(&mut search, &channels));
    }

    #[test]
    fn restored_mention_deletions_refresh_retained_channel_search_results() {
        for (query, before, after) in [("is:deleted", 0, 1), ("!is:deleted", 1, 0)] {
            let (channel, mut room) = mk_channel("example");
            let mut message = mk_msg(1, "alice", "needle", 0);
            message.channel = channel.clone();
            message.server_id = Some("retained-message".to_owned());
            message.flags.is_mention = true;
            room.push_message(message.clone());
            let mut app = crust_core::AppState::default();
            app.channels.insert(channel.clone(), room);
            let mut search = GlobalSearchState {
                input: query.to_owned(),
                ..Default::default()
            };
            assert!(refresh_if_stale(&mut search, &app.channels));
            assert_eq!(search.results.len(), before);
            assert!(!refresh_if_stale(&mut search, &app.channels));
            let fingerprint_before = fingerprint(&app.channels);

            // The saved mention has another local ID but describes the same
            // channel message that is already present in the live buffer.
            message.id = MessageId(99);
            message.flags.is_deleted = true;
            let event = AppEvent::MentionsLoaded { messages: vec![message] };
            search.on_event(&event);
            let AppEvent::MentionsLoaded { messages } = event else { unreachable!() };
            app.prepend_mentions_history(messages);

            let retained = &app.channels[&channel].messages[0];
            assert_eq!(retained.id, MessageId(1));
            assert!(retained.flags.is_deleted);
            assert_eq!(fingerprint(&app.channels), fingerprint_before,
                "saved deletion must leave channel length and endpoint IDs unchanged");
            assert!(refresh_if_stale(&mut search, &app.channels));
            assert_eq!(search.results.len(), after, "stale results for {query}");
            assert!(!refresh_if_stale(&mut search, &app.channels));
        }
    }

    #[test]
    fn replacement_channel_with_equal_name_length_refreshes_search() {
        let (old, messages) = mk_channel("foo");
        let mut channels = HashMap::from([(old.clone(), messages)]);
        let before = fingerprint(&channels);
        channels.remove(&old);
        let (new, messages) = mk_channel("bar");
        channels.insert(new, messages);
        assert_ne!(fingerprint(&channels), before);
    }

    #[test]
    fn keyboard_jump_uses_the_visually_selected_grouped_row() {
        let (first, mut first_messages) = mk_channel("first");
        first_messages.push_message(mk_msg(1, "alice", "needle", 0));
        first_messages.push_message(mk_msg(3, "alice", "needle", 20));
        let (second, mut second_messages) = mk_channel("second");
        second_messages.push_message(mk_msg(2, "alice", "needle", 10));
        let channels = HashMap::from([(first.clone(), first_messages), (second, second_messages)]);
        let mut search = GlobalSearchState {
            input: "needle".to_owned(),
            ..Default::default()
        };
        refresh_if_stale(&mut search, &channels);
        assert_eq!(search.results.iter().map(|hit| hit.message.id.0).collect::<Vec<_>>(), vec![1, 3, 2]);
        let ctx = Context::default();
        let mut output = GlobalSearchOutput::default();
        for key in [Key::ArrowDown, Key::Enter] {
            let input = egui::RawInput {
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                ctx.memory_mut(|memory| memory.request_focus(Id::new("global_search_input")));
                handle_keyboard(ctx, &mut search, &mut output);
            });
        }
        assert_eq!(search.selected_idx, 1);
        assert_eq!(output.jump_to, Some((first, MessageId(3))));
    }

    #[test]
    fn new_hits_preserve_the_selected_message() {
        let (channel, mut messages) = mk_channel("foo");
        messages.push_message(mk_msg(1, "alice", "needle", 10));
        messages.push_message(mk_msg(2, "alice", "needle", 20));
        let mut channels = HashMap::from([(channel.clone(), messages)]);
        let mut search = GlobalSearchState {
            input: "needle".to_owned(),
            ..Default::default()
        };
        refresh_if_stale(&mut search, &channels);
        search.selected_idx = 1;
        channels.get_mut(&channel).unwrap().push_message(mk_msg(3, "alice", "needle", 0));
        refresh_if_stale(&mut search, &channels);
        assert_eq!(search.results[search.selected_idx].message.id, MessageId(2));
    }

    #[test]
    fn search_limit_retains_only_the_newest_matches() {
        let mut channels = HashMap::new();
        for channel_index in 0..4 {
            let (channel, mut messages) = mk_channel(&format!("channel{channel_index}"));
            for row in 0..1500 {
                let id = channel_index * 1500 + row;
                messages.push_message(mk_msg(id, "alice", "needle", 6000 - id as i64));
            }
            channels.insert(channel, messages);
        }
        let hits = run_search(&parse("needle").predicates, &channels);
        assert_eq!(hits.len(), MAX_HITS);
        assert_eq!(hits.first().unwrap().message.id, MessageId(5999));
        assert_eq!(hits.last().unwrap().message.id, MessageId(1000));
    }


    fn search_key(key: Key) -> egui::Event {
        egui::Event::Key {
            key, physical_key: None, pressed: true, repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn search_widget_frame(
        ctx: &Context,
        state: &mut GlobalSearchState,
        channels: &HashMap<ChannelId, ChannelState>,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, GlobalSearchOutput) {
        let mut output = GlobalSearchOutput::default();
        let rendered = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO, egui::vec2(480.0, 320.0),
            )),
            events, ..Default::default()
        }, |ctx| {
            CentralPanel::default().show(ctx, |ui| {
                render_body(ui, channels, state, &mut output, &UiImageCache::default(), true);
            });
        });
        (rendered, output)
    }

    #[test]
    fn painted_search_names_keep_row_click_and_keyboard_focus() {
        let (channel, mut messages) = mk_channel("example");
        let mut first = mk_msg(1, "first_login", "needle first", 0);
        first.sender.display_name = "Custom Display Name".to_owned();
        messages.push_message(first);
        messages.push_message(mk_msg(2, "second_login", "needle second", 1));
        let mut channels = HashMap::from([(channel.clone(), messages)]);
        let mut search = GlobalSearchState {
            open: true,
            input: "needle".to_owned(),
            focus_input: true,
            ..Default::default()
        };
        refresh_if_stale(&mut search, &channels);
        let ctx = Context::default();
        let mut rendered = egui::FullOutput::default();
        for _ in 0..3 {
            rendered = search_widget_frame(&ctx, &mut search, &channels, Vec::new()).0;
        }
        let name = rendered
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Custom Display Name" => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()))
                }
                _ => None,
            })
            .expect("search uses the displayed sender name");
        for message in &mut channels.get_mut(&channel).unwrap().messages {
            message.sender.name_paint = Some(test_name_paint());
        }
        search.invalidate();
        refresh_if_stale(&mut search, &channels);
        for _ in 0..2 {
            rendered = search_widget_frame(&ctx, &mut search, &channels, Vec::new()).0;
        }
        assert!(super::super::name_paint::cached_name_count(&ctx) >= 2);
        assert!(!rendered.shapes.iter().any(|shape| matches!(
            &shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "Custom Display Name"
        )));
        let mut action = GlobalSearchOutput::default();
        for pressed in [true, false] {
            action = search_widget_frame(
                &ctx,
                &mut search,
                &channels,
                vec![
                    egui::Event::PointerMoved(name.center()),
                    egui::Event::PointerButton {
                        pos: name.center(),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: Default::default(),
                    },
                ],
            )
            .1;
        }
        assert_eq!(action.jump_to, Some((channel.clone(), MessageId(1))));
        assert_eq!(
            search.focused_result,
            Some(search_hit_id(&search.results[0]))
        );
        search_widget_frame(
            &ctx,
            &mut search,
            &channels,
            vec![search_key(Key::ArrowDown)],
        );
        assert_eq!(search.selected_idx, 1);
        let (_, action) =
            search_widget_frame(&ctx, &mut search, &channels, vec![search_key(Key::Enter)]);
        assert_eq!(action.jump_to, Some((channel, MessageId(2))));
    }

    #[test]
    fn search_buttons_keep_enter_instead_of_jumping_to_a_result() {
        for button_label in ["Clear", "Close"] {
            let (channel, mut messages) = mk_channel("example");
            messages.push_message(mk_msg(1, "alice", "needle", 0));
            let channels = HashMap::from([(channel, messages)]);
            let mut search = GlobalSearchState {
                open: true, input: "needle".to_owned(), focus_input: true, ..Default::default()
            };
            refresh_if_stale(&mut search, &channels);
            let ctx = Context::default();
            let mut rendered = egui::FullOutput::default();
            for _ in 0..3 {
                rendered = search_widget_frame(&ctx, &mut search, &channels, Vec::new()).0;
            }
            let button = rendered.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == button_label =>
                    Some(egui::Rect::from_min_size(text.pos, text.galley.size())),
                _ => None,
            }).unwrap();
            let mut focused = false;
            for _ in 0..10 {
                let mut events = vec![search_key(Key::Tab)];
                events.push(egui::Event::Key {
                    key: Key::Tab, physical_key: None, pressed: false,
                    repeat: false, modifiers: egui::Modifiers::NONE,
                });
                search_widget_frame(&ctx, &mut search, &channels, events);
                focused = ctx.memory(|memory| memory.focused())
                    .and_then(|id| ctx.read_response(id))
                    .is_some_and(|response| response.rect.contains_rect(button));
                if focused { break; }
            }
            assert!(focused, "{button_label} must be reachable by keyboard");
            let (_, action) = search_widget_frame(&ctx, &mut search, &channels, vec![search_key(Key::Enter)]);
            assert!(action.jump_to.is_none(), "Enter on {button_label} must not jump to a search hit");
            if button_label == "Clear" {
                assert!(search.input.is_empty());
                assert!(search.results.is_empty());
            } else {
                assert!(!search.open);
                assert!(action.close_requested);
            }
        }
    }

    #[test]
    fn search_navigation_leaves_another_focused_control_alone() {
        let mut search = GlobalSearchState {
            open: true, results: vec![SearchHit {
                channel: ChannelId::new("example"), message: mk_msg(1, "alice", "needle", 0),
            }], ..Default::default()
        };
        let ctx = Context::default();
        for key in [Key::Enter, Key::ArrowDown, Key::ArrowUp, Key::PageDown, Key::PageUp, Key::Home, Key::End] {
            let mut output = GlobalSearchOutput::default();
            let _ = ctx.run(egui::RawInput {
                events: vec![search_key(key)], ..Default::default()
            }, |ctx| {
                ctx.memory_mut(|memory| memory.request_focus(Id::new("another_control")));
                handle_keyboard(ctx, &mut search, &mut output);
                assert!(ctx.input(|input| input.key_pressed(key)),
                    "search must leave {key:?} for the focused control");
            });
            assert!(output.jump_to.is_none());
        }
        let _ = ctx.run(egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::F, physical_key: None, pressed: true, repeat: false,
                modifiers: egui::Modifiers::CTRL,
            }],
            ..Default::default()
        }, |ctx| {
            ctx.memory_mut(|memory| memory.request_focus(Id::new("another_control")));
            handle_keyboard(ctx, &mut search, &mut GlobalSearchOutput::default());
            assert!(ctx.input(|input| input.key_pressed(Key::F)));
        });
        assert!(!search.focus_input);
    }

    #[test]
    fn popup_search_shortcut_selects_the_existing_query_from_another_control() {
        let ctx = Context::default();
        let (channel, messages) = mk_channel("room");
        let channels = HashMap::from([(channel, messages)]);
        let mut search = GlobalSearchState {
            open: true, input: "original café🙂".to_owned(), focus_input: true,
            ..Default::default()
        };
        for _ in 0..3 { search_widget_frame(&ctx, &mut search, &channels, Vec::new()); }
        let input_id = Id::new("global_search_input");
        let range = egui::TextEdit::load_state(&ctx, input_id).unwrap().cursor.char_range().unwrap();
        assert_eq!(range.primary.index.min(range.secondary.index), 0);
        assert_eq!(range.primary.index.max(range.secondary.index), search.input.chars().count());
        search_widget_frame(&ctx, &mut search, &channels, vec![search_key(Key::Tab)]);
        search_widget_frame(&ctx, &mut search, &channels, Vec::new());
        assert!(search.focused_control.is_some());
        assert_ne!(search.focused_control, Some(input_id));
        let (_, action) = search_widget_frame(&ctx, &mut search, &channels, vec![egui::Event::Key {
            key: Key::F, physical_key: None, pressed: true, repeat: false,
            modifiers: egui::Modifiers::CTRL,
        }]);
        assert!(action.jump_to.is_none());
        assert!(action.load_older_requests.is_empty());
        assert!(ctx.memory(|memory| memory.has_focus(input_id)));
        assert!(!ctx.input(|input| input.key_pressed(Key::F)));
        search_widget_frame(&ctx, &mut search, &channels,
            vec![egui::Event::Text("replacement".to_owned())]);
        assert_eq!(search.input, "replacement");
    }

    fn result_fixture(count: usize) -> (GlobalSearchState, HashMap<ChannelId, ChannelState>) {
        let mut channels = HashMap::new();
        let mut results = Vec::with_capacity(count);
        for index in 0..count {
            let (channel, channel_state) = mk_channel(&format!("room{}", index / 1250));
            channels.entry(channel.clone()).or_insert(channel_state);
            let text = if index % 3 == 0 {
                format!("Result {index}: café 🙂 with a longer message and https://example.test/a/path that wraps across narrow search windows. ").repeat(2)
            } else {
                format!("Result {index}: short message")
            };
            let mut message = mk_msg(index as u64 + 1, &format!("viewer{index}"), &text, index as i64);
            message.channel = channel.clone();
            results.push(SearchHit { channel, message });
        }
        (GlobalSearchState { open: true, results, ..Default::default() }, channels)
    }

    struct ResultFrame {
        output: egui::FullOutput,
        action: GlobalSearchOutput,
        content_height: f32,
    }

    // Draw every row as a reference for geometry and the controlled benchmark.
    fn render_all_results(
        ui: &mut Ui,
        channels: &HashMap<ChannelId, ChannelState>,
        state: &mut GlobalSearchState,
        output: &mut GlobalSearchOutput,
    ) -> f32 {
        let scroll_to_selected = std::mem::take(&mut state.scroll_to_selected);
        state.focused_result = None;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let images = UiImageCache::default();
            let mut index = 0;
            for group in state.results.chunk_by(|left, right| left.channel == right.channel) {
                let channel = &group[0].channel;
                render_group_header(
                    ui, channel, group.len(),
                    channels.get(channel).is_some_and(|state| state.local_history_exhausted),
                    &mut state.load_older_pending, output,
                );
                for hit in group {
                    let selected = index == state.selected_idx;
                    let response = render_hit_row(ui, hit, selected, &images, false, |hit| {
                        output.jump_to = Some((hit.channel.clone(), hit.message.id));
                    });
                    if selected && scroll_to_selected {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    if response.has_focus() {
                        state.focused_result = Some(response.id);
                        state.selected_idx = index;
                    }
                    index += 1;
                }
                ui.add_space(4.0);
            }
        }).content_size.y
    }

    fn result_frame(
        ctx: &Context,
        state: &mut GlobalSearchState,
        channels: &HashMap<ChannelId, ChannelState>,
        width: f32,
        cached: bool,
        events: Vec<egui::Event>,
    ) -> ResultFrame {
        let mut action = GlobalSearchOutput::default();
        let mut content_height = 0.0;
        let output = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO, egui::vec2(width, 320.0),
            )),
            time: Some(ctx.cumulative_pass_nr() as f64 * 0.2),
            events,
            ..Default::default()
        }, |ctx| {
            handle_keyboard(ctx, state, &mut action);
            CentralPanel::default().show(ctx, |ui| {
                if cached {
                    render_results(ui, channels, state, &mut action, &UiImageCache::default(), false);
                    content_height = state.layout_cache.content_height;
                } else {
                    content_height = render_all_results(ui, channels, state, &mut action);
                }
            });
        });
        ResultFrame { output, action, content_height }
    }

    fn visible_result_text(output: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
        output.shapes.iter().filter_map(|shape| {
            let egui::Shape::Text(text) = &shape.shape else { return None };
            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
            shape.clip_rect.intersects(rect).then(|| (text.galley.text().to_owned(), text.pos))
        }).collect()
    }

    #[test]
    fn cached_search_results_match_all_rows_at_the_top_middle_and_end() {
        let (mut cached, channels) = result_fixture(MAX_HITS);
        let mut all = GlobalSearchState { open: true, results: cached.results.clone(), ..Default::default() };
        let cached_ctx = Context::default();
        let all_ctx = Context::default();
        for selected in [0, 2499, MAX_HITS - 1] {
            cached.selected_idx = selected;
            cached.scroll_to_selected = true;
            all.selected_idx = selected;
            all.scroll_to_selected = true;
            for _ in 0..4 {
                result_frame(&cached_ctx, &mut cached, &channels, 360.0, true, Vec::new());
                result_frame(&all_ctx, &mut all, &channels, 360.0, false, Vec::new());
            }
            let actual = result_frame(&cached_ctx, &mut cached, &channels, 360.0, true, Vec::new());
            let expected = result_frame(&all_ctx, &mut all, &channels, 360.0, false, Vec::new());
            assert!((actual.content_height - expected.content_height).abs() < 1.0);
            let actual_text = visible_result_text(&actual.output);
            let expected_text = visible_result_text(&expected.output);
            assert_eq!(actual_text.len(), expected_text.len(), "visible text count at {selected}");
            for ((actual_text, actual_pos), (expected_text, expected_pos)) in actual_text.iter().zip(&expected_text) {
                assert_eq!(actual_text, expected_text, "visible text at {selected}");
                assert!((*actual_pos - *expected_pos).length() < 1.0,
                    "text moved at {selected}: {actual_pos:?} vs {expected_pos:?}");
            }
            assert_eq!(cached.layout_cache.measured_rows, 0);
            assert!(cached.layout_cache.rendered_rows < 30,
                "{} rows rendered at {selected}", cached.layout_cache.rendered_rows);
            assert_eq!(cached.layout_cache.rows.len(), MAX_HITS);
        }
    }

    #[test]
    fn result_height_cache_tracks_content_layout_and_removed_results() {
        let (mut search, channels) = result_fixture(200);
        let ctx = Context::default();
        for _ in 0..3 { result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new()); }
        assert_eq!(search.layout_cache.measured_rows, 0);
        let original_height = search.layout_cache.content_height;
        search.results[151].message.raw_text = "A changed offscreen result has a much longer preview with café and 🙂. ".repeat(4);
        result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new());
        assert_eq!(search.layout_cache.measured_rows, 1);
        assert!(search.layout_cache.content_height > original_height);
        search.results[152].message.sender.display_name = "A substantially longer displayed name".repeat(4);
        result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new());
        assert_eq!(search.layout_cache.measured_rows, 1);
        result_frame(&ctx, &mut search, &channels, 240.0, true, Vec::new());
        assert_eq!(search.layout_cache.measured_rows, search.results.len());
        ctx.style_mut(|style| style.spacing.item_spacing.y += 3.0);
        result_frame(&ctx, &mut search, &channels, 240.0, true, Vec::new());
        assert_eq!(search.layout_cache.measured_rows, search.results.len());
        let mut fonts = egui::FontDefinitions::default();
        fonts.families.insert(egui::FontFamily::Proportional,
            fonts.families[&egui::FontFamily::Monospace].clone());
        ctx.set_fonts(fonts);
        result_frame(&ctx, &mut search, &channels, 240.0, true, Vec::new());
        assert_eq!(search.layout_cache.measured_rows, search.results.len());
        let mut fonts = ctx.fonts(|fonts| fonts.lock().fonts.definitions().clone());
        let primary = fonts.families[&egui::FontFamily::Proportional][0].clone();
        Arc::make_mut(fonts.font_data.get_mut(&primary).unwrap()).tweak.scale *= 1.1;
        ctx.set_fonts(fonts);
        result_frame(&ctx, &mut search, &channels, 240.0, true, Vec::new());
        assert_eq!(search.layout_cache.measured_rows, search.results.len());
        search.results.drain(..150);
        result_frame(&ctx, &mut search, &channels, 240.0, true, Vec::new());
        assert_eq!(search.layout_cache.rows.len(), search.results.len());
        search.clear();
        assert!(search.layout_cache.rows.is_empty());
    }

    #[test]
    fn cached_results_preserve_end_key_focus_jump_and_load_older() {
        let (mut search, channels) = result_fixture(MAX_HITS);
        let ctx = Context::default();
        for _ in 0..3 { result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new()); }
        let first = search_hit_id(&search.results[0]);
        ctx.memory_mut(|memory| memory.request_focus(first));
        result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new());
        assert_eq!(search.focused_result, Some(first));
        result_frame(&ctx, &mut search, &channels, 480.0, true, vec![search_key(Key::End)]);
        assert_eq!(search.selected_idx, MAX_HITS - 1);
        let last = search_hit_id(search.results.last().unwrap());
        assert_eq!(search.focused_result, Some(last));
        for _ in 0..3 { result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new()); }
        let shown = result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new());
        let last_rect = ctx.read_response(last).unwrap().rect;
        assert!(ctx.screen_rect().intersects(last_rect),
            "End scrolls the focused row into view: row={last_rect:?}, visible={:?}",
            visible_result_text(&shown.output));
        let jumped = result_frame(&ctx, &mut search, &channels, 480.0, true, vec![search_key(Key::Enter)]);
        assert_eq!(jumped.action.jump_to, Some((ChannelId::new("room3"), MessageId(MAX_HITS as u64))));

        ctx.memory_mut(|memory| memory.surrender_focus(last));
        search.selected_idx = 2500;
        search.scroll_to_selected = true;
        for _ in 0..4 { result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new()); }
        let shown = result_frame(&ctx, &mut search, &channels, 480.0, true, Vec::new());
        let target = shown.output.shapes.iter().find_map(|shape| {
            let egui::Shape::Text(text) = &shape.shape else { return None };
            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
            (text.galley.text() == "Load older" && shape.clip_rect.contains_rect(rect)).then_some(rect.center())
        }).expect("the selected group's load-older control remains visible");
        let mut action = GlobalSearchOutput::default();
        for pressed in [true, false] {
            action = result_frame(&ctx, &mut search, &channels, 480.0, true, vec![
                egui::Event::PointerMoved(target),
                egui::Event::PointerButton { pos: target, button: egui::PointerButton::Primary,
                    pressed, modifiers: egui::Modifiers::NONE },
            ]).action;
        }
        assert_eq!(action.load_older_requests, vec![ChannelId::new("room2")]);
        assert!(search.load_older_pending.contains_key(&ChannelId::new("room2")));
    }

    #[test]
    fn search_preview_truncation_preserves_unicode_and_exact_limits() {
        assert_eq!(truncate("café🙂", 5), "café🙂");
        assert_eq!(truncate("café🙂 more", 5), "café🙂...");
        assert_eq!(truncate("", 0), "");
        assert_eq!(truncate("🙂", 0), "...");
    }

    #[test]
    fn load_older_keeps_its_focus_when_preceding_row_counts_change() {
        let ctx = Context::default();
        let channel = ChannelId::new("target");
        let mut pending = HashMap::new();
        let mut button_id = Id::NULL;
        let mut show = |preceding_rows, events| {
            let mut output = GlobalSearchOutput::default();
            let _ = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO, egui::vec2(480.0, 320.0),
                )),
                events,
                ..Default::default()
            }, |ctx| {
                CentralPanel::default().show(ctx, |ui| {
                    for _ in 0..preceding_rows {
                        ui.label("A preceding result");
                    }
                    button_id = render_group_header(
                        ui, &channel, 200, false, &mut pending, &mut output,
                    ).id;
                });
            });
            (button_id, output)
        };
        let (original, _) = show(1, Vec::new());
        ctx.memory_mut(|memory| memory.request_focus(original));
        let (after, _) = show(6, Vec::new());
        assert_eq!(after, original);
        assert!(ctx.memory(|memory| memory.has_focus(original)));
        let (_, output) = show(3, vec![search_key(Key::Enter)]);
        assert_eq!(output.load_older_requests, vec![channel]);
    }

    #[test]
    #[ignore = "controlled 5000-result CPU layout benchmark"]
    fn benchmark_global_search_results_cache() {
        let (mut cached, channels) = result_fixture(MAX_HITS);
        let mut all = GlobalSearchState { open: true, results: cached.results.clone(), ..Default::default() };
        let cached_ctx = Context::default();
        let all_ctx = Context::default();
        let start = std::time::Instant::now();
        std::hint::black_box(result_frame(&cached_ctx, &mut cached, &channels, 480.0, true, Vec::new()));
        let cold_cached = start.elapsed();
        let start = std::time::Instant::now();
        std::hint::black_box(result_frame(&all_ctx, &mut all, &channels, 480.0, false, Vec::new()));
        let cold_all = start.elapsed();
        for _ in 0..4 {
            result_frame(&cached_ctx, &mut cached, &channels, 480.0, true, Vec::new());
            result_frame(&all_ctx, &mut all, &channels, 480.0, false, Vec::new());
        }
        let mut cached_times = Vec::new();
        let mut all_times = Vec::new();
        for _ in 0..25 {
            let start = std::time::Instant::now();
            std::hint::black_box(result_frame(&cached_ctx, &mut cached, &channels, 480.0, true, Vec::new()));
            cached_times.push(start.elapsed());
            let start = std::time::Instant::now();
            std::hint::black_box(result_frame(&all_ctx, &mut all, &channels, 480.0, false, Vec::new()));
            all_times.push(start.elapsed());
        }
        cached_times.sort();
        all_times.sort();
        println!("global search {MAX_HITS} results; cold cached={cold_cached:?}; cold all={cold_all:?}; warm cached median={:?}; warm all median={:?}; cached rows={}; measured rows={}",
            cached_times[12], all_times[12], cached.layout_cache.rendered_rows, cached.layout_cache.measured_rows);
        assert_eq!(cached.layout_cache.measured_rows, 0);
        assert!(cached.layout_cache.rendered_rows < 30);
    }

    #[test]
    fn invalid_unicode_colors_do_not_panic() {
        assert_eq!(parse_hex_color("#12Ab34"), Some(egui::Color32::from_rgb(0x12, 0xab, 0x34)));
        for color in ["€abc", "ab€c", "abc€", "ééé", "12345g", "12345"] {
            assert_eq!(parse_hex_color(color), None);
        }
    }
}
