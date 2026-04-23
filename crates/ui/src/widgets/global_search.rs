use std::collections::HashMap;

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
    last_seen_fingerprint: u64,
    parsed: Vec<Predicate>,
    pub load_older_pending: HashMap<ChannelId, std::time::Instant>,
}

impl GlobalSearchState {
    pub fn request_open(&mut self) {
        self.open = true;
        self.focus_input = true;
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
}

/// Compute a cheap fingerprint of the current buffer state.
///
/// Used to decide whether the search result cache needs to be rebuilt.
/// Collisions are acceptablea missed refresh shows stale hits for one frame.
pub fn fingerprint(channels: &HashMap<ChannelId, ChannelState>) -> u64 {
    let mut n: u64 = channels.len() as u64;
    for (id, state) in channels {
        n = n
            .wrapping_mul(1469598103934665603)
            .wrapping_add(state.messages.len() as u64);
        n = n
            .wrapping_mul(1469598103934665603)
            .wrapping_add(id.as_str().len() as u64);
    }
    n
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
    let buffer_changed = fp != state.last_seen_fingerprint;
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
    state.last_seen_fingerprint = fp;
    state.results = run_search(&state.parsed, channels);
    if state.selected_idx >= state.results.len() {
        state.selected_idx = state.results.len().saturating_sub(1);
    }
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
    let mut hits: Vec<SearchHit> = channels
        .iter()
        .flat_map(|(id, st)| {
            st.messages.iter().filter_map(move |m| {
                if matches(preds, m, id) {
                    Some(SearchHit {
                        channel: id.clone(),
                        message: m.clone(),
                    })
                } else {
                    None
                }
            })
        })
        .collect();
    hits.sort_by(|a, b| b.message.timestamp.cmp(&a.message.timestamp));
    hits.truncate(MAX_HITS);
    hits
}

use egui::{
    Align, Button, CentralPanel, Context, Frame, Id, Key, Layout, RichText, ScrollArea, Stroke, Ui,
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
                            .stroke(Stroke::new(1.0, t::border_accent())),
                    )
                    .open(&mut open)
                    .show(child_ctx, |ui| {
                        render_body(ui, channels, state, &mut output);
                    });
                state.open = open;
            }
            _ => {
                CentralPanel::default()
                    .frame(
                        chrome::card_frame()
                            .fill(t::bg_dialog())
                            .stroke(Stroke::new(1.0, t::border_subtle()))
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
    ui.horizontal(|ui| {
        chrome::dialog_header(
            ui,
            "Search all channels",
            Some("Predicates: from: in: has: is: regex: badge: subtier:  |  !pred negates"),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
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
    handle_keyboard(ui.ctx(), state, output);

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

            let groups = group_hits_by_channel(&state.results);
            let mut running_idx = 0usize;
            for (channel, hits_in_group) in groups {
                let group_len = hits_in_group.len();
                render_group_header(
                    ui,
                    &channel,
                    group_len,
                    &mut state.load_older_pending,
                    output,
                );
                for hit in hits_in_group {
                    let selected = running_idx == state.selected_idx;
                    render_hit_row(ui, hit, selected, |hit_ref| {
                        output.jump_to = Some((hit_ref.channel.clone(), hit_ref.message.id));
                    });
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

fn group_hits_by_channel(hits: &[SearchHit]) -> Vec<(ChannelId, Vec<&SearchHit>)> {
    let mut order: Vec<ChannelId> = Vec::new();
    let mut buckets: HashMap<ChannelId, Vec<&SearchHit>> = HashMap::new();
    for h in hits {
        if !buckets.contains_key(&h.channel) {
            order.push(h.channel.clone());
        }
        buckets.entry(h.channel.clone()).or_default().push(h);
    }
    order
        .into_iter()
        .map(|c| {
            let v = buckets.remove(&c).unwrap_or_default();
            (c, v)
        })
        .collect()
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
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("#{}  ({} hits)", channel.display_name(), hit_count))
                .font(t::small())
                .color(t::text_secondary()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if pending { "Loading…" } else { "Load older" };
            let btn = ui.add_enabled(!pending, Button::new(RichText::new(label).font(t::tiny())));
            if btn.clicked() {
                load_older_pending.insert(channel.clone(), std::time::Instant::now());
                output.load_older_requests.push(channel.clone());
            }
        });
    });
}

fn render_hit_row(
    ui: &mut Ui,
    hit: &SearchHit,
    selected: bool,
    mut on_jump: impl FnMut(&SearchHit),
) {
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
            ui.horizontal(|ui| {
                let ts = hit.message.timestamp.format("%H:%M").to_string();
                ui.label(RichText::new(ts).font(t::tiny()).color(t::text_muted()));
                let color = hit
                    .message
                    .sender
                    .color
                    .as_deref()
                    .and_then(parse_hex_color)
                    .unwrap_or_else(t::text_secondary);
                ui.label(
                    RichText::new(&hit.message.sender.login)
                        .font(t::small())
                        .color(color),
                );
                let body = hit.message.raw_text.trim();
                let body = truncate(body, 160);
                ui.label(RichText::new(body).font(t::small()));
            });
        })
        .response;
    if resp.interact(egui::Sense::click()).clicked() {
        on_jump(hit);
    }
}

fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn handle_keyboard(ctx: &Context, state: &mut GlobalSearchState, output: &mut GlobalSearchOutput) {
    if state.results.is_empty() {
        return;
    }
    let max = state.results.len() - 1;
    ctx.input_mut(|i| {
        if i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) {
            state.selected_idx = (state.selected_idx + 1).min(max);
        }
        if i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) {
            state.selected_idx = state.selected_idx.saturating_sub(1);
        }
        if i.consume_key(egui::Modifiers::NONE, Key::PageDown) {
            state.selected_idx = (state.selected_idx + 10).min(max);
        }
        if i.consume_key(egui::Modifiers::NONE, Key::PageUp) {
            state.selected_idx = state.selected_idx.saturating_sub(10);
        }
        if i.consume_key(egui::Modifiers::NONE, Key::Home) {
            state.selected_idx = 0;
        }
        if i.consume_key(egui::Modifiers::NONE, Key::End) {
            state.selected_idx = max;
        }
        if i.consume_key(egui::Modifiers::NONE, Key::Enter) {
            if let Some(hit) = state.results.get(state.selected_idx) {
                output.jump_to = Some((hit.channel.clone(), hit.message.id));
            }
        }
    });
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
}
