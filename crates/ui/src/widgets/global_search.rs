use std::collections::HashMap;
use std::hash::{Hash, Hasher};

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
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.regex_error = None;
        self.results.clear();
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

use crate::theme as t;

use super::chrome;

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
        let esc = child_ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
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
                        render_body(ui, channels, state, &mut output);
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
                        render_body(ui, channels, state, &mut output);
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
    render_results(ui, channels, state, output);
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
            .iter()
            .map(|h| &h.channel)
            .collect::<std::collections::HashSet<_>>()
            .len();
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
        state.focus_input = false;
    }
}

fn render_results(
    ui: &mut Ui,
    channels: &HashMap<ChannelId, ChannelState>,
    state: &mut GlobalSearchState,
    output: &mut GlobalSearchOutput,
) {
    let hit_count = state.results.len();
    let scroll_to_selected = std::mem::take(&mut state.scroll_to_selected);

    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if state.results.is_empty() {
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
                return;
            }

            let mut running_idx = 0usize;
            for hits_in_group in state.results.chunk_by(|a, b| a.channel == b.channel) {
                let channel = &hits_in_group[0].channel;
                let group_len = hits_in_group.len();
                render_group_header(
                    ui,
                    channel,
                    group_len,
                    &mut state.load_older_pending,
                    output,
                );
                for hit in hits_in_group {
                    let selected = running_idx == state.selected_idx;
                    let response = render_hit_row(ui, hit, selected, |hit_ref| {
                        output.jump_to = Some((hit_ref.channel.clone(), hit_ref.message.id));
                    });
                    if selected && scroll_to_selected {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    running_idx += 1;
                }
                ui.add_space(4.0);
            }
        });

    if hit_count > 0 && state.selected_idx >= hit_count {
        state.selected_idx = hit_count - 1;
    }
    let _ = channels;
}

fn render_group_header(
    ui: &mut Ui,
    channel: &ChannelId,
    hit_count: usize,
    load_older_pending: &mut HashMap<ChannelId, std::time::Instant>,
    output: &mut GlobalSearchOutput,
) {
    let pending = load_older_pending
        .get(channel)
        .map(|ts| ts.elapsed() < std::time::Duration::from_secs(10))
        .unwrap_or(false);
    if !pending {
        load_older_pending.remove(channel);
    }
    ui.horizontal_top(|ui| {
        let label = if pending { "Loading..." } else { "Load older" };
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
        let btn = ui.add_enabled(!pending, Button::new(RichText::new(label).font(t::tiny())));
        if btn.clicked() {
            load_older_pending.insert(channel.clone(), std::time::Instant::now());
            output.load_older_requests.push(channel.clone());
        }
    });
}

fn render_hit_row(
    ui: &mut Ui,
    hit: &SearchHit,
    selected: bool,
    mut on_jump: impl FnMut(&SearchHit),
) -> egui::Response {
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
            let mut row = egui::text::LayoutJob::default();
            row.append(
                &hit.message.timestamp.format("%H:%M ").to_string(),
                0.0,
                egui::TextFormat {
                    font_id: t::tiny(),
                    color: t::text_muted(),
                    ..Default::default()
                },
            );
            let color = hit
                .message
                .sender
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or_else(t::text_secondary);
            row.append(
                &hit.message.sender.login,
                0.0,
                egui::TextFormat {
                    font_id: t::small(),
                    color,
                    ..Default::default()
                },
            );
            row.append(
                &format!(" {}", truncate(hit.message.raw_text.trim(), 160)),
                0.0,
                egui::TextFormat {
                    font_id: t::small(),
                    color: t::text_primary(),
                    ..Default::default()
                },
            );
            ui.add(egui::Label::new(row).wrap());
        })
        .response
        .interact(egui::Sense::click());
    if resp.clicked() {
        on_jump(hit);
    }
    resp
}

fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(s, 16).ok()?;
    Some(egui::Color32::from_rgb(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    ))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out
    }
}

fn handle_keyboard(ctx: &Context, state: &mut GlobalSearchState, output: &mut GlobalSearchOutput) {
    if state.results.is_empty() {
        return;
    }
    let max = state.results.len() - 1;
    let previous = state.selected_idx;
    let editing = ctx.memory(|memory| memory.has_focus(Id::new("global_search_input")));
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

    #[test]
    fn narrow_results_wrap_and_the_last_line_remains_clickable() {
        for width in [240.0, 360.0, 480.0] {
            let ctx = Context::default();
            let hit = SearchHit {
                channel: ChannelId::new("channel"),
                message: mk_msg(9, "a_long_viewer_name", "A narrow result must preserve readable text, including café and a long URL: https://example.com/a/very/long/path/that/still/belongs/to/the/same/message", 0),
            };
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
                                row_rect = render_hit_row(ui, &hit, true, |_| clicked = true).rect;
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
            assert!(text.galley.rows.len() > 1);
            assert!(text.pos.x + text.galley.size().x <= row_rect.right() + 0.5);
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
                            render_hit_row(ui, &hit, true, |_| clicked = true);
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
                                render_group_header(ui, &channel, 1500, &mut pending, &mut output);
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
                            render_group_header(ui, &channel, 1500, &mut pending, &mut output);
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
                show_global_search_window(ctx, &channels, &mut search, false);
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
                    ctx, &channels, &mut search, false,
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
            let _ = ctx.run(input, |ctx| handle_keyboard(ctx, &mut search, &mut output));
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

    #[test]
    fn invalid_unicode_colors_do_not_panic() {
        assert_eq!(parse_hex_color("#12Ab34"), Some(egui::Color32::from_rgb(0x12, 0xab, 0x34)));
        for color in ["€abc", "ab€c", "abc€", "ééé", "12345g", "12345"] {
            assert_eq!(parse_hex_color(color), None);
        }
    }
}
