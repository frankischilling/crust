use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use egui::{Color32, Id, Label, RichText, ScrollArea, Ui, Vec2};
use image::DynamicImage;
use tokio::sync::mpsc;

use crust_core::{
    events::{AppCommand, LinkPreview},
    model::{
        filters::FilterAction, Badge, ChannelId, ChatMessage, MessageFlags, MsgKind, ReplyInfo,
        SenderNamePaint, Span,
    },
};

use crate::theme as t;
use crate::widgets::message_search::MessageSearchState;

/// Returned from [`MessageList::show`].
pub struct MessageListResult {
    /// Set when the user right-clicked a message and chose "Reply".
    pub reply: Option<ReplyInfo>,
    /// Set when a username was clicked: (login, sender_badges).
    pub profile_request: Option<(String, Vec<Badge>)>,
    /// Lightweight per-frame counters for the debug performance overlay.
    pub perf_stats: MessageListPerfStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageListPerfStats {
    pub retained_rows: usize,
    pub active_rows: usize,
    pub rendered_rows: usize,
    pub boundary_hidden_rows: usize,
    pub prefix_rebuilt: bool,
    pub height_cache_misses: usize,
}

// -- Zero-allocation visible-index abstraction ----------------------------
// When no search filter is active, `VisibleIndices::All(n)` avoids
// allocating a Vec<usize> with one entry per message every frame.

enum VisibleIndices {
    /// No filter - visible index `i` maps directly to message index `i`.
    All(usize),
    /// Search filter active - sparse set of matching message indices.
    Filtered(Vec<usize>),
}

impl VisibleIndices {
    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::All(n) => *n,
            Self::Filtered(v) => v.len(),
        }
    }

    /// Map a virtual row number to the real message index.
    #[inline]
    fn get(&self, i: usize) -> usize {
        match self {
            Self::All(_) => i,
            Self::Filtered(v) => v[i],
        }
    }

    /// Find the first virtual row matching a predicate on the real index.
    fn position<F: Fn(usize) -> bool>(&self, pred: F) -> Option<usize> {
        match self {
            Self::All(n) => (0..*n).position(|i| pred(i)),
            Self::Filtered(v) => v.iter().position(|&idx| pred(idx)),
        }
    }

    fn collect_ids(&self, messages: &VecDeque<ChatMessage>) -> Vec<u64> {
        (0..self.len())
            .map(|i| messages[self.get(i)].id.0)
            .collect()
    }
}

/// Extra context to locate a reply parent when direct msg-id lookup fails.
#[derive(Clone, Default)]
struct ReplyScrollHint {
    parent_user_login: String,
    parent_msg_body: String,
    child_msg_local_id: u64,
}

#[derive(Clone)]
enum StaticFrameCacheEntry {
    Loaded(egui::TextureHandle),
    Unavailable,
}

/// Pre-computed per-frame values passed to `render_message` to avoid
/// redundant `data_mut` lock acquisitions and `Id::new` constructions.
struct RenderCtx {
    reply_key: Id,
    scroll_to_key: Id,
    scroll_hint_key: Id,
    /// The server-id of the message currently being flash-highlighted (if any).
    highlight_server_id: Option<String>,
    /// Pre-computed base alpha for the highlight flash (0.0 when inactive).
    highlight_alpha: f32,
    /// Whether the flash is still animating (need repaint).
    #[allow(dead_code)]
    highlight_animating: bool,
}

const EMOTE_SIZE: f32 = 22.0;
const TOOLTIP_EMOTE_SIZE: f32 = 112.0;
const BADGE_SIZE: f32 = 18.0;
const TOOLTIP_BADGE_SIZE: f32 = 72.0;
const MAX_CACHED_USERNAME_COLORS: usize = 8192;
/// Row left/right padding (px)
const ROW_PAD_X: f32 = 6.0;
/// Row top/bottom padding (px)
const ROW_PAD_Y: f32 = 2.0;
/// Fallback height for rows we have never rendered before.
const EST_H: f32 = 26.0;
const HOT_WINDOW_TRIGGER: usize = 600;
const HOT_WINDOW_ROWS: usize = 400;
const HOT_WINDOW_EXPAND_CHUNK: usize = 200;
const HOT_WINDOW_EXPAND_THRESHOLD_PX: f32 = 24.0;
const COMPACT_BOUNDARY_HEIGHT: f32 = 22.0;

// Twitch username fallback palette used when no explicit color is provided.
const TWITCH_USERNAME_COLORS: [Color32; 15] = [
    Color32::from_rgb(255, 0, 0),     // Red
    Color32::from_rgb(0, 0, 255),     // Blue
    Color32::from_rgb(0, 255, 0),     // Green
    Color32::from_rgb(178, 34, 34),   // FireBrick
    Color32::from_rgb(255, 127, 80),  // Coral
    Color32::from_rgb(154, 205, 50),  // YellowGreen
    Color32::from_rgb(255, 69, 0),    // OrangeRed
    Color32::from_rgb(46, 139, 87),   // SeaGreen
    Color32::from_rgb(218, 165, 32),  // GoldenRod
    Color32::from_rgb(210, 105, 30),  // Chocolate
    Color32::from_rgb(95, 158, 160),  // CadetBlue
    Color32::from_rgb(30, 144, 255),  // DodgerBlue
    Color32::from_rgb(255, 105, 180), // HotPink
    Color32::from_rgb(138, 43, 226),  // BlueViolet
    Color32::from_rgb(0, 255, 127),   // SpringGreen
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HotWindowState {
    active_rows: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ScrollAnchor {
    message_id: u64,
    /// Distance from the viewport top to the anchor row's top edge.
    distance_to_viewport_top: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HotWindowPlan {
    active_start: usize,
    active_len: usize,
    hidden_rows: usize,
}

impl HotWindowPlan {
    fn full(total_rows: usize) -> Self {
        Self {
            active_start: 0,
            active_len: total_rows,
            hidden_rows: 0,
        }
    }

    fn has_boundary(&self) -> bool {
        self.hidden_rows > 0
    }

    fn boundary_height(&self) -> f32 {
        if self.has_boundary() {
            COMPACT_BOUNDARY_HEIGHT
        } else {
            0.0
        }
    }
}

fn compute_hot_window_plan(
    total_rows: usize,
    search_filtering: bool,
    following_bottom: bool,
    requested_active_rows: usize,
) -> HotWindowPlan {
    if search_filtering || total_rows <= HOT_WINDOW_TRIGGER {
        return HotWindowPlan::full(total_rows);
    }

    let active_len = if following_bottom {
        HOT_WINDOW_ROWS.min(total_rows)
    } else {
        requested_active_rows.max(HOT_WINDOW_ROWS).min(total_rows)
    };
    let active_start = total_rows.saturating_sub(active_len);

    HotWindowPlan {
        active_start,
        active_len,
        hidden_rows: active_start,
    }
}

fn expand_hot_window_rows(current_active_rows: usize, total_rows: usize) -> usize {
    current_active_rows
        .saturating_add(HOT_WINDOW_EXPAND_CHUNK)
        .min(total_rows)
}

fn should_expand_hot_window(plan: HotWindowPlan, scroll_paused: bool, scroll_offset: f32) -> bool {
    scroll_paused && plan.has_boundary() && scroll_offset <= HOT_WINDOW_EXPAND_THRESHOLD_PX
}

fn capture_scroll_anchor(
    active_ids: &[u64],
    prefix: &[f32],
    boundary_height: f32,
    scroll_offset: f32,
) -> Option<ScrollAnchor> {
    if active_ids.is_empty() {
        return None;
    }

    let content_offset = (scroll_offset - boundary_height).max(0.0);
    let idx = prefix
        .partition_point(|&p| p <= content_offset)
        .saturating_sub(1)
        .min(active_ids.len().saturating_sub(1));
    let anchor_top = boundary_height + prefix.get(idx).copied().unwrap_or(0.0);
    Some(ScrollAnchor {
        message_id: active_ids[idx],
        distance_to_viewport_top: anchor_top - scroll_offset,
    })
}

fn compensate_anchor_offset(
    anchor: &ScrollAnchor,
    active_ids: &[u64],
    height_cache: &HashMap<u64, f32>,
    boundary_height: f32,
) -> Option<f32> {
    let anchor_idx = active_ids.iter().position(|&id| id == anchor.message_id)?;
    let before_anchor: f32 = active_ids[..anchor_idx]
        .iter()
        .map(|id| height_cache.get(id).copied().unwrap_or(EST_H))
        .sum();
    Some((boundary_height + before_anchor - anchor.distance_to_viewport_top).max(0.0))
}

fn map_snapshot_ids_to_indices(snapshot_ids: &[u64], live_ids: &[u64]) -> Vec<usize> {
    let id_to_index: HashMap<u64, usize> = live_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i))
        .collect();
    snapshot_ids
        .iter()
        .filter_map(|id| id_to_index.get(id).copied())
        .collect()
}

#[derive(Clone, Copy)]
pub struct KeywordHighlightMatch {
    pub color: Option<egui::Color32>,
}

/// Scrollable, bottom-anchored list of chat messages with inline emote images.
pub struct MessageList<'a> {
    messages: &'a VecDeque<ChatMessage>,
    /// Raw image bytes keyed by CDN URL: CDN url → (width, height, raw_bytes)
    emote_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
    /// For sending on-demand image-fetch requests (e.g. HD emote on hover).
    cmd_tx: &'a mpsc::Sender<AppCommand>,
    /// Channel identifier - used for per-channel scroll state.
    channel: &'a ChannelId,
    /// Cached link previews keyed by URL.
    link_previews: &'a HashMap<String, LinkPreview>,
    /// Optional active search filter for this channel.
    search: Option<&'a MessageSearchState>,
    /// Whether long messages are collapsed with an ellipsis.
    collapse_long_messages: bool,
    /// Maximum visible lines before collapse applies.
    collapse_long_message_lines: usize,
    /// Whether animated emotes should animate this frame.
    animate_emotes: bool,
    /// Whether timestamps should be shown for each message.
    show_timestamps: bool,
    /// Whether timestamps should include seconds.
    show_timestamp_seconds: bool,
    /// Whether timestamps should use 24-hour clock format.
    use_24h_timestamps: bool,
    /// Whether the local user can moderate in this channel.
    can_moderate: bool,
    /// Ignored usernames (lowercase) hidden from the message list.
    ignored_logins: &'a HashSet<String>,
    /// Compiled highlight rules used for local keyword highlighting.
    highlight_rules: &'a [crust_core::highlight::HighlightMatch],
    /// Compiled filter records used for hiding messages.
    filter_records: &'a [crust_core::model::filters::CompiledFilter],
    /// Moderation action presets
    mod_action_presets: &'a [crust_core::model::mod_actions::ModActionPreset],
}

impl<'a> MessageList<'a> {
    fn message_filter_action(&self, msg: &ChatMessage) -> Option<FilterAction> {
        if self.filter_records.is_empty() {
            return None;
        }

        crust_core::model::filters::check_filters(
            self.filter_records,
            Some(self.channel),
            &msg.raw_text,
            &msg.sender.login,
        )
    }

    pub(crate) fn new(
        messages: &'a VecDeque<ChatMessage>,
        emote_bytes: &'a HashMap<String, (u32, u32, Arc<[u8]>)>,
        cmd_tx: &'a mpsc::Sender<AppCommand>,
        channel: &'a ChannelId,
        link_previews: &'a HashMap<String, LinkPreview>,
        search: Option<&'a MessageSearchState>,
        collapse_long_messages: bool,
        collapse_long_message_lines: usize,
        animate_emotes: bool,
        show_timestamps: bool,
        show_timestamp_seconds: bool,
        use_24h_timestamps: bool,
        can_moderate: bool,
        ignored_logins: &'a HashSet<String>,
        highlight_rules: &'a [crust_core::highlight::HighlightMatch],
        filter_records: &'a [crust_core::model::filters::CompiledFilter],
        mod_action_presets: &'a [crust_core::model::mod_actions::ModActionPreset],
    ) -> Self {
        Self {
            messages,
            emote_bytes,
            cmd_tx,
            channel,
            link_previews,
            search,
            collapse_long_messages,
            collapse_long_message_lines: collapse_long_message_lines.max(1),
            animate_emotes,
            show_timestamps,
            show_timestamp_seconds,
            use_24h_timestamps,
            can_moderate,
            ignored_logins,
            highlight_rules,
            filter_records,
            mod_action_presets,
        }
    }

    /// Render the message list with auto-scroll behaviour.
    ///
    /// * Auto-scrolls to the bottom when new messages arrive.
    /// * Pauses auto-scroll when the user scrolls up.
    /// * Shows a floating "↓ Resume scrolling" button while paused.
    /// * Returns a [`MessageListResult`] that may contain a reply request.
    pub fn show(&self, ui: &mut Ui) -> MessageListResult {
        let reply_key = Id::new("ml_reply_req").with(self.channel.as_str());
        let user_color_cache_key = Id::new("ml_user_color_cache").with(self.channel.as_str());
        let mut user_color_cache: HashMap<String, Color32> = ui
            .ctx()
            .data_mut(|d| d.get_temp(user_color_cache_key).unwrap_or_default());
        // We need the available rect before the scroll area consumes it
        let panel_rect = ui.available_rect_before_wrap();
        // Keep a small safety gap at the bottom so message pixels/emotes
        // don't bleed into the input panel border while scrolling.
        let mut clip = ui.clip_rect();
        clip.max.y = clip.max.y.min(panel_rect.max.y - 2.0);
        ui.set_clip_rect(clip);

        let search_filtering = self.search.map(|s| s.is_filtering()).unwrap_or(false);
        let is_visible_message = |msg: &ChatMessage| {
            if !self.ignored_logins.is_empty() {
                let login = msg.sender.login.as_str();
                if self.ignored_logins.contains(login)
                    || self.ignored_logins.contains(&login.to_ascii_lowercase())
                {
                    return false;
                }
            }

            if matches!(self.message_filter_action(msg), Some(FilterAction::Hide)) {
                return false;
            }

            true
        };
        // PERF: use VisibleIndices enum to avoid O(n) Vec allocation when
        // no search filter is active (the common case).
        let live_visible_indices: VisibleIndices = match self.search {
            Some(search) if search.is_filtering() => VisibleIndices::Filtered(
                self.messages
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, msg)| {
                        (is_visible_message(msg) && search.matches(msg)).then_some(idx)
                    })
                    .collect(),
            ),
            _ if self.ignored_logins.is_empty() => VisibleIndices::All(self.messages.len()),
            _ => VisibleIndices::Filtered(
                self.messages
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, msg)| is_visible_message(msg).then_some(idx))
                    .collect(),
            ),
        };
        let live_visible_count = live_visible_indices.len();
        // Use a per-channel scroll area ID so offset doesn't leak
        let scroll_id = egui::Id::new("message_list").with(self.channel.as_str());
        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
        let paused_snapshot_key = egui::Id::new("paused_snapshot_ids").with(self.channel.as_str());
        let paused_before_show: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));

        // Paused snapshot behavior:
        // freeze the rendered message-id set while paused so incoming chat does
        // not keep shifting the lines the user is reading.
        let visible_indices: VisibleIndices = if paused_before_show && !search_filtering {
            let mut snapshot_ids: Vec<u64> = ui
                .ctx()
                .data_mut(|d| d.get_temp(paused_snapshot_key).unwrap_or_default());
            if snapshot_ids.is_empty() {
                snapshot_ids = live_visible_indices.collect_ids(self.messages);
            }
            let live_ids: Vec<u64> = self.messages.iter().map(|m| m.id.0).collect();
            let mapped = map_snapshot_ids_to_indices(&snapshot_ids, &live_ids);
            let persisted_ids: Vec<u64> = mapped.iter().map(|&idx| live_ids[idx]).collect();
            ui.ctx()
                .data_mut(|d| d.insert_temp(paused_snapshot_key, persisted_ids));
            if mapped.is_empty() {
                live_visible_indices
            } else {
                VisibleIndices::Filtered(mapped)
            }
        } else {
            ui.ctx()
                .data_mut(|d| d.remove_temp::<Vec<u64>>(paused_snapshot_key));
            live_visible_indices
        };
        let n = visible_indices.len();
        let paused_snapshot_new_rows = live_visible_count.saturating_sub(n);

        // If the user scrolls over the message panel, immediately pause
        // stick-to-bottom so upward wheel input can take effect this frame.
        let wheel_over_panel = ui.ctx().input(|i| {
            let over_panel = i
                .pointer
                .hover_pos()
                .map(|p| panel_rect.contains(p))
                .unwrap_or(false);
            over_panel && i.raw_scroll_delta.y.abs() > 0.0
        });
        // Store whether the wheel was used this frame, so show_resume_button
        // doesn't immediately clear the paused flag before the delta is applied.
        let wheel_key = egui::Id::new("scroll_wheel_this_frame").with(self.channel.as_str());
        // Batch two temp writes into one data_mut call.
        ui.ctx().data_mut(|d| {
            d.insert_temp(wheel_key, wheel_over_panel);
            if wheel_over_panel {
                d.insert_temp(paused_key, true);
            }
        });

        // Reset stale scroll state on first render of a channel
        // egui persists scroll offsets across sessions.  When we
        // (re-)enter a channel whose old state carried a large offset,
        // the first frame would render content at the wrong position.
        // Detect "first render" via a temp flag and force offset to 0.
        let init_key = egui::Id::new("msg_list_init").with(self.channel.as_str());
        let first_render = !ui.ctx().data_mut(|d| {
            let seen: bool = d.get_temp(init_key).unwrap_or(false);
            if !seen {
                d.insert_temp(init_key, true);
            }
            seen
        });

        // Threshold: below this, render all rows directly (no virtual scrolling).
        // This avoids height-estimation and stale-offset edge cases that cause
        // layout glitches with very few messages.  Kept low so virtual
        // scrolling kicks in early and only visible rows are rendered.
        const VIRTUAL_THRESHOLD: usize = 40;

        // Height cache
        // Keyed by MessageId (u64). Persisted in egui temp storage so that
        // off-screen rows are not re-measured every frame.  Shared between
        // the simple and virtual paths so the transition is seamless.
        //
        // PERF: use remove_temp (ownership via mem::take) instead of get_temp
        // (which clones the entire HashMap every frame).  The cache is put
        // back via insert_temp at the end of each path.
        let hc_id = egui::Id::new("msg_row_h").with(self.channel.as_str());
        let mut height_cache: std::collections::HashMap<u64, f32> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(hc_id).unwrap_or_default());
        let static_id = egui::Id::new("ml_static_frames").with(self.channel.as_str());
        let mut static_frames: HashMap<String, StaticFrameCacheEntry> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(static_id).unwrap_or_default());

        // Invalidate height cache when available width changes significantly
        // (e.g. window resize or sidebar drag), since messages re-wrap to
        // different heights at different widths.
        let avail_width = ui.available_width();
        let width_key = egui::Id::new("msg_list_width").with(self.channel.as_str());
        let prev_width: f32 = ui.ctx().data_mut(|d| d.get_temp(width_key).unwrap_or(0.0));
        if (avail_width - prev_width).abs() > 2.0 {
            height_cache.clear();
            ui.ctx().data_mut(|d| d.insert_temp(width_key, avail_width));
        }

        // Clear stale height cache when the channel has no messages
        // (e.g. after leaving and re-joining a channel).
        // Also reset the "first render" flag so re-entering triggers
        // a fresh scroll-offset reset.
        if self.messages.is_empty() {
            height_cache.clear();
            ui.ctx()
                .data_mut(|d| d.insert_temp::<bool>(init_key, false));
        }

        // Scroll-to-reply target
        // Written by the reply-header click handler; read and cleared here so
        // it only fires once.
        let scroll_to_key = egui::Id::new("ml_scroll_to").with(self.channel.as_str());
        let scroll_hint_key = egui::Id::new("ml_scroll_hint").with(self.channel.as_str());
        let highlight_key = egui::Id::new("ml_highlight_msg").with(self.channel.as_str());
        let highlight_time_key = egui::Id::new("ml_highlight_t").with(self.channel.as_str());
        let hot_state_key = egui::Id::new("ml_hot_window").with(self.channel.as_str());
        let anchor_key = egui::Id::new("ml_scroll_anchor").with(self.channel.as_str());
        let scroll_target_pending = ui.ctx().data_mut(|d| {
            d.get_temp::<String>(scroll_to_key).is_some()
                || d.get_temp::<ReplyScrollHint>(scroll_hint_key).is_some()
        });
        let forced_offset: Option<f32> = {
            let target: Option<String> = ui.ctx().data_mut(|d| d.remove_temp(scroll_to_key));
            let hint: Option<ReplyScrollHint> =
                ui.ctx().data_mut(|d| d.remove_temp(scroll_hint_key));
            let idx =
                self.find_reply_target_index(&visible_indices, target.as_deref(), hint.as_ref());
            idx.map(|idx| {
                // Store the target server_id + time for a brief highlight flash.
                let now = ui.input(|i| i.time);
                if let Some(tgt_id) = target {
                    ui.ctx().data_mut(|d| {
                        d.insert_temp(highlight_key, tgt_id);
                        d.insert_temp(highlight_time_key, now);
                    });
                }
                let offset: f32 = (0..idx)
                    .map(|i| {
                        height_cache
                            .get(&self.messages[visible_indices.get(i)].id.0)
                            .copied()
                            .unwrap_or(EST_H)
                    })
                    .sum();
                offset
            })
        };

        let mut hot_state: HotWindowState = ui
            .ctx()
            .data_mut(|d| d.get_temp(hot_state_key).unwrap_or_default());
        if hot_state.active_rows == 0 {
            hot_state.active_rows = HOT_WINDOW_ROWS.min(n);
        }
        let hot_plan = compute_hot_window_plan(
            n,
            search_filtering || scroll_target_pending,
            !paused_before_show,
            hot_state.active_rows,
        );
        let active_ids: Vec<u64> = (0..hot_plan.active_len)
            .map(|i| {
                self.messages[visible_indices.get(hot_plan.active_start + i)]
                    .id
                    .0
            })
            .collect();
        let paused_offset_compensation = if paused_before_show {
            ui.ctx()
                .data_mut(|d| d.get_temp::<ScrollAnchor>(anchor_key))
                .and_then(|anchor| {
                    compensate_anchor_offset(
                        &anchor,
                        &active_ids,
                        &height_cache,
                        hot_plan.boundary_height(),
                    )
                })
        } else {
            None
        };
        let mut perf_stats = MessageListPerfStats {
            retained_rows: n,
            active_rows: hot_plan.active_len,
            rendered_rows: 0,
            boundary_hidden_rows: hot_plan.hidden_rows,
            prefix_rebuilt: false,
            height_cache_misses: 0,
        };

        // -- Pre-compute highlight state once per frame -------------------
        // Previously this was looked up via 2× data_mut calls inside every
        // render_message invocation.  Computing it once here avoids dozens
        // of redundant lock acquisitions per frame.
        let rctx = {
            let (hl_id, hl_time): (Option<String>, Option<f64>) = ui
                .ctx()
                .data_mut(|d| (d.get_temp(highlight_key), d.get_temp(highlight_time_key)));
            let now = ui.input(|i| i.time);
            let (highlight_server_id, highlight_alpha, highlight_animating) = match (hl_id, hl_time)
            {
                (Some(id), Some(t0)) => {
                    let elapsed = (now - t0) as f32;
                    const FLASH_SECS: f32 = 1.5;
                    if elapsed < FLASH_SECS {
                        (Some(id), 1.0 - (elapsed / FLASH_SECS), true)
                    } else {
                        // Flash complete - clean up temp data.
                        ui.ctx().data_mut(|d| {
                            d.remove::<String>(highlight_key);
                            d.remove::<f64>(highlight_time_key);
                        });
                        (None, 0.0, false)
                    }
                }
                _ => (None, 0.0, false),
            };
            if highlight_animating {
                ui.ctx().request_repaint();
            }
            RenderCtx {
                reply_key,
                scroll_to_key,
                scroll_hint_key,
                highlight_server_id,
                highlight_alpha,
                highlight_animating,
            }
        };

        if n == 0 {
            let scroll_paused: bool = ui
                .ctx()
                .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
            let mut sa = ScrollArea::vertical()
                .id_salt(scroll_id)
                .auto_shrink([false; 2])
                .stick_to_bottom(!scroll_paused && forced_offset.is_none());
            if let Some(offset) = forced_offset {
                sa = sa.vertical_scroll_offset(offset);
            } else if let Some(offset) = paused_offset_compensation {
                sa = sa.vertical_scroll_offset(offset);
            } else if first_render {
                sa = sa.vertical_scroll_offset(0.0);
            }
            let output = sa.show(ui, |ui| {
                let full_width = ui.available_width();
                ui.set_min_width(full_width);
                let empty_h = (panel_rect.height() * 0.35).max(24.0);
                ui.add_space(empty_h);
                let text = if self.search.map(|s| s.is_filtering()).unwrap_or(false) {
                    "No messages match the current filters."
                } else {
                    "No messages yet."
                };
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(text).color(t::text_muted()));
                });
            });
            ui.ctx().data_mut(|d| {
                d.insert_temp(hc_id, height_cache);
                d.insert_temp(static_id, static_frames);
                d.insert_temp(hot_state_key, HotWindowState { active_rows: 0 });
                d.insert_temp(user_color_cache_key, user_color_cache);
                d.remove_temp::<ScrollAnchor>(anchor_key);
            });
            self.show_resume_button(ui, &output, panel_rect, paused_snapshot_new_rows);
            self.apply_snap(ui, &output);
            return MessageListResult {
                reply: self.take_reply(ui, rctx.reply_key),
                profile_request: self.take_profile_request(ui),
                perf_stats,
            };
        }

        if n < VIRTUAL_THRESHOLD {
            // -- Simple path: render every message, let egui handle layout -
            // We also measure row heights here so the cache is pre-populated
            // when the channel crosses VIRTUAL_THRESHOLD.
            let scroll_paused: bool = ui
                .ctx()
                .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
            let mut sa = ScrollArea::vertical()
                .id_salt(scroll_id)
                .auto_shrink([false; 2])
                .stick_to_bottom(!scroll_paused && forced_offset.is_none());
            if let Some(offset) = forced_offset {
                sa = sa.vertical_scroll_offset(offset);
            } else if let Some(offset) = paused_offset_compensation {
                sa = sa.vertical_scroll_offset(offset);
            } else if first_render {
                sa = sa.vertical_scroll_offset(0.0);
            }
            let output = sa.show(ui, |ui| {
                let full_width = ui.available_width();
                ui.set_min_width(full_width);
                for vi in hot_plan.active_start..(hot_plan.active_start + hot_plan.active_len) {
                    let msg_idx = visible_indices.get(vi);
                    let msg = &self.messages[msg_idx];
                    let dimmed = matches!(self.message_filter_action(msg), Some(FilterAction::Dim));
                    let top_y = ui.next_widget_position().y;
                    self.render_message(
                        ui,
                        msg,
                        dimmed,
                        &rctx,
                        &mut static_frames,
                        &mut user_color_cache,
                    );
                    let measured = ui.next_widget_position().y - top_y;
                    if measured > 0.0 {
                        height_cache.insert(msg.id.0, measured);
                    }
                }
            });

            self.show_resume_button(ui, &output, panel_rect, paused_snapshot_new_rows);
            self.apply_snap(ui, &output);
            let scroll_paused: bool = ui
                .ctx()
                .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
            let mut anchor_prefix = Vec::with_capacity(active_ids.len() + 1);
            anchor_prefix.push(0.0);
            for id in &active_ids {
                let h = height_cache.get(id).copied().unwrap_or(EST_H);
                anchor_prefix.push(anchor_prefix.last().copied().unwrap_or(0.0) + h);
            }
            self.persist_scroll_anchor(
                ui,
                anchor_key,
                scroll_paused,
                &active_ids,
                &anchor_prefix,
                0.0,
                output.state.offset.y,
            );
            ui.ctx().data_mut(|d| {
                d.insert_temp(hc_id, height_cache);
                d.insert_temp(static_id, static_frames);
                d.insert_temp(hot_state_key, HotWindowState { active_rows: 0 });
                d.insert_temp(user_color_cache_key, user_color_cache);
            });
            perf_stats.rendered_rows = hot_plan.active_len;
            return MessageListResult {
                reply: self.take_reply(ui, rctx.reply_key),
                profile_request: self.take_profile_request(ui),
                perf_stats,
            };
        }

        // Build prefix-sum array.  prefix[i] = y-offset of the top of message i.
        // PERF: reuse the previous Vec allocation via remove_temp (ownership
        // transfer) so we don't hit the allocator every frame.  We also cache
        // the previous message count so we can skip the rebuild when nothing
        // changed (the common steady-state case).
        let ps_id = egui::Id::new("msg_prefix_sum").with(self.channel.as_str());
        let ps_gen_id = egui::Id::new("msg_prefix_gen").with(self.channel.as_str());
        let mut prefix: Vec<f32> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(ps_id).unwrap_or_default());

        // Build a lightweight generation key from count + first/last message IDs
        // so we detect content changes even when the count stays the same
        // (e.g. ring-buffer eviction: pop_front + push_back at MAX_MESSAGES).
        let cur_gen: (usize, u64, u64, usize) = if hot_plan.active_len > 0 {
            let first_id = self.messages[visible_indices.get(hot_plan.active_start)]
                .id
                .0;
            let last_id = self.messages
                [visible_indices.get(hot_plan.active_start + hot_plan.active_len - 1)]
            .id
            .0;
            (hot_plan.active_len, first_id, last_id, hot_plan.hidden_rows)
        } else {
            (0, 0, 0, 0)
        };
        let prev_gen: (usize, u64, u64, usize) = ui
            .ctx()
            .data_mut(|d| d.get_temp(ps_gen_id).unwrap_or((0, 0, 0, 0)));
        let prefix_stale = prefix.len() != hot_plan.active_len + 1 || cur_gen != prev_gen;
        if prefix_stale {
            prefix.clear();
            prefix.reserve(hot_plan.active_len + 1);
            prefix.push(0.0f32);
            for vi in hot_plan.active_start..(hot_plan.active_start + hot_plan.active_len) {
                let msg_idx = visible_indices.get(vi);
                let msg = &self.messages[msg_idx];
                let h = match height_cache.get(&msg.id.0).copied() {
                    Some(h) => h,
                    None => {
                        perf_stats.height_cache_misses += 1;
                        EST_H
                    }
                };
                prefix.push(prefix.last().unwrap() + h);
            }
            ui.ctx().data_mut(|d| d.insert_temp(ps_gen_id, cur_gen));
        }
        perf_stats.prefix_rebuilt = prefix_stale;
        let boundary_h = hot_plan.boundary_height();
        let total_h = boundary_h + *prefix.last().unwrap_or(&0.0);

        // -- Virtual-scrolling render pass --------------------------------
        // show_viewport gives us the currently-visible rect in content-local
        // coordinates.  We allocate dead space for off-screen rows and only
        // call render_message for rows whose y-range overlaps the viewport.
        let scroll_paused: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
        let mut sa = ScrollArea::vertical()
            .id_salt(scroll_id)
            .auto_shrink([false; 2])
            .stick_to_bottom(!scroll_paused && forced_offset.is_none());
        if let Some(offset) = forced_offset {
            sa = sa.vertical_scroll_offset(offset);
        } else if let Some(offset) = paused_offset_compensation {
            sa = sa.vertical_scroll_offset(offset);
        } else if first_render {
            sa = sa.vertical_scroll_offset(0.0);
        }
        let output = sa.show_viewport(ui, |ui, viewport| {
            let full_width = ui.available_width();
            ui.set_min_width(full_width);

            let vis_min = viewport.min.y;
            let vis_max = viewport.max.y;

            // Overscan and minimum-window safeguards prevent first-frame
            // under-rendering when viewport reports a tiny height.
            const OVERSCAN_PX: f32 = 260.0;
            const MIN_RENDER_ROWS: usize = 24;
            let scan_min = (vis_min - OVERSCAN_PX).max(0.0);
            let scan_max = vis_max + OVERSCAN_PX;

            let scan_msg_min = (scan_min - boundary_h).max(0.0);
            let scan_msg_max = (scan_max - boundary_h).max(0.0);

            // First row whose bottom edge is visible (top < vis_max).
            let mut first = if hot_plan.active_len == 0 {
                0
            } else {
                prefix
                    .partition_point(|&p| p < scan_msg_min)
                    .saturating_sub(1)
            };
            // One past the last visible row (top <= vis_max).
            let mut last = prefix
                .partition_point(|&p| p <= scan_msg_max)
                .min(hot_plan.active_len);
            let min_last = (first + MIN_RENDER_ROWS).min(hot_plan.active_len);
            if last < min_last {
                last = min_last;
            }
            let min_rows = MIN_RENDER_ROWS.min(hot_plan.active_len);
            if last.saturating_sub(first) < min_rows {
                if last == hot_plan.active_len {
                    first = hot_plan.active_len.saturating_sub(min_rows);
                } else {
                    last = (first + min_rows).min(hot_plan.active_len);
                }
            }

            // Dead space above the visible window.
            let render_boundary = hot_plan.has_boundary() && scan_min < boundary_h;
            let top_dead = if render_boundary { 0.0 } else { boundary_h };
            let top_space = top_dead + prefix[first];
            if top_space > 0.0 {
                ui.allocate_exact_size(
                    egui::Vec2::new(full_width, top_space),
                    egui::Sense::hover(),
                );
            }
            if render_boundary {
                self.render_compact_boundary(ui, hot_plan.hidden_rows);
            }

            // Render only visible rows; measure heights for future frames.
            // Track whether any height changed so we can invalidate the
            // prefix sum for the next frame.
            let mut any_height_changed = false;
            let mut rendered_rows = 0usize;
            for i in first..last {
                let msg = &self.messages[visible_indices.get(hot_plan.active_start + i)];
                let dimmed = matches!(self.message_filter_action(msg), Some(FilterAction::Dim));
                let top_y = ui.next_widget_position().y;

                self.render_message(
                    ui,
                    msg,
                    dimmed,
                    &rctx,
                    &mut static_frames,
                    &mut user_color_cache,
                );

                let measured = ui.next_widget_position().y - top_y;
                rendered_rows += 1;
                if measured > 0.0 {
                    let prev = height_cache.insert(msg.id.0, measured);
                    if prev.map(|p| (p - measured).abs() > 0.5).unwrap_or(true) {
                        any_height_changed = true;
                    }
                }
            }
            perf_stats.rendered_rows = rendered_rows;

            // If any visible row's measured height changed, force prefix
            // rebuild next frame so the scroll bar and dead-space are correct.
            if any_height_changed {
                // Invalidate the generation key so the prefix sum is rebuilt next frame.
                ui.ctx()
                    .data_mut(|d| d.insert_temp(ps_gen_id, (0usize, 0u64, 0u64, 0usize)));
            }

            // Dead space below the visible window.
            let tail = total_h - (boundary_h + prefix[last]);
            if tail > 0.0 {
                ui.allocate_exact_size(egui::Vec2::new(full_width, tail), egui::Sense::hover());
            }
        });

        self.show_resume_button(ui, &output, panel_rect, paused_snapshot_new_rows);
        self.apply_snap(ui, &output);
        let scroll_paused: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
        if should_expand_hot_window(hot_plan, scroll_paused, output.state.offset.y) {
            let expanded_rows = expand_hot_window_rows(hot_plan.active_len, n);
            if expanded_rows != hot_plan.active_len {
                if let Some(anchor) = capture_scroll_anchor(
                    &active_ids,
                    &prefix,
                    hot_plan.boundary_height(),
                    output.state.offset.y,
                ) {
                    ui.ctx().data_mut(|d| d.insert_temp(anchor_key, anchor));
                }
                hot_state.active_rows = expanded_rows;
                ui.ctx().request_repaint();
            }
        } else if !scroll_paused {
            hot_state.active_rows = HOT_WINDOW_ROWS.min(n);
        } else {
            hot_state.active_rows = hot_plan.active_len;
        }
        self.persist_scroll_anchor(
            ui,
            anchor_key,
            scroll_paused,
            &active_ids,
            &prefix,
            hot_plan.boundary_height(),
            output.state.offset.y,
        );
        ui.ctx()
            .data_mut(|d| d.insert_temp(hot_state_key, hot_state));
        // Persist height cache and prefix-sum Vec for next frame.
        ui.ctx().data_mut(|d| {
            d.insert_temp(hc_id, height_cache);
            d.insert_temp(ps_id, prefix);
            d.insert_temp(static_id, static_frames);
            d.insert_temp(user_color_cache_key, user_color_cache);
        });
        MessageListResult {
            reply: self.take_reply(ui, rctx.reply_key),
            profile_request: self.take_profile_request(ui),
            perf_stats,
        }
    }

    fn find_reply_target_index(
        &self,
        visible_indices: &VisibleIndices,
        target_id: Option<&str>,
        hint: Option<&ReplyScrollHint>,
    ) -> Option<usize> {
        // Primary path: exact message-id match from reply-parent-msg-id.
        if let Some(target_id) = target_id.map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(idx) = visible_indices.position(|msg_idx| {
                self.messages[msg_idx]
                    .server_id
                    .as_deref()
                    .map(str::trim)
                    .map(|sid| sid == target_id)
                    .unwrap_or(false)
            }) {
                return Some(idx);
            }
            // Be tolerant of case mismatches from external history providers.
            if let Some(idx) = visible_indices.position(|msg_idx| {
                self.messages[msg_idx]
                    .server_id
                    .as_deref()
                    .map(str::trim)
                    .map(|sid| sid.eq_ignore_ascii_case(target_id))
                    .unwrap_or(false)
            }) {
                return Some(idx);
            }
        }

        // Fallback path: find the nearest older line whose sender/body match
        // the embedded reply metadata. This handles history rows where msg-id
        // tags may be missing or inconsistent.
        let hint = hint?;
        let login = hint.parent_user_login.trim();
        let body = hint.parent_msg_body.trim();
        if login.is_empty() && body.is_empty() {
            return None;
        }

        let child_vi = visible_indices
            .position(|msg_idx| self.messages[msg_idx].id.0 == hint.child_msg_local_id)
            .unwrap_or(visible_indices.len());
        if child_vi == 0 {
            return None;
        }

        for vi in (0..child_vi).rev() {
            let m = &self.messages[visible_indices.get(vi)];
            if !login.is_empty() && !m.sender.login.eq_ignore_ascii_case(login) {
                continue;
            }
            if body.is_empty() {
                return Some(vi);
            }
            let raw = m.raw_text.trim();
            if raw == body || raw.starts_with(body) || body.starts_with(raw) {
                return Some(vi);
            }
        }

        None
    }

    fn persist_scroll_anchor(
        &self,
        ui: &Ui,
        anchor_key: Id,
        scroll_paused: bool,
        active_ids: &[u64],
        prefix: &[f32],
        boundary_height: f32,
        scroll_offset: f32,
    ) {
        ui.ctx().data_mut(|d| {
            if scroll_paused {
                if let Some(anchor) =
                    capture_scroll_anchor(active_ids, prefix, boundary_height, scroll_offset)
                {
                    d.insert_temp(anchor_key, anchor);
                }
            } else {
                d.remove_temp::<ScrollAnchor>(anchor_key);
            }
        });
    }

    fn render_compact_boundary(&self, ui: &mut Ui, hidden_rows: usize) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), COMPACT_BOUNDARY_HEIGHT),
            egui::Sense::hover(),
        );
        let fill = t::sparkline_bg();
        let stroke = t::border_subtle();
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(6), fill);
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(6),
            egui::Stroke::new(1.0, stroke),
            egui::epaint::StrokeKind::Outside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{hidden_rows} older messages hidden while following live chat"),
            t::small(),
            t::text_muted(),
        );
    }

    /// Read and clear the reply request stored by a context menu during this frame.
    fn take_reply(&self, ui: &Ui, key: Id) -> Option<ReplyInfo> {
        ui.ctx().data_mut(|d| {
            let v: Option<ReplyInfo> = d.get_temp(key);
            if v.is_some() {
                d.remove::<ReplyInfo>(key);
            }
            v
        })
    }

    /// Read and clear the profile-request stored by a username click this frame.
    fn take_profile_request(&self, ui: &Ui) -> Option<(String, Vec<Badge>)> {
        let key = Id::new("ml_profile_req").with(self.channel.as_str());
        ui.ctx().data_mut(|d| {
            let v: Option<(String, Vec<Badge>)> = d.get_temp(key);
            if v.is_some() {
                d.remove::<(String, Vec<Badge>)>(key);
            }
            v
        })
    }

    fn reply_info_for_message(msg: &ChatMessage) -> Option<ReplyInfo> {
        if msg.flags.is_deleted || !msg.channel.is_twitch() {
            return None;
        }
        let parent_msg_id = msg.server_id.clone()?.trim().to_owned();
        if parent_msg_id.is_empty() {
            return None;
        }
        Some(ReplyInfo {
            parent_msg_id,
            parent_user_login: msg.sender.login.clone(),
            parent_display_name: msg.sender.display_name.clone(),
            parent_msg_body: msg.raw_text.clone(),
        })
    }

    fn resolve_server_id_for_actions(&self, msg: &ChatMessage) -> Option<String> {
        if let Some(server_id) = msg
            .server_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(server_id.to_owned());
        }

        // Fresh local echoes can temporarily have no msg-id until Twitch echoes
        // the same payload back. Try to resolve that server-id from a near match.
        self.messages.iter().rev().find_map(|candidate| {
            let server_id = candidate
                .server_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())?;

            if candidate.channel != msg.channel {
                return None;
            }
            if !candidate
                .sender
                .login
                .eq_ignore_ascii_case(&msg.sender.login)
            {
                return None;
            }
            if candidate.raw_text != msg.raw_text {
                return None;
            }

            let delta_ms = (candidate.timestamp.timestamp_millis()
                - msg.timestamp.timestamp_millis())
            .unsigned_abs();
            if delta_ms > 10_000 {
                return None;
            }

            Some(server_id.to_owned())
        })
    }

    fn redemption_is_terminal(status: Option<&str>) -> bool {
        status
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.eq_ignore_ascii_case("fulfilled")
                    || s.eq_ignore_ascii_case("canceled")
                    || s.eq_ignore_ascii_case("cancelled")
            })
            .unwrap_or(false)
    }

    fn redemption_can_update(
        reward_id: Option<&str>,
        redemption_id: Option<&str>,
        status: Option<&str>,
    ) -> bool {
        !Self::redemption_is_terminal(status)
            && reward_id.map(str::trim).is_some_and(|s| !s.is_empty())
            && redemption_id.map(str::trim).is_some_and(|s| !s.is_empty())
    }

    fn show_message_context_menu(&self, ui: &mut Ui, msg: &ChatMessage, reply_key: Id) {
        let resolved_server_id = self.resolve_server_id_for_actions(msg);

        if let Some(info) = Self::reply_info_for_message(msg) {
            if ui.button("Reply").clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(reply_key, info));
                ui.close_menu();
            }
        } else {
            let hint = if msg.flags.is_deleted {
                "Cannot reply to deleted messages"
            } else if !msg.channel.is_twitch() {
                "Inline replies are currently supported for Twitch messages only"
            } else {
                "Cannot reply to this message yet (missing message id)"
            };
            ui.add_enabled(false, egui::Button::new("Reply"))
                .on_hover_text(hint);
        }

        ui.separator();
        ui.label(RichText::new("Message").small().color(t::text_muted()));
        if ui.button("Copy message text").clicked() {
            ui.ctx().copy_text(msg.raw_text.clone());
            ui.close_menu();
        }
        if let Some(server_id) = resolved_server_id.as_deref() {
            if ui.button("Copy message ID").clicked() {
                ui.ctx().copy_text(server_id.to_owned());
                ui.close_menu();
            }
        }

        ui.separator();
        ui.label(RichText::new("User").small().color(t::text_muted()));
        if ui.button("Copy username").clicked() {
            ui.ctx().copy_text(msg.sender.login.clone());
            ui.close_menu();
        }
        if !msg.sender.login.trim().is_empty() {
            if ui.button("Open user card").clicked() {
                let _ = self.cmd_tx.try_send(AppCommand::ShowUserCard {
                    login: msg.sender.login.clone(),
                    channel: self.channel.clone(),
                });
                ui.close_menu();
            }
        }

        if let Some(reward_id) = msg
            .flags
            .custom_reward_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            ui.separator();
            ui.label(
                RichText::new("Channel points")
                    .small()
                    .color(t::text_muted()),
            );
            if ui.button("Copy custom reward ID").clicked() {
                ui.ctx().copy_text(reward_id.to_owned());
                ui.close_menu();
            }
        }

        if let MsgKind::ChannelPointsReward {
            reward_title,
            reward_id,
            redemption_id,
            ..
        } = &msg.msg_kind
        {
            ui.separator();
            ui.label(
                RichText::new("Redemption details")
                    .small()
                    .color(t::text_muted()),
            );

            if ui.button("Copy reward title").clicked() {
                ui.ctx().copy_text(reward_title.clone());
                ui.close_menu();
            }

            if let Some(reward_id) = reward_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if ui.button("Copy reward ID").clicked() {
                    ui.ctx().copy_text(reward_id.to_owned());
                    ui.close_menu();
                }
            }

            if let Some(redemption_id) = redemption_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if ui.button("Copy redemption ID").clicked() {
                    ui.ctx().copy_text(redemption_id.to_owned());
                    ui.close_menu();
                }
            }
        }

        if self.can_moderate && msg.channel.is_twitch() {
            ui.separator();
            ui.label(RichText::new("Mod actions").small().color(t::text_muted()));

            let target_user_id = msg.sender.user_id.0.trim();
            let can_user_action = !target_user_id.is_empty() && !msg.sender.login.trim().is_empty();

            ui.horizontal_wrapped(|ui| {
                if let Some(server_id) = resolved_server_id.as_deref() {
                    if ui.button("Quick: Delete").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::DeleteMessage {
                            channel: msg.channel.clone(),
                            message_id: server_id.to_owned(),
                        });
                        ui.close_menu();
                    }
                } else {
                    ui.add_enabled(false, egui::Button::new("Quick: Delete"));
                }

                if can_user_action {
                    if ui.button("Quick: Timeout 10m").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::TimeoutUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                            seconds: 10 * 60,
                            reason: None,
                        });
                        ui.close_menu();
                    }
                    if ui.button("Quick: Ban").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::BanUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                            reason: None,
                        });
                        ui.close_menu();
                    }
                    if ui.button("Quick: Warn").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::WarnUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                            reason: "Please review the channel rules.".to_owned(),
                        });
                        ui.close_menu();
                    }
                } else {
                    ui.add_enabled(false, egui::Button::new("Quick: Timeout 10m"));
                    ui.add_enabled(false, egui::Button::new("Quick: Ban"));
                    ui.add_enabled(false, egui::Button::new("Quick: Warn"));
                }
            });
            ui.separator();

            if let Some(server_id) = resolved_server_id.as_deref() {
                if ui.button("Delete message").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::SendMessage {
                        channel: msg.channel.clone(),
                        text: format!("/delete {server_id}"),
                        reply_to_msg_id: None,
                        reply: None,
                    });
                    ui.close_menu();
                }
            } else {
                ui.add_enabled(false, egui::Button::new("Delete message"))
                    .on_hover_text("Cannot delete this message yet (missing message id)");
            }

            if can_user_action {
                ui.menu_button("Timeouts", |ui| {
                    for (label, seconds) in [("1m", 60u32), ("10m", 10 * 60), ("1h", 60 * 60)] {
                        if ui.button(format!("Timeout {label}")).clicked() {
                            let _ = self.cmd_tx.try_send(AppCommand::TimeoutUser {
                                channel: self.channel.clone(),
                                login: msg.sender.login.clone(),
                                user_id: target_user_id.to_owned(),
                                seconds,
                                reason: None,
                            });
                            ui.close_menu();
                        }
                    }
                });

                ui.menu_button("Low trust", |ui| {
                    if ui.button("Monitor").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::SetSuspiciousUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                            restricted: false,
                        });
                        ui.close_menu();
                    }
                    if ui.button("Restrict").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::SetSuspiciousUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                            restricted: true,
                        });
                        ui.close_menu();
                    }
                    if ui.button("Unmonitor").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::ClearSuspiciousUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                        });
                        ui.close_menu();
                    }
                    if ui.button("Unrestrict").clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::ClearSuspiciousUser {
                            channel: self.channel.clone(),
                            login: msg.sender.login.clone(),
                            user_id: target_user_id.to_owned(),
                        });
                        ui.close_menu();
                    }
                });

                if !self.mod_action_presets.is_empty() {
                    ui.menu_button("Presets", |ui| {
                        for preset in self.mod_action_presets {
                            if ui.button(&preset.label).clicked() {
                                let command =
                                    preset.expand(&msg.sender.login, self.channel.display_name());
                                let _ = self.cmd_tx.try_send(AppCommand::SendMessage {
                                    channel: self.channel.clone(),
                                    text: command,
                                    reply_to_msg_id: None,
                                    reply: None,
                                });
                                ui.close_menu();
                            }
                        }
                    });
                }

                if ui.button("Ban user").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::BanUser {
                        channel: self.channel.clone(),
                        login: msg.sender.login.clone(),
                        user_id: target_user_id.to_owned(),
                        reason: None,
                    });
                    ui.close_menu();
                }

                if ui.button("Unban user").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::UnbanUser {
                        channel: self.channel.clone(),
                        login: msg.sender.login.clone(),
                        user_id: target_user_id.to_owned(),
                    });
                    ui.close_menu();
                }

                if ui.button("Hide user's messages locally").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::ClearUserMessagesLocally {
                        channel: self.channel.clone(),
                        login: msg.sender.login.clone(),
                    });
                    ui.close_menu();
                }
            } else {
                ui.add_enabled(false, egui::Button::new("Timeouts"));
                ui.add_enabled(false, egui::Button::new("Ban user"));
                ui.add_enabled(false, egui::Button::new("Unban user"));
            }

            let channel_login = self.channel.display_name().trim().to_ascii_lowercase();
            if !channel_login.is_empty() {
                ui.separator();
                ui.label(RichText::new("Workflows").small().color(t::text_muted()));

                if ui.button("Open mod view").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::OpenUrl {
                        url: format!("https://www.twitch.tv/moderator/{channel_login}/chat"),
                    });
                    ui.close_menu();
                }
                if ui.button("Open in-app moderation tools").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::OpenModerationTools {
                        channel: Some(self.channel.clone()),
                    });
                    ui.close_menu();
                }
                if ui.button("Refresh in-app unban requests").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::FetchUnbanRequests {
                        channel: self.channel.clone(),
                    });
                    let _ = self.cmd_tx.try_send(AppCommand::OpenModerationTools {
                        channel: Some(self.channel.clone()),
                    });
                    ui.close_menu();
                }
                if ui.button("Open AutoMod queue").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::OpenUrl {
                        url: format!(
                            "https://dashboard.twitch.tv/u/{channel_login}/settings/moderation/automod"
                        ),
                    });
                    ui.close_menu();
                }
                if ui.button("Open unban requests").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::OpenUrl {
                        url: format!(
                            "https://dashboard.twitch.tv/u/{channel_login}/community/unban-requests"
                        ),
                    });
                    ui.close_menu();
                }
                if ui.button("Open user logs (IVR)").clicked() {
                    let _ = self.cmd_tx.try_send(AppCommand::OpenUrl {
                        url: format!(
                            "https://logs.ivr.fi/?channel={channel_login}&username={}",
                            msg.sender.login
                        ),
                    });
                    ui.close_menu();
                }

                if matches!(msg.msg_kind, MsgKind::ChannelPointsReward { .. })
                    && ui.button("Open reward queue").clicked()
                {
                    let _ = self.cmd_tx.try_send(AppCommand::OpenUrl {
                        url: format!("https://www.twitch.tv/popout/{channel_login}/reward-queue"),
                    });
                    ui.close_menu();
                }
            }

            if let MsgKind::ChannelPointsReward {
                reward_id,
                redemption_id,
                status,
                user_login,
                reward_title,
                ..
            } = &msg.msg_kind
            {
                ui.separator();
                ui.label(RichText::new("Redemption").small().color(t::text_muted()));

                let can_update = Self::redemption_can_update(
                    reward_id.as_deref(),
                    redemption_id.as_deref(),
                    status.as_deref(),
                );
                let is_terminal = Self::redemption_is_terminal(status.as_deref());

                if can_update {
                    if ui.button("Mark fulfilled").clicked() {
                        let _ = self
                            .cmd_tx
                            .try_send(AppCommand::UpdateRewardRedemptionStatus {
                                channel: self.channel.clone(),
                                reward_id: reward_id.clone().unwrap_or_default(),
                                redemption_id: redemption_id.clone().unwrap_or_default(),
                                status: "FULFILLED".to_owned(),
                                user_login: user_login.clone(),
                                reward_title: reward_title.clone(),
                            });
                        ui.close_menu();
                    }

                    if ui.button("Reject redemption").clicked() {
                        let _ = self
                            .cmd_tx
                            .try_send(AppCommand::UpdateRewardRedemptionStatus {
                                channel: self.channel.clone(),
                                reward_id: reward_id.clone().unwrap_or_default(),
                                redemption_id: redemption_id.clone().unwrap_or_default(),
                                status: "CANCELED".to_owned(),
                                user_login: user_login.clone(),
                                reward_title: reward_title.clone(),
                            });
                        ui.close_menu();
                    }
                } else {
                    let reason = if is_terminal {
                        "This redemption is already final."
                    } else {
                        "Missing reward/redemption id from EventSub payload."
                    };
                    ui.add_enabled(false, egui::Button::new("Mark fulfilled"))
                        .on_hover_text(reason);
                    ui.add_enabled(false, egui::Button::new("Reject redemption"))
                        .on_hover_text(reason);
                }
            }
        }
    }

    /// If the snap-to-bottom flag is active, force the scroll offset to the
    /// current real maximum every frame until `stick_to_bottom` takes over.
    fn apply_snap(&self, ui: &mut Ui, output: &egui::scroll_area::ScrollAreaOutput<()>) {
        let snap_key = Id::new("snap_to_bottom").with(self.channel.as_str());
        let snapping: bool = ui.ctx().data_mut(|d| d.get_temp(snap_key).unwrap_or(false));
        if !snapping {
            return;
        }

        // If user has scrolled up, never keep forcing snap-to-bottom.
        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
        let scroll_paused: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
        if scroll_paused {
            ui.ctx().data_mut(|d| d.insert_temp(snap_key, false));
            return;
        }

        let viewport_h = output.inner_rect.height();
        let max_scroll = (output.content_size.y - viewport_h).max(0.0);
        let at_bottom = max_scroll < 1.0 || output.state.offset.y >= max_scroll - 20.0;

        if at_bottom {
            // stick_to_bottom has taken over; clear the flag.
            ui.ctx().data_mut(|d| d.insert_temp(snap_key, false));
        } else {
            // Re-write the true max every frame so new messages don't stall us.
            let mut state = output.state;
            state.offset.y = max_scroll;
            state.store(ui.ctx(), output.id);
            ui.ctx().request_repaint();
        }
    }

    /// Show the floating "Resume scrolling" button when the user has scrolled up.
    /// Also updates the per-channel `scroll_paused` flag used to gate `stick_to_bottom`.
    fn show_resume_button(
        &self,
        ui: &mut Ui,
        output: &egui::scroll_area::ScrollAreaOutput<()>,
        panel_rect: egui::Rect,
        paused_new_rows: usize,
    ) {
        fn clamp_rect_to_panel(rect: egui::Rect, panel: egui::Rect, pad: f32) -> egui::Rect {
            let max_w = (panel.width() - 2.0 * pad).max(1.0);
            let max_h = (panel.height() - 2.0 * pad).max(1.0);
            let size = egui::vec2(rect.width().min(max_w), rect.height().min(max_h));
            let x = rect
                .min
                .x
                .clamp(panel.min.x + pad, panel.max.x - pad - size.x);
            let y = rect
                .min
                .y
                .clamp(panel.min.y + pad, panel.max.y - pad - size.y);
            egui::Rect::from_min_size(egui::pos2(x, y), size)
        }

        let paused_key = egui::Id::new("scroll_paused").with(self.channel.as_str());
        let wheel_key = egui::Id::new("scroll_wheel_this_frame").with(self.channel.as_str());
        let prev_offset_key = egui::Id::new("scroll_prev_offset").with(self.channel.as_str());
        let viewport_h = output.inner_rect.height();
        let max_scroll = (output.content_size.y - viewport_h).max(0.0);
        let at_bottom = max_scroll < 1.0 || output.state.offset.y >= max_scroll - 20.0;
        let was_paused: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));
        let prev_offset: f32 = ui
            .ctx()
            .data_mut(|d| d.get_temp(prev_offset_key).unwrap_or(output.state.offset.y));
        let moved_up = output.state.offset.y + 2.0 < prev_offset;

        // Keep the paused flag in sync with where the scroll actually is.
        // When the wheel was used this frame, never clear the flag - the
        // scroll delta may not have been applied yet, so at_bottom could
        // still read as true even though the user just scrolled up.
        let wheel_this_frame: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(wheel_key).unwrap_or(false));
        let (drag_scrolling_up, pointer_over_panel_down) = {
            let over_panel = ui.ctx().input(|i| {
                i.pointer
                    .hover_pos()
                    .map(|p| panel_rect.contains(p))
                    .unwrap_or(false)
                    && i.pointer.primary_down()
            });
            (over_panel && moved_up, over_panel)
        };
        let pause_due_to_user_scroll = wheel_this_frame || drag_scrolling_up;

        // Detect if the user actively scrolled DOWN to the bottom this frame.
        // We only un-pause when there is positive evidence of user intent
        // (wheel or drag scroll that lands at the bottom), NOT merely because
        // `at_bottom` happened to become true due to content changes (e.g.
        // message eviction from the ring buffer shrinking total_h).
        let moved_down = output.state.offset.y > prev_offset + 2.0;
        let user_scrolled_to_bottom =
            at_bottom && moved_down && (wheel_this_frame || pointer_over_panel_down);

        let scroll_paused = if pause_due_to_user_scroll {
            true
        } else if user_scrolled_to_bottom {
            // User actively scrolled back down to the bottom.
            false
        } else if !was_paused {
            // Was not paused - stay unpaused (stick-to-bottom is active).
            // Transient !at_bottom from content growth is fine; egui's
            // stick_to_bottom will catch up next frame.
            false
        } else {
            // was_paused and no explicit un-pause action → stay paused.
            // This prevents content resizes from silently clearing the pause.
            true
        };
        ui.ctx()
            .data_mut(|d| d.insert_temp(paused_key, scroll_paused));
        ui.ctx()
            .data_mut(|d| d.insert_temp(prev_offset_key, output.state.offset.y));

        let show_resume_button = scroll_paused || !at_bottom;
        let resume_btn_rect = if show_resume_button {
            let btn_label = "Resume scrolling";
            let estimated_btn_w = (btn_label.chars().count() as f32 * 7.0 + 34.0)
                .clamp(140.0, (panel_rect.width() - 16.0).max(120.0));
            let btn_size = egui::vec2(estimated_btn_w, 28.0);
            let btn_center = egui::pos2(panel_rect.center().x, panel_rect.bottom() - 36.0);
            let rect = egui::Rect::from_center_size(btn_center, btn_size);
            Some(clamp_rect_to_panel(rect, panel_rect, 8.0))
        } else {
            None
        };

        if scroll_paused {
            let raw_text = if paused_new_rows > 0 {
                format!("Paused - {paused_new_rows} new")
            } else {
                "Paused".to_owned()
            };
            let estimated_badge_w = (raw_text.chars().count() as f32 * 6.8 + 24.0)
                .clamp(92.0, (panel_rect.width() - 16.0).max(92.0));
            let badge_size = egui::vec2(estimated_badge_w, 24.0);
            let paused_rect = if let Some(btn_rect) = resume_btn_rect {
                let x = btn_rect.center().x - badge_size.x * 0.5;
                let y = (btn_rect.min.y - 8.0 - badge_size.y).max(panel_rect.top() + 8.0);
                clamp_rect_to_panel(
                    egui::Rect::from_min_size(egui::pos2(x, y), badge_size),
                    panel_rect,
                    8.0,
                )
            } else {
                clamp_rect_to_panel(
                    egui::Rect::from_min_size(
                        panel_rect.left_top() + egui::vec2(8.0, 8.0),
                        badge_size,
                    ),
                    panel_rect,
                    8.0,
                )
            };
            let painter = ui.painter().with_clip_rect(panel_rect);
            painter.rect_filled(paused_rect, 6.0, t::alpha(Color32::BLACK, 160));
            let max_chars = (((paused_rect.width() - 16.0) / 6.8).floor() as usize).max(4);
            let text = if raw_text.chars().count() > max_chars {
                let mut trimmed: String =
                    raw_text.chars().take(max_chars.saturating_sub(1)).collect();
                trimmed.push('…');
                trimmed
            } else {
                raw_text
            };
            painter.text(
                paused_rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(12.0),
                t::text_primary(),
            );
        }

        if let Some(btn_rect) = resume_btn_rect {
            let painter = ui.painter().with_clip_rect(panel_rect);

            // Button background
            painter.rect_filled(btn_rect, 8.0, t::accent_dim());
            // Subtle border for definition
            painter.rect_stroke(
                btn_rect,
                8.0,
                egui::Stroke::new(1.0, t::accent()),
                egui::epaint::StrokeKind::Outside,
            );
            // Button label
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Resume scrolling",
                egui::FontId::proportional(12.0),
                t::text_on_accent(),
            );

            // Detect click on the painted rect
            let btn_response = ui.interact(
                btn_rect,
                Id::new("resume_scroll_btn").with(self.channel.as_str()),
                egui::Sense::click(),
            );
            if btn_response.clicked() {
                // Clear paused so stick_to_bottom re-engages next frame.
                ui.ctx().data_mut(|d| d.insert_temp(paused_key, false));
                // Immediately write the real max_scroll to the correct egui
                // scroll-state key (output.id, not the salt), and set the
                // snap flag so apply_snap keeps rewriting every frame until
                // stick_to_bottom confirms we are at the bottom.
                let mut state = output.state;
                state.offset.y = max_scroll;
                state.store(ui.ctx(), output.id);
                let snap_key = Id::new("snap_to_bottom").with(self.channel.as_str());
                ui.ctx().data_mut(|d| d.insert_temp(snap_key, true));
                ui.ctx().request_repaint();
            }
        }
    }

    fn render_message(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        dimmed: bool,
        rctx: &RenderCtx,
        static_frames: &mut HashMap<String, StaticFrameCacheEntry>,
        user_color_cache: &mut HashMap<String, Color32>,
    ) {
        // Dispatch non-chat (and non-bits) events to the compact system-event renderer.
        match &msg.msg_kind {
            MsgKind::Chat | MsgKind::Bits { .. } => {}
            MsgKind::SuspiciousUserMessage => {
                self.render_suspicious_user_message(
                    ui,
                    msg,
                    dimmed,
                    rctx,
                    static_frames,
                    user_color_cache,
                );
                return;
            }
            _ => {
                self.render_system_event(ui, msg, static_frames);
                return;
            }
        }

        let reply_key = rctx.reply_key;
        let scroll_to_key = rctx.scroll_to_key;

        // -- Message background ------------------------------------------
        // Use pre-computed highlight state from RenderCtx instead of per-
        // message data_mut lookups.
        let highlight_alpha: f32 = match (&rctx.highlight_server_id, msg.server_id.as_deref()) {
            (Some(hl_id), Some(msg_id)) if hl_id == msg_id => rctx.highlight_alpha,
            _ => 0.0,
        };
        let keyword_highlight_match = self.message_keyword_highlight(msg);
        let keyword_highlight = keyword_highlight_match.is_some();
        let keyword_highlight_color = keyword_highlight_match.and_then(|m| m.color);
        let bg = message_row_background(
            &msg.flags,
            &msg.msg_kind,
            highlight_alpha,
            keyword_highlight,
            keyword_highlight_color,
        );

        // Context-menu approach:
        //
        // The Frame is registered FIRST on the layer (before any inner
        // widgets), so it has the LOWEST hit-test priority.  After
        // Frame::end(), we augment the frame's response with Sense::click()
        // via Response::interact().  Egui OR's the Sense and updates the
        // widget in-place (same index).  Result: inner widgets (username,
        // URL links) still win primary/secondary clicks in their rects,
        // but right-clicks on the message body (text, emotes, empty space)
        // fall through to the frame and open the context menu.

        // Push a stable ID derived from the message's own identifier so that
        // every inner widget (username label, badge images, emote images, URL
        // links) keeps the same egui widget-ID across frames.  Without this,
        // virtual-scrolling shifts the auto-ID counter whenever new messages
        // arrive and the dead-space allocation above the visible window
        // changes, causing click-press and click-release to see different IDs
        // and silently dropping the click event.
        ui.push_id(msg.id.0, |ui| {
            let mut prepared = egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
                .begin(ui);
            // Register a background click sensor EARLY - before any inner widgets -
            // so it gets the lowest idx_in_layer and thus the lowest hit-test
            // priority.  Inner widgets (reply header, username label, emotes, URLs)
            // are registered afterwards and therefore *win* the hit test when the
            // pointer is over them.  Only clicks on "empty" message space (padding,
            // gaps) fall through to this background widget to open the context menu.
            //
            // After Frame::end() we re-register with the SAME id and the actual
            // frame rect; `WidgetRects::insert` updates the rect in-place while
            // keeping the original (low) idx_in_layer.
            let bg_click_id = Id::new("msg_bg_click").with(msg.id.0);
            {
                let ui = &mut prepared.content_ui;
                // Use a zero-size rect for the early placeholder so that the
                // second interact() (with the real frame rect) doesn't trigger
                // egui's ID-clash warning.  The zero rect is fully contained
                // within the final frame rect, so `check_for_id_clash` treats
                // them as the same widget.  The key property we care about -
                // low `idx_in_layer` for hit-test priority - is preserved
                // because the widget is still registered before inner widgets.
                let placeholder_rect =
                    egui::Rect::from_min_size(ui.max_rect().left_top(), egui::Vec2::ZERO);
                ui.interact(placeholder_rect, bg_click_id, egui::Sense::click());

                // Keep selectable_labels off globally so timestamp / badge
                // chip / separator labels stay non-interactive.  Text spans
                // opt-in to selection via `.selectable(true)` and each has
                // its own `.context_menu()` so right-click → Reply still works
                // even when the label wins the hit test.
                ui.style_mut().interaction.selectable_labels = false;

                if dimmed {
                    ui.set_opacity(if msg.flags.is_history { 0.35 } else { 0.45 });
                }

                // History messages are rendered at reduced opacity so they
                // read as older context while still being fully legible.
                if msg.flags.is_history {
                    ui.set_opacity(if dimmed { 0.35 } else { 0.55 });
                }
                if let Some(ref rep) = msg.reply {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        // Accent left stripe
                        let (stripe, _) =
                            ui.allocate_exact_size(egui::vec2(2.0, 12.0), egui::Sense::hover());
                        ui.painter().rect_filled(stripe, 0.0, t::accent());
                        let body = if rep.parent_msg_body.chars().count() > 80 {
                            // Find the byte offset of the 80th char boundary.
                            let cut = rep
                                .parent_msg_body
                                .char_indices()
                                .nth(80)
                                .map(|(i, _)| i)
                                .unwrap_or(rep.parent_msg_body.len());
                            format!("{}…", &rep.parent_msg_body[..cut])
                        } else {
                            rep.parent_msg_body.clone()
                        };
                        let reply_color = t::text_secondary();
                        let h = ui.add(
                            Label::new(
                                RichText::new(format!("↩ @{}: {}", rep.parent_display_name, body))
                                    .font(t::small())
                                    .color(reply_color)
                                    .italics(),
                            )
                            .sense(egui::Sense::click())
                            .truncate(),
                        );
                        h.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
                        if h.clicked() {
                            let target = rep.parent_msg_id.trim().to_owned();
                            let hint = ReplyScrollHint {
                                parent_user_login: rep.parent_user_login.clone(),
                                parent_msg_body: rep.parent_msg_body.clone(),
                                child_msg_local_id: msg.id.0,
                            };
                            ui.ctx().data_mut(|d| {
                                if !target.is_empty() {
                                    d.insert_temp(scroll_to_key, target);
                                }
                                d.insert_temp(rctx.scroll_hint_key, hint);
                            });
                        }
                        if h.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                    });
                }
                // -- Notification banner (pinned / first / highlighted / rewards) --
                // Rendered inside the Frame so the background fill covers
                // the banner as well, and the interaction rect is contiguous.
                if let Some((label, stripe_color)) = notification_label(
                    &msg.flags,
                    &msg.msg_kind,
                    keyword_highlight,
                    keyword_highlight_color,
                ) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // Colored left stripe
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 1.0, stripe_color);
                        ui.add(Label::new(
                            RichText::new(label).font(t::small()).color(stripe_color),
                        ));
                    });
                }

                // Center-align all items vertically so images don't sit above text baseline.
                // Use allocate_ui_with_layout with a constrained height hint
                // (one emote row) instead of with_layout, because Align::Center
                // in a horizontal layout causes egui to expand frame_size.y to
                // fill the full available height - which for the first message
                // in a ScrollArea means the entire viewport, creating huge gaps.
                let wrap_width = ui.available_width();
                ui.allocate_ui_with_layout(
                    egui::vec2(wrap_width, EMOTE_SIZE),
                    egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                    |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 1.0);

                        if self.show_timestamps {
                            // Timestamp
                            let ts = format_message_timestamp(
                                &msg.timestamp,
                                self.show_timestamp_seconds,
                                self.use_24h_timestamps,
                            );
                            ui.add(Label::new(
                                RichText::new(ts).color(t::timestamp()).font(t::small()),
                            ));

                            // Separator dot between timestamp and badges/name
                            ui.add(Label::new(
                                RichText::new("·").color(t::separator()).font(t::small()),
                            ));
                        }

                        // Badges: image if loaded, else text fallback
                        for badge in &msg.sender.badges {
                            let tooltip_label = pretty_badge_name(&badge.name, &badge.version);
                            if let Some(url) = &badge.url {
                                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                                    let size = fit_size(w, h, BADGE_SIZE);
                                    let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
                                    let url_key = super::bytes_uri(url, raw);
                                    // Closures capture by reference - clones
                                    // only happen when the tooltip is actually
                                    // shown (on hover), not every frame.
                                    self.show_image(
                                        ui,
                                        &url_key,
                                        raw,
                                        size,
                                        Some(url.as_str()),
                                        static_frames,
                                    )
                                    .on_hover_ui_at_pointer(|ui| {
                                        ui.set_max_width(200.0);
                                        ui.vertical_centered(|ui| {
                                            ui.add(
                                                egui::Image::from_bytes(
                                                    url_key.clone(),
                                                    egui::load::Bytes::Shared(raw.clone()),
                                                )
                                                .fit_to_exact_size(tooltip_size),
                                            );
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(&tooltip_label).strong());
                                        });
                                    })
                                    .context_menu(|ui| {
                                        self.show_message_context_menu(ui, msg, reply_key);
                                    });
                                    continue;
                                }
                            }
                            render_badge_fallback(ui, &badge.name, &badge.version, &tooltip_label);
                        }

                        // Sender name - clickable to open user profile card.
                        let name_color = resolve_sender_color(msg, user_color_cache);
                        let name = if msg.flags.is_action {
                            format!("* {}", msg.sender.display_name)
                        } else {
                            msg.sender.display_name.clone()
                        };
                        let name_resp = show_sender_name(
                            ui,
                            msg.id.0,
                            &name,
                            name_color,
                            None,
                            self.emote_bytes,
                            self.cmd_tx,
                        )
                        .on_hover_ui(|ui| {
                            ui.label(format!("@{}", msg.sender.login));
                        });
                        name_resp
                            .context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
                        if name_resp.clicked_by(egui::PointerButton::Primary) {
                            // Clone only when clicked - not every frame.
                            let _ = self.cmd_tx.try_send(AppCommand::ShowUserCard {
                                login: msg.sender.login.clone(),
                                channel: self.channel.clone(),
                            });
                            let key = Id::new("ml_profile_req").with(self.channel.as_str());
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(
                                    key,
                                    (msg.sender.login.clone(), msg.sender.badges.clone()),
                                );
                            });
                        }
                        if name_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        // Colon separator after name (not shown for /me actions)
                        if !msg.flags.is_action {
                            ui.add(Label::new(RichText::new(":").color(t::separator())));
                        }

                        if let Some(reward_id) = msg
                            .flags
                            .custom_reward_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            let chip_bg = t::alpha(t::accent(), 52);
                            let chip_stroke = t::alpha(t::accent_dim(), 200);
                            let chip = egui::Frame::new()
                                .fill(chip_bg)
                                .stroke(egui::Stroke::new(1.0, chip_stroke))
                                .corner_radius(egui::CornerRadius::same(3))
                                .inner_margin(egui::Margin::symmetric(5, 1))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("POINTS")
                                            .font(t::tiny())
                                            .color(t::text_on_accent())
                                            .strong(),
                                    );
                                })
                                .response;
                            chip.on_hover_text(format!(
                                "Sent via channel points reward\nReward ID: {reward_id}"
                            ));
                        }

                        // Message spans carry their own whitespace from the
                        // tokenizer, so keep inter-widget spacing at zero to
                        // avoid rendering words with visually doubled spaces.
                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let collapsed_body = if self.collapse_long_messages {
                                collapse_message_for_display(
                                    &msg.raw_text,
                                    self.collapse_long_message_lines,
                                )
                            } else {
                                None
                            };

                            // For deleted messages show the original content
                            // with strikethrough so moderator actions are
                            // visible without being prominent.
                            if msg.flags.is_deleted {
                                let deleted_text =
                                    collapsed_body.as_deref().unwrap_or(msg.raw_text.as_str());
                                ui.add(
                                    Label::new(
                                        RichText::new(format!("✂ {}", deleted_text))
                                            .strikethrough()
                                            .italics()
                                            .color(t::text_muted()),
                                    )
                                    .wrap(),
                                );
                            } else if let Some(collapsed_text) = collapsed_body {
                                // Long-message collapse:
                                // render a compact text summary instead of
                                // laying out all original spans/emotes.
                                let rich = if msg.flags.is_action {
                                    RichText::new(collapsed_text).italics()
                                } else {
                                    RichText::new(collapsed_text)
                                };
                                let resp = ui.add(Label::new(rich).wrap().selectable(true));
                                resp.context_menu(|ui| {
                                    self.show_message_context_menu(ui, msg, reply_key)
                                });
                                resp.on_hover_ui(|ui| {
                                    ui.label(
                                        RichText::new("Long message collapsed for performance")
                                            .font(t::small())
                                            .color(t::text_muted()),
                                    );
                                });
                            } else {
                                for span in &msg.spans {
                                    self.render_span(
                                        ui,
                                        span,
                                        msg.flags.is_action,
                                        msg,
                                        reply_key,
                                        static_frames,
                                    );
                                }
                            }
                        });
                    },
                );
            }
            let msg_frame_resp = prepared.end(ui);

            // Re-register the background click widget with the actual frame rect.
            // Same `bg_click_id` → updates in-place, keeping low hit-test priority.
            let bg_click = ui.interact(msg_frame_resp.rect, bg_click_id, egui::Sense::click());
            bg_click.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));

            // Left accent strip for mentions and highlights - a vivid 3 px bar on
            // the left edge of the row so the eye finds them instantly in fast chat.
            if let Some(bar_col) = message_left_accent_color(
                &msg.flags,
                &msg.msg_kind,
                keyword_highlight,
                keyword_highlight_color,
            ) {
                let r = msg_frame_resp.rect;
                let strip = egui::Rect::from_min_size(r.left_top(), egui::vec2(3.0, r.height()));
                ui.painter().rect_filled(strip, 0.0, bar_col);
            }
        }); // end push_id
    }

    fn message_keyword_highlight(&self, msg: &ChatMessage) -> Option<KeywordHighlightMatch> {
        if self.highlight_rules.is_empty() {
            return None;
        }
        if msg.raw_text.is_empty() {
            return None;
        }
        crust_core::highlight::first_match_context(
            self.highlight_rules,
            &msg.raw_text,
            &msg.sender.login,
            &msg.sender.display_name,
            self.channel.display_name(),
            msg.flags.is_mention,
        )
        .map(
            |(color, _show_in_mentions, _has_alert, _has_sound)| KeywordHighlightMatch {
                color: color.map(|[r, g, b]| Color32::from_rgb(r, g, b)),
            },
        )
    }

    /// Render a compact system-event row (mod action, sub alert, raid, notice).
    /// These rows are centered italic lines with a colored left stripe and icon.
    fn render_system_event(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        static_frames: &mut HashMap<String, StaticFrameCacheEntry>,
    ) {
        let automod_row = msg.sender.login.eq_ignore_ascii_case("automod")
            && msg.raw_text.starts_with("AutoMod:");
        let suspicious_header =
            msg.sender.login.is_empty() && msg.raw_text.starts_with("Suspicious User:");

        if automod_row || suspicious_header {
            let accent = t::red();
            let title = if suspicious_header {
                "Suspicious User:"
            } else {
                "AutoMod:"
            };
            let body = msg
                .raw_text
                .strip_prefix(title)
                .unwrap_or(&msg.raw_text)
                .trim_start();

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 1.0, accent);

                for badge in &msg.sender.badges {
                    let tooltip_label = pretty_badge_name(&badge.name, &badge.version);
                    if let Some(url) = &badge.url {
                        if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                            let size = fit_size(w, h, BADGE_SIZE);
                            let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
                            let url_key = super::bytes_uri(url, raw);
                            self.show_image(
                                ui,
                                &url_key,
                                raw,
                                size,
                                Some(url.as_str()),
                                static_frames,
                            )
                            .on_hover_ui_at_pointer(|ui| {
                                ui.set_max_width(200.0);
                                ui.vertical_centered(|ui| {
                                    ui.add(
                                        egui::Image::from_bytes(
                                            url_key.clone(),
                                            egui::load::Bytes::Shared(raw.clone()),
                                        )
                                        .fit_to_exact_size(tooltip_size),
                                    );
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(&tooltip_label).strong());
                                });
                            });
                            continue;
                        }
                    }
                    render_badge_fallback(ui, &badge.name, &badge.version, &tooltip_label);
                }

                ui.add(Label::new(
                    RichText::new(title).font(t::small()).color(accent).strong(),
                ));
                if !body.is_empty() {
                    ui.add(Label::new(
                        RichText::new(body)
                            .font(t::small())
                            .color(t::text_primary()),
                    ));
                }
            });
            return;
        }

        let (accent, label_override): (Color32, Option<String>) = match &msg.msg_kind {
            MsgKind::Sub {
                display_name,
                months,
                plan,
                is_gift,
                ..
            } => {
                let gifted_to_me = *is_gift && msg.flags.is_mention;
                let text = if gifted_to_me {
                    format!("🎉🎊  You received a gifted {plan} sub! ({months} months)")
                } else if *is_gift {
                    format!("🎁  {display_name} received a gifted {plan} sub! ({months} months)")
                } else if *months <= 1 {
                    format!("⭐  {display_name} subscribed with {plan}!")
                } else {
                    format!("⭐  {display_name} resubscribed with {plan}! ({months} months)")
                };
                (
                    if gifted_to_me {
                        t::raid_cyan()
                    } else {
                        t::gold()
                    },
                    Some(text),
                )
            }
            MsgKind::Raid {
                display_name,
                viewer_count,
            } => (
                t::raid_cyan(),
                Some(format!(
                    "🎉  {display_name} is raiding with {viewer_count} viewers!"
                )),
            ),
            MsgKind::Timeout { login, seconds } => {
                let dur = if *seconds < 60 {
                    format!("{seconds}s")
                } else if *seconds < 3600 {
                    format!("{}m", seconds / 60)
                } else {
                    format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
                };
                (
                    t::yellow(),
                    Some(format!("⏱  {login} was timed out for {dur}.")),
                )
            }
            MsgKind::Ban { login } => (
                t::red(),
                Some(format!("🚫  {login} was permanently banned.")),
            ),
            MsgKind::ChatCleared => (
                t::text_secondary(),
                Some("🗑  Chat was cleared by a moderator.".to_owned()),
            ),
            MsgKind::SystemInfo => {
                let (color, text) = style_system_info_text(&msg.raw_text);
                (color, Some(text))
            }
            MsgKind::ChannelPointsReward {
                user_login,
                reward_title,
                cost,
                status,
                ..
            } => {
                let (status_label, _) = redemption_status_presentation(status.as_deref());
                let text = format!(
                    "🎟  {user_login} redeemed '{reward_title}' ({cost} points) [{status_label}]"
                );
                (redemption_accent(status.as_deref()), Some(text))
            }
            _ => (t::text_secondary(), Some(msg.raw_text.clone())),
        };

        let text = label_override.unwrap_or_else(|| msg.raw_text.clone());
        let opacity = if msg.flags.is_history { 0.55 } else { 1.0 };

        // Push a stable ID derived from the message's own identifier so that
        // widget IDs inside the system-event row are stable regardless of
        // where this message falls in the virtual-scroll window.  Without
        // this, the auto-incremented IDs shift every frame as the visible
        // range moves, causing egui to report widget ID clashes for every
        // system event in the loaded history.
        ui.push_id(msg.id.0, |ui| {
            egui::Frame::new()
                .fill(t::alpha(accent, 10))
                .inner_margin(egui::Margin::symmetric(
                    ROW_PAD_X as i8,
                    ROW_PAD_Y as i8 + 1,
                ))
                .show(ui, |ui| {
                    if msg.flags.is_history {
                        ui.set_opacity(opacity);
                    }
                    if let MsgKind::ChannelPointsReward {
                        user_login,
                        reward_title,
                        cost,
                        reward_id,
                        redemption_id,
                        user_input,
                        status,
                    } = &msg.msg_kind
                    {
                        let (status_label, status_color) =
                            redemption_status_presentation(status.as_deref());

                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(3.0, 14.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(rect, 1.0, accent);

                                if self.show_timestamps {
                                    let ts = format_message_timestamp(
                                        &msg.timestamp,
                                        self.show_timestamp_seconds,
                                        self.use_24h_timestamps,
                                    );
                                    ui.add(Label::new(
                                        RichText::new(ts).color(t::timestamp()).font(t::small()),
                                    ));
                                }

                                ui.add(
                                    Label::new(
                                        RichText::new(format!(
                                            "🎟 {user_login} redeemed '{reward_title}'"
                                        ))
                                        .color(accent)
                                        .font(t::small()),
                                    )
                                    .wrap(),
                                );

                                let cost_chip = format!("{cost} pts");
                                let cost_response = egui::Frame::new()
                                    .fill(t::alpha(t::accent(), 48))
                                    .corner_radius(egui::CornerRadius::same(3))
                                    .inner_margin(egui::Margin::symmetric(4, 1))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(cost_chip)
                                                .font(t::tiny())
                                                .color(t::text_on_accent())
                                                .strong(),
                                        );
                                    })
                                    .response;
                                cost_response.on_hover_text("Channel points cost");

                                egui::Frame::new()
                                    .fill(t::alpha(status_color, 50))
                                    .corner_radius(egui::CornerRadius::same(3))
                                    .inner_margin(egui::Margin::symmetric(4, 1))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(status_label.as_ref())
                                                .font(t::tiny())
                                                .color(status_color)
                                                .strong(),
                                        );
                                    });

                                if self.can_moderate {
                                    let can_update = Self::redemption_can_update(
                                        reward_id.as_deref(),
                                        redemption_id.as_deref(),
                                        status.as_deref(),
                                    );
                                    if can_update {
                                        ui.add_space(4.0);
                                        if ui.small_button("Approve").clicked() {
                                            let _ = self.cmd_tx.try_send(
                                                AppCommand::UpdateRewardRedemptionStatus {
                                                    channel: self.channel.clone(),
                                                    reward_id: reward_id
                                                        .as_ref()
                                                        .cloned()
                                                        .unwrap_or_default(),
                                                    redemption_id: redemption_id
                                                        .as_ref()
                                                        .cloned()
                                                        .unwrap_or_default(),
                                                    status: "FULFILLED".to_owned(),
                                                    user_login: user_login.clone(),
                                                    reward_title: reward_title.clone(),
                                                },
                                            );
                                        }
                                        if ui.small_button("Reject").clicked() {
                                            let _ = self.cmd_tx.try_send(
                                                AppCommand::UpdateRewardRedemptionStatus {
                                                    channel: self.channel.clone(),
                                                    reward_id: reward_id
                                                        .as_ref()
                                                        .cloned()
                                                        .unwrap_or_default(),
                                                    redemption_id: redemption_id
                                                        .as_ref()
                                                        .cloned()
                                                        .unwrap_or_default(),
                                                    status: "CANCELED".to_owned(),
                                                    user_login: user_login.clone(),
                                                    reward_title: reward_title.clone(),
                                                },
                                            );
                                        }
                                    }
                                }
                            });

                            if let Some(input) = user_input
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    ui.label(
                                        RichText::new(format!("Message: {input}"))
                                            .font(t::small())
                                            .color(t::text_muted())
                                            .italics(),
                                    );
                                });
                            }
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            // Coloured left stripe
                            let (rect, _) =
                                ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 1.0, accent);

                            if self.show_timestamps {
                                // Timestamp
                                let ts = format_message_timestamp(
                                    &msg.timestamp,
                                    self.show_timestamp_seconds,
                                    self.use_24h_timestamps,
                                );
                                ui.add(Label::new(
                                    RichText::new(ts).color(t::timestamp()).font(t::small()),
                                ));
                            }

                            // Message text
                            let rich = if is_irc_motd_line(&text) {
                                RichText::new(text).color(accent).font(t::small())
                            } else {
                                RichText::new(text).italics().color(accent).font(t::small())
                            };
                            ui.add(Label::new(rich).wrap());
                        });
                    }
                });
        }); // end push_id
    }

    fn render_span(
        &self,
        ui: &mut Ui,
        span: &Span,
        is_action: bool,
        msg: &ChatMessage,
        reply_key: Id,
        static_frames: &mut HashMap<String, StaticFrameCacheEntry>,
    ) {
        let action_color = t::text_secondary();
        match span {
            Span::Text { text, .. } => {
                let cleaned = strip_invisible_chars(text);
                if cleaned.is_empty() {
                    return;
                }
                let rt = if is_action {
                    RichText::new(&*cleaned).italics().color(action_color)
                } else {
                    RichText::new(&*cleaned)
                };
                let resp = ui.add(Label::new(rt).wrap().selectable(true));
                resp.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
            }
            Span::Emote {
                url,
                url_hd,
                code,
                provider,
                ..
            } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let url_key = super::bytes_uri(url, raw);

                    // Capture shared references - string/Arc clones only
                    // happen when the tooltip is actually shown (on hover).
                    let emote_bytes = self.emote_bytes; // &HashMap - Copy
                    let cmd_tx = self.cmd_tx; // &Sender  - Copy

                    self.show_image(ui, &url_key, raw, size, Some(url.as_str()), static_frames)
                        .on_hover_ui_at_pointer(|ui| {
                            // Check HD availability at hover time, not every frame.
                            let hd_entry = url_hd.as_deref().and_then(|u| emote_bytes.get(u));

                            // Fire HD fetch once on first hover if not yet loaded.
                            if hd_entry.is_none() {
                                if let Some(hd_url) = url_hd.as_deref() {
                                    let _ = cmd_tx.try_send(AppCommand::FetchImage {
                                        url: hd_url.to_owned(),
                                    });
                                }
                            }

                            let (tt_key, tt_raw, tt_w, tt_h) = match hd_entry {
                                Some(&(hw, hh, ref href)) => (
                                    super::bytes_uri(url_hd.as_deref().unwrap(), href),
                                    href.clone(),
                                    hw,
                                    hh,
                                ),
                                None => (url_key.clone(), raw.clone(), w, h),
                            };
                            let tt_size = fit_size(tt_w, tt_h, TOOLTIP_EMOTE_SIZE);

                            ui.set_max_width(280.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                                    egui::Image::from_bytes(
                                        tt_key,
                                        egui::load::Bytes::Shared(tt_raw),
                                    )
                                    .fit_to_exact_size(tt_size),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(code.as_str()).strong());
                                ui.label(
                                    RichText::new(provider_label(provider))
                                        .small()
                                        .color(t::text_secondary()),
                                );
                            });
                        })
                        .context_menu(|ui| {
                            self.show_message_context_menu(ui, msg, reply_key);
                        });
                } else {
                    // Image not yet loaded - show text code as placeholder
                    ui.add(Label::new(
                        RichText::new(code)
                            .italics()
                            .font(t::small())
                            .color(t::green()),
                    ));
                }
            }
            Span::Emoji { text, url } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let tooltip_size = fit_size(w, h, TOOLTIP_EMOTE_SIZE);
                    let url_key = super::bytes_uri(url, raw);
                    self.show_image(ui, &url_key, raw, size, Some(url.as_str()), static_frames)
                        .on_hover_ui_at_pointer(|ui| {
                            ui.set_max_width(200.0);
                            ui.vertical_centered(|ui| {
                                ui.add(
                                    egui::Image::from_bytes(
                                        url_key.clone(),
                                        egui::load::Bytes::Shared(raw.clone()),
                                    )
                                    .fit_to_exact_size(tooltip_size),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(text.as_str()).strong());
                                ui.label(
                                    RichText::new("Twemoji").small().color(t::text_secondary()),
                                );
                            });
                        })
                        .context_menu(|ui| {
                            self.show_message_context_menu(ui, msg, reply_key);
                        });
                } else {
                    ui.add(Label::new(RichText::new(text)));
                }
            }
            Span::Mention { login } => {
                let resp = ui.add(
                    Label::new(
                        RichText::new(format!("@{login}"))
                            .color(t::mention())
                            .strong(),
                    )
                    .selectable(true),
                );
                resp.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
            }
            Span::Url { text, url } => {
                let cmd_tx = self.cmd_tx;
                let link_previews = self.link_previews;
                let emote_bytes = self.emote_bytes;

                // Render as a clickable hyperlink-style label.
                let resp = ui.add(
                    Label::new(RichText::new(text).color(t::link()).underline())
                        .selectable(false)
                        .sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    let _ = cmd_tx.try_send(AppCommand::OpenUrl { url: url.clone() });
                }
                resp.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
                resp.on_hover_ui(|ui| {
                    // Fire preview fetch on first hover (idempotent in reducer).
                    let preview = link_previews.get(url.as_str());
                    if preview.map(|p| p.fetched).unwrap_or(false) == false {
                        let _ = cmd_tx.try_send(AppCommand::FetchLinkPreview { url: url.clone() });
                    }

                    ui.set_max_width(320.0);
                    ui.vertical(|ui| {
                        match preview {
                            None => {
                                // Not yet fetched - show hostname + spinner.
                                let host = url_hostname(url);
                                ui.label(RichText::new(host).small().color(t::link()));
                                ui.label(
                                    RichText::new("Loading preview…")
                                        .small()
                                        .italics()
                                        .color(t::text_secondary()),
                                );
                            }
                            Some(p) => {
                                // Site name badge (YouTube, Twitter, etc.)
                                if let Some(ref sn) = p.site_name {
                                    let (badge_bg, badge_fg) = site_badge_colors(sn);
                                    egui::Frame::new()
                                        .fill(badge_bg)
                                        .corner_radius(egui::CornerRadius::same(3))
                                        .inner_margin(egui::Margin::symmetric(5, 1))
                                        .show(ui, |ui| {
                                            ui.label(
                                                RichText::new(sn).small().strong().color(badge_fg),
                                            );
                                        });
                                    ui.add_space(3.0);
                                }
                                // Thumbnail
                                if let Some(ref thumb) = p.thumbnail_url {
                                    if let Some(&(w, h, ref raw)) = emote_bytes.get(thumb.as_str())
                                    {
                                        let scale = (170.0_f32 / h as f32).min(300.0 / w as f32);
                                        let size = Vec2::new(w as f32 * scale, h as f32 * scale);
                                        let key = super::bytes_uri(thumb, raw);
                                        egui::Frame::new()
                                            .corner_radius(egui::CornerRadius::same(4))
                                            .show(ui, |ui| {
                                                self.show_image(
                                                    ui,
                                                    &key,
                                                    raw,
                                                    size,
                                                    Some(thumb.as_str()),
                                                    static_frames,
                                                );
                                            });
                                        ui.add_space(4.0);
                                    }
                                }
                                // Title
                                if let Some(ref title) = p.title {
                                    ui.add(Label::new(RichText::new(title).strong()).wrap());
                                }
                                // Description
                                if let Some(ref d) = p.description {
                                    let snippet = if d.chars().count() > 260 {
                                        let cut = d
                                            .char_indices()
                                            .nth(260)
                                            .map(|(i, _)| i)
                                            .unwrap_or(d.len());
                                        format!("{}\u{2026}", &d[..cut])
                                    } else {
                                        d.clone()
                                    };
                                    ui.add(
                                        Label::new(
                                            RichText::new(snippet).small().color(t::text_muted()),
                                        )
                                        .wrap(),
                                    );
                                }
                                if p.title.is_none()
                                    && p.description.is_none()
                                    && p.thumbnail_url.is_none()
                                {
                                    ui.label(
                                        RichText::new("No preview available")
                                            .small()
                                            .italics()
                                            .color(t::text_secondary()),
                                    );
                                }
                                // Domain footer
                                let host = url_hostname(url);
                                ui.add_space(2.0);
                                ui.label(RichText::new(host).small().color(t::text_muted()));
                            }
                        }
                    });
                });
            }
            Span::Badge { name, .. } => {
                let tooltip = pretty_badge_name(name, "1");
                render_badge_fallback(ui, name, "1", &tooltip);
            }
        }
    }

    fn render_suspicious_user_message(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        dimmed: bool,
        rctx: &RenderCtx,
        static_frames: &mut HashMap<String, StaticFrameCacheEntry>,
        user_color_cache: &mut HashMap<String, Color32>,
    ) {
        let reply_key = rctx.reply_key;
        let highlight_alpha: f32 = match (&rctx.highlight_server_id, msg.server_id.as_deref()) {
            (Some(hl_id), Some(msg_id)) if hl_id == msg_id => rctx.highlight_alpha,
            _ => 0.0,
        };
        let keyword_highlight_match = self.message_keyword_highlight(msg);
        let keyword_highlight = keyword_highlight_match.is_some();
        let keyword_highlight_color = keyword_highlight_match.and_then(|m| m.color);
        let bg = message_row_background(
            &msg.flags,
            &msg.msg_kind,
            highlight_alpha,
            keyword_highlight,
            keyword_highlight_color,
        );

        ui.push_id(msg.id.0, |ui| {
            let mut prepared = egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
                .begin(ui);

            let bg_click_id = Id::new("msg_bg_click").with(msg.id.0);
            {
                let ui = &mut prepared.content_ui;
                let placeholder_rect =
                    egui::Rect::from_min_size(ui.max_rect().left_top(), egui::Vec2::ZERO);
                ui.interact(placeholder_rect, bg_click_id, egui::Sense::click());
                ui.style_mut().interaction.selectable_labels = false;

                if dimmed {
                    ui.set_opacity(if msg.flags.is_history { 0.35 } else { 0.45 });
                }
                if msg.flags.is_history {
                    ui.set_opacity(if dimmed { 0.35 } else { 0.55 });
                }

                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), EMOTE_SIZE),
                    egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                    |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 1.0);

                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 1.0, t::red());

                        let channel_name = format!("#{}", msg.channel.display_name());
                        ui.add(Label::new(
                            RichText::new(channel_name)
                                .color(t::text_secondary())
                                .font(t::small()),
                        ));

                        if self.show_timestamps {
                            let ts = format_message_timestamp(
                                &msg.timestamp,
                                self.show_timestamp_seconds,
                                self.use_24h_timestamps,
                            );
                            ui.add(Label::new(
                                RichText::new(ts).color(t::timestamp()).font(t::small()),
                            ));
                            ui.add(Label::new(
                                RichText::new("·").color(t::separator()).font(t::small()),
                            ));
                        }

                        if self.can_moderate && msg.channel.is_twitch() {
                            let presets = if self.mod_action_presets.is_empty() {
                                crust_core::model::mod_actions::ModActionPreset::defaults()
                            } else {
                                self.mod_action_presets.to_vec()
                            };
                            let channel_name = msg.channel.display_name().to_ascii_lowercase();
                            let target_login = msg.sender.login.clone();
                            let mut command_to_send: Option<String> = None;
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
                                for preset in presets {
                                    let (glyph, fg_color, bg_color, stroke_color) = match preset
                                        .action_type()
                                    {
                                        crust_core::model::mod_actions::ModActionType::Ban => (
                                            "⛔",
                                            t::red(),
                                            t::alpha(t::red(), 12),
                                            t::alpha(t::red(), 95),
                                        ),
                                        crust_core::model::mod_actions::ModActionType::Timeout { .. } => (
                                            "⏱",
                                            t::yellow(),
                                            t::alpha(t::yellow(), 12),
                                            t::alpha(t::yellow(), 90),
                                        ),
                                        crust_core::model::mod_actions::ModActionType::Delete => (
                                            "🗑",
                                            t::text_secondary(),
                                            t::alpha(t::text_secondary(), 12),
                                            t::alpha(t::text_secondary(), 80),
                                        ),
                                        crust_core::model::mod_actions::ModActionType::Custom => (
                                            "⚙",
                                            t::accent(),
                                            t::alpha(t::accent(), 12),
                                            t::alpha(t::accent(), 90),
                                        ),
                                    };
                                    let response = egui::Frame::new()
                                        .fill(bg_color)
                                        .stroke(egui::Stroke::new(1.0, stroke_color))
                                        .corner_radius(egui::CornerRadius::same(2))
                                        .inner_margin(egui::Margin::symmetric(1, 0))
                                        .show(ui, |ui| {
                                            ui.add_sized(
                                                [16.0, 16.0],
                                                Label::new(
                                                    RichText::new(glyph)
                                                        .font(t::small())
                                                        .color(fg_color)
                                                        .strong(),
                                                )
                                                .sense(egui::Sense::click())
                                                .wrap(),
                                            )
                                            .on_hover_text(format!(
                                                "{}\n{}",
                                                preset.display_label(),
                                                preset.command_template
                                            ))
                                        })
                                        .response;
                                    if response.clicked() {
                                        command_to_send =
                                            Some(preset.expand(&target_login, &channel_name));
                                    }
                                }
                            });
                            if let Some(command) = command_to_send {
                                let _ = self.cmd_tx.try_send(AppCommand::SendMessage {
                                    channel: self.channel.clone(),
                                    text: command,
                                    reply_to_msg_id: None,
                                    reply: None,
                                });
                            }
                        }

                        let name = if msg.flags.is_action {
                            format!("* {}:", msg.sender.display_name)
                        } else {
                            format!("{}:", msg.sender.display_name)
                        };
                        let name_color = msg
                            .sender
                            .color
                            .as_deref()
                            .and_then(parse_hex_color)
                            .unwrap_or_else(|| resolve_sender_color(msg, user_color_cache));
                        let name_resp = show_sender_name(
                            ui,
                            msg.id.0,
                            &name,
                            name_color,
                            msg.sender.name_paint.as_ref(),
                            self.emote_bytes,
                            self.cmd_tx,
                        )
                        .on_hover_ui(|ui| {
                            ui.label(format!("@{}", msg.sender.login));
                        });
                        name_resp
                            .context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));
                        if name_resp.clicked_by(egui::PointerButton::Primary) {
                            let _ = self.cmd_tx.try_send(AppCommand::ShowUserCard {
                                login: msg.sender.login.clone(),
                                channel: self.channel.clone(),
                            });
                            let key = Id::new("ml_profile_req").with(self.channel.as_str());
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(
                                    key,
                                    (msg.sender.login.clone(), msg.sender.badges.clone()),
                                );
                            });
                        }
                        if name_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            if msg.flags.is_deleted {
                                ui.add(
                                    Label::new(
                                        RichText::new(format!("✂ {}", msg.raw_text))
                                            .strikethrough()
                                            .italics()
                                            .color(t::text_muted()),
                                    )
                                    .wrap(),
                                );
                            } else {
                                for span in &msg.spans {
                                    self.render_span(
                                        ui,
                                        span,
                                        msg.flags.is_action,
                                        msg,
                                        reply_key,
                                        static_frames,
                                    );
                                }
                            }
                        });
                    },
                );
            }
            let msg_frame_resp = prepared.end(ui);
            let bg_click = ui.interact(msg_frame_resp.rect, bg_click_id, egui::Sense::click());
            bg_click.context_menu(|ui| self.show_message_context_menu(ui, msg, reply_key));

            if let Some(bar_col) = message_left_accent_color(
                &msg.flags,
                &msg.msg_kind,
                keyword_highlight,
                keyword_highlight_color,
            ) {
                let r = msg_frame_resp.rect;
                let strip = egui::Rect::from_min_size(r.left_top(), egui::vec2(3.0, r.height()));
                ui.painter().rect_filled(strip, 0.0, bar_col);
            }
        });
    }

    /// Render an image from raw bytes, freezing animated sources to a cached static frame.
    ///
    /// Uses `Sense::click()` (not `Sense::hover()`) because egui 0.31's interaction
    /// system only adds widgets to the `hovered` IdSet when they are in `hits.click`
    /// or `hits.drag`.  A `Sense::hover()`-only widget has click=false/drag=false so
    /// it is never selected by hit-test, `response.hovered()` is always false, and
    /// `on_hover_ui_at_pointer` never fires.  Using `Sense::click()` ensures the image
    /// enters the hovered set so tooltips work.  We never actually handle the click
    /// on images - callers that want right-click menus chain `.context_menu()` themselves.
    fn show_image(
        &self,
        ui: &mut Ui,
        uri: &str,
        raw: &Arc<[u8]>,
        size: Vec2,
        url_hint: Option<&str>,
        static_frames: &mut HashMap<String, StaticFrameCacheEntry>,
    ) -> egui::Response {
        let cache_key = url_hint.unwrap_or(uri);
        let is_animated =
            url_hint.map(is_likely_animated_url).unwrap_or(false) || is_likely_animated_bytes(raw);

        if is_animated && !self.animate_emotes {
            if !static_frames.contains_key(cache_key) {
                let entry = if let Some(img) = decode_static_frame(raw) {
                    let tex = ui.ctx().load_texture(
                        format!("ml-static://{cache_key}"),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    StaticFrameCacheEntry::Loaded(tex)
                } else {
                    StaticFrameCacheEntry::Unavailable
                };
                static_frames.insert(cache_key.to_owned(), entry);
            }

            if let Some(entry) = static_frames.get(cache_key) {
                if let StaticFrameCacheEntry::Loaded(tex) = entry {
                    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                    ui.painter().image(
                        tex.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        t::text_on_accent(),
                    );
                    return resp;
                }
                return show_image_placeholder(ui, size);
            }
        }

        if is_animated && self.animate_emotes {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }

        ui.add(
            egui::Image::from_bytes(uri.to_owned(), egui::load::Bytes::Shared(raw.clone()))
                .sense(egui::Sense::click())
                .fit_to_exact_size(size),
        )
    }
}

fn format_message_timestamp(
    ts: &chrono::DateTime<chrono::Utc>,
    show_seconds: bool,
    use_24h: bool,
) -> String {
    let local = ts.with_timezone(&chrono::Local);
    if use_24h && show_seconds {
        local.format("%H:%M:%S").to_string()
    } else if use_24h {
        local.format("%H:%M").to_string()
    } else if show_seconds {
        local.format("%I:%M:%S %p").to_string()
    } else {
        local.format("%I:%M %p").to_string()
    }
}

/// Collapse a message body after a line budget and append an ellipsis.
///
/// This reduces expensive layout work on giant copypastas while staying
/// lightweight in egui:
/// - explicit newline budget first
/// - fallback soft-wrap estimate for long single-line bursts
fn collapse_message_for_display(text: &str, max_lines: usize) -> Option<String> {
    if text.is_empty() || max_lines == 0 {
        return None;
    }

    if let Some(collapsed) = collapse_by_newline_budget(text, max_lines) {
        return Some(collapsed);
    }

    const SOFT_WRAP_CHARS_PER_LINE_ESTIMATE: usize = 110;
    collapse_by_char_budget(
        text,
        max_lines.saturating_mul(SOFT_WRAP_CHARS_PER_LINE_ESTIMATE),
    )
}

fn collapse_by_newline_budget(text: &str, max_lines: usize) -> Option<String> {
    let mut lines_seen = 1usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            lines_seen += 1;
            if lines_seen > max_lines {
                return Some(with_ellipsis(text, idx));
            }
        }
    }
    None
}

fn collapse_by_char_budget(text: &str, char_budget: usize) -> Option<String> {
    if text.chars().count() <= char_budget {
        return None;
    }
    let cut = text
        .char_indices()
        .nth(char_budget)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    Some(with_ellipsis(text, cut))
}

fn with_ellipsis(text: &str, cut: usize) -> String {
    let trimmed = text[..cut].trim_end();
    if trimmed.is_empty() {
        "…".to_owned()
    } else {
        format!("{trimmed} …")
    }
}

/// Strip invisible / zero-width Unicode characters that render as squares.
/// Preserves normal whitespace (space, newline) but removes combining marks
/// that appear without a preceding base character, zero-width joiners/non-
/// joiners, direction overrides, and other control characters that most fonts
/// cannot render.
///
/// PERF: fast-path scan first - if no bytes need stripping, return a
/// zero-allocation `Cow::Borrowed` pointing at the original string.
fn strip_invisible_chars(s: &str) -> std::borrow::Cow<'_, str> {
    // Fast-path: scan for any character that might need stripping.
    // Most chat messages are plain ASCII text with no invisible chars,
    // so this avoids allocating a new String every frame per text span.
    let needs_work = s.chars().any(|c| {
        if c == ' ' || c == '\n' || c == '\t' {
            return false;
        }
        if c.is_control() {
            return true;
        }
        let cp = c as u32;
        is_invisible_codepoint(cp) || is_combining_codepoint(cp)
    });
    if !needs_work {
        return std::borrow::Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len());
    let mut prev_is_base = false; // was the previous kept char a base character?
    for c in s.chars() {
        // Keep ASCII printable + common whitespace verbatim.
        if c == ' ' || c == '\n' || c == '\t' {
            prev_is_base = false;
            out.push(c);
            continue;
        }
        // Drop C0/C1 control characters (except the whitespace above).
        if c.is_control() {
            continue;
        }
        let cp = c as u32;
        // Zero-width and invisible formatting characters - always drop.
        if is_invisible_codepoint(cp) {
            continue;
        }
        // Combining marks: keep them only when they follow a base character,
        // otherwise they render as standalone squares / dotted circles.
        if is_combining_codepoint(cp) {
            if prev_is_base {
                out.push(c);
            }
            // Whether kept or dropped, the next char still has a base before it
            // (we don't reset prev_is_base so stacked diacritics work).
            continue;
        }
        // Everything else is a visible base character - keep it.
        prev_is_base = true;
        out.push(c);
    }
    std::borrow::Cow::Owned(out)
}

/// Returns true for zero-width and invisible formatting codepoints.
#[inline]
fn is_invisible_codepoint(cp: u32) -> bool {
    matches!(
        cp,
        0x00AD             // Soft Hyphen
        | 0x034F           // Combining Grapheme Joiner
        | 0x061C           // Arabic Letter Mark
        | 0x180E           // Mongolian Vowel Separator
        | 0x200B           // Zero Width Space
        | 0x200C           // Zero Width Non-Joiner
        | 0x200D           // Zero Width Joiner (outside emoji context)
        | 0x200E           // LTR Mark
        | 0x200F           // RTL Mark
        | 0x2028           // Line Separator
        | 0x2029           // Paragraph Separator
        | 0x202A..=0x202E  // LTR/RTL embedding/override/pop
        | 0x2060           // Word Joiner
        | 0x2061..=0x2064  // Invisible operators
        | 0x2066..=0x2069  // Isolate formatting
        | 0x206A..=0x206F  // Deprecated formatting
        | 0x2800           // Braille Pattern Blank
        | 0x3164           // Hangul Filler
        | 0xFE00..=0xFE0F  // Variation Selectors
        | 0xFEFF           // BOM / Zero Width No-Break Space
        | 0xFFA0           // Halfwidth Hangul Filler
        | 0xFFF9..=0xFFFB  // Interlinear annotations
        | 0xE0000..=0xE007F // Tags block
        | 0xE0100..=0xE01EF // Variation Selectors Supplement
    )
}

/// Returns true for Unicode combining mark codepoints.
#[inline]
fn is_combining_codepoint(cp: u32) -> bool {
    matches!(
        cp,
        0x0300..=0x036F    // Combining Diacritical Marks
        | 0x0483..=0x0489  // Combining Cyrillic
        | 0x0591..=0x05BD  // Hebrew accents
        | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
        | 0x0610..=0x061A  // Arabic combining
        | 0x064B..=0x065F  // Arabic combining marks
        | 0x0670           // Arabic superscript alef
            | 0x06D6..=0x06DC  // Arabic small marks
            | 0x06DF..=0x06E4
            | 0x06E7..=0x06E8
            | 0x06EA..=0x06ED
            | 0x0730..=0x074A  // Syriac combining
            | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E  // Thai
            | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECE  // Lao
            | 0x1AB0..=0x1AFF  // Combining Diacritical Marks Extended
            | 0x1DC0..=0x1DFF  // Combining Diacritical Marks Supplement
            | 0x20D0..=0x20FF  // Combining Marks for Symbols
            | 0xFE20..=0xFE2F  // Combining Half Marks
    )
}

/// Scale image dimensions to a target height, preserving aspect ratio.
fn fit_size(w: u32, h: u32, target_h: f32) -> Vec2 {
    if h == 0 {
        return Vec2::new(target_h, target_h);
    }
    let scale = target_h / h as f32;
    Vec2::new(w as f32 * scale, target_h)
}

fn is_likely_animated_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains(".gif") || lower.contains(".webp")
}

fn is_likely_animated_bytes(raw: &[u8]) -> bool {
    // GIF: more than one image descriptor usually indicates multiple frames.
    let is_gif = raw.len() >= 6 && (&raw[..6] == b"GIF87a" || &raw[..6] == b"GIF89a");
    if is_gif {
        let frame_markers = raw.iter().filter(|&&b| b == 0x2C).take(2).count();
        if frame_markers >= 2 {
            return true;
        }
    }

    // WEBP animation uses an ANIM chunk in RIFF/WEBP containers.
    let is_webp = raw.len() >= 12 && &raw[..4] == b"RIFF" && &raw[8..12] == b"WEBP";
    is_webp && raw.windows(4).any(|w| w == b"ANIM")
}

fn decode_static_frame(raw: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(raw).ok()?;
    dynamic_image_to_color_image(img)
}

fn dynamic_image_to_color_image(img: DynamicImage) -> Option<egui::ColorImage> {
    let rgba = img.to_rgba8();
    let w = usize::try_from(rgba.width()).ok()?;
    let h = usize::try_from(rgba.height()).ok()?;
    let pixels = rgba.into_raw();
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels))
}

fn show_image_placeholder(ui: &mut Ui, size: Vec2) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(3), Color32::from_gray(80));
    resp
}

fn crust_seeded_username_color(seed: &str) -> Option<Color32> {
    if seed.is_empty() {
        return None;
    }

    let color_seed: i64 = seed.parse::<i64>().unwrap_or_else(|_| {
        // For non-numeric IDs, derive a stable seed from the string content.
        seed.chars()
            .map(|c| c.to_digit(10).map(|d| d as i64).unwrap_or(-1))
            .sum()
    });

    let idx = color_seed.rem_euclid(TWITCH_USERNAME_COLORS.len() as i64) as usize;
    Some(TWITCH_USERNAME_COLORS[idx])
}

fn resolve_sender_color(
    msg: &ChatMessage,
    user_color_cache: &mut HashMap<String, Color32>,
) -> Color32 {
    let login_key = msg.sender.login.trim().to_ascii_lowercase();

    if let Some(parsed) = msg
        .sender
        .color
        .as_deref()
        .and_then(parse_hex_color_opaque_rgb)
    {
        if !login_key.is_empty() {
            if user_color_cache.len() >= MAX_CACHED_USERNAME_COLORS {
                user_color_cache.clear();
            }
            user_color_cache.insert(login_key, parsed);
        }
        return parsed;
    }

    if !login_key.is_empty() {
        if let Some(cached) = user_color_cache.get(&login_key).copied() {
            return cached;
        }
    }

    let seeded = crust_seeded_username_color(msg.sender.user_id.0.trim())
        .or_else(|| crust_seeded_username_color(login_key.as_str()));
    if let Some(color) = seeded {
        if !login_key.is_empty() {
            if user_color_cache.len() >= MAX_CACHED_USERNAME_COLORS {
                user_color_cache.clear();
            }
            user_color_cache.entry(login_key).or_insert(color);
        }
        return color;
    }

    t::accent()
}

fn show_sender_name(
    ui: &mut Ui,
    message_id: u64,
    name: &str,
    fallback_color: Color32,
    paint: Option<&SenderNamePaint>,
    emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> egui::Response {
    let fallback_color =
        Color32::from_rgb(fallback_color.r(), fallback_color.g(), fallback_color.b());
    if let Some(paint) = paint {
        let label = Label::new(RichText::new(name).strong())
            .selectable(false)
            .sense(egui::Sense::click());
        let (galley_pos, galley, response) = label.layout_in_ui(ui);

        if ui.is_rect_visible(response.rect) {
            let paint_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, galley.size());
            let stops = normalized_paint_stops(paint, fallback_color);
            let url_sampler = url_paint_sampler(ui, paint, emote_bytes, cmd_tx);
            let font_uv_normalizer = font_uv_normalizer(ui);
            if stops.is_some() || url_sampler.is_some() {
                if !paint.shadows.is_empty() {
                    paint_sender_name_shadows(ui, paint, galley_pos, &galley, font_uv_normalizer);
                }
                let mut painted_vertices = 0usize;
                for row in &galley.rows {
                    let mut row_painted = false;
                    let mut mesh = row.visuals.mesh.clone();
                    normalize_text_mesh_uvs(&mut mesh, font_uv_normalizer);
                    if !row.visuals.glyph_vertex_range.is_empty() {
                        for idx in row.visuals.glyph_vertex_range.clone() {
                            if let Some(v) = mesh.vertices.get_mut(idx) {
                                if let Some(sampler) = &url_sampler {
                                    v.color = sample_url_paint_color(
                                        fallback_color,
                                        sampler,
                                        paint_rect,
                                        v.pos,
                                    );
                                } else if let Some(stops) = &stops {
                                    let t = paint_sample_t(paint, paint_rect, v.pos, stops);
                                    v.color = gradient_color_at_t(stops, t, paint.repeat);
                                } else {
                                    v.color = fallback_color;
                                }
                                painted_vertices += 1;
                                row_painted = true;
                            }
                        }
                    }
                    if !row_painted {
                        continue;
                    }
                    mesh.translate(galley_pos.to_vec2());
                    ui.painter().add(egui::Shape::mesh(mesh));
                }
                if painted_vertices == 0 {
                    ui.painter().add(egui::epaint::TextShape::new(
                        galley_pos,
                        galley,
                        fallback_color,
                    ));
                }
            } else {
                ui.painter().add(egui::epaint::TextShape::new(
                    galley_pos,
                    galley,
                    fallback_color,
                ));
            }
        }

        let id = Id::new("ml_sender_name_paint").with(message_id);
        return ui.interact(response.rect, id, egui::Sense::click());
    }

    ui.add(
        Label::new(RichText::new(name).color(fallback_color).strong())
            .selectable(false)
            .sense(egui::Sense::click()),
    )
}

#[derive(Clone)]
struct UrlPaintSampler {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

fn url_paint_sampler(
    ui: &Ui,
    paint: &SenderNamePaint,
    emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> Option<UrlPaintSampler> {
    let function = paint.function.trim();
    if !function.eq_ignore_ascii_case("url") && !function.eq_ignore_ascii_case("image") {
        return None;
    }
    let raw_url = paint.image_url.as_ref()?;
    let url = normalize_external_image_url(raw_url)?;
    if !emote_bytes.contains_key(url.as_str()) {
        let _ = cmd_tx.try_send(AppCommand::FetchImage { url: url.clone() });
        return None;
    }

    let cache_key = Id::new("ml_url_paint_sampler").with(url.as_str());
    if let Some(cached) = ui
        .ctx()
        .data_mut(|d| d.get_temp::<UrlPaintSampler>(cache_key))
    {
        return Some(cached);
    }

    let (_w, _h, raw) = emote_bytes.get(url.as_str())?;
    let decoded = image::load_from_memory(raw).ok()?.to_rgba8();
    let (dw, dh) = decoded.dimensions();
    let rgba: Arc<[u8]> = decoded.into_raw().into();
    let sampler = UrlPaintSampler {
        width: dw.max(1),
        height: dh.max(1),
        rgba,
    };
    ui.ctx()
        .data_mut(|d| d.insert_temp(cache_key, sampler.clone()));
    Some(sampler)
}

fn normalize_external_image_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("//") {
        return Some(format!("https:{trimmed}"));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_owned());
    }
    None
}

fn sample_url_paint_color(
    user_color: Color32,
    sampler: &UrlPaintSampler,
    drawing_rect: egui::Rect,
    sample: egui::Pos2,
) -> Color32 {
    let w = drawing_rect.width().max(1.0);
    let h = drawing_rect.height().max(1.0);
    let u = ((sample.x - drawing_rect.left()) / w).clamp(0.0, 1.0);
    let v = ((sample.y - drawing_rect.top()) / h).clamp(0.0, 1.0);

    let sx = u * sampler.width.saturating_sub(1) as f32;
    let sy = v * sampler.height.saturating_sub(1) as f32;
    let x0 = sx.floor() as u32;
    let y0 = sy.floor() as u32;
    let x1 = (x0 + 1).min(sampler.width.saturating_sub(1));
    let y1 = (y0 + 1).min(sampler.height.saturating_sub(1));
    let tx = sx - x0 as f32;
    let ty = sy - y0 as f32;

    let c00 = sampler_pixel(sampler, x0, y0);
    let c10 = sampler_pixel(sampler, x1, y0);
    let c01 = sampler_pixel(sampler, x0, y1);
    let c11 = sampler_pixel(sampler, x1, y1);

    let lerp = |a: u8, b: u8, t: f32| -> f32 { a as f32 + (b as f32 - a as f32) * t };
    let top = [
        lerp(c00[0], c10[0], tx),
        lerp(c00[1], c10[1], tx),
        lerp(c00[2], c10[2], tx),
        lerp(c00[3], c10[3], tx),
    ];
    let bot = [
        lerp(c01[0], c11[0], tx),
        lerp(c01[1], c11[1], tx),
        lerp(c01[2], c11[2], tx),
        lerp(c01[3], c11[3], tx),
    ];
    let lerp_f = |a: f32, b: f32, t: f32| -> f32 { a + (b - a) * t };
    let rgba = [
        lerp_f(top[0], bot[0], ty).round() as u8,
        lerp_f(top[1], bot[1], ty).round() as u8,
        lerp_f(top[2], bot[2], ty).round() as u8,
        lerp_f(top[3], bot[3], ty).round() as u8,
    ];

    if rgba[3] == 0 {
        return user_color;
    }
    let fg = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
    overlay_colors(user_color, fg)
}

fn sampler_pixel(sampler: &UrlPaintSampler, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * sampler.width + x) * 4) as usize;
    if idx + 3 >= sampler.rgba.len() {
        return [0, 0, 0, 0];
    }
    [
        sampler.rgba[idx],
        sampler.rgba[idx + 1],
        sampler.rgba[idx + 2],
        sampler.rgba[idx + 3],
    ]
}

fn paint_sender_name_shadows(
    ui: &Ui,
    paint: &SenderNamePaint,
    galley_pos: egui::Pos2,
    galley: &Arc<egui::Galley>,
    uv_normalizer: egui::Vec2,
) {
    for shadow in &paint.shadows {
        if shadow.radius <= 0.0 {
            continue;
        }
        let Some(base_color) = parse_hex_color(&shadow.color) else {
            continue;
        };
        let offsets = shadow_offsets(shadow.x_offset, shadow.y_offset, shadow.radius);
        if offsets.is_empty() {
            continue;
        }
        for row in &galley.rows {
            for (dx, dy, alpha_scale) in &offsets {
                let mut mesh = row.visuals.mesh.clone();
                normalize_text_mesh_uvs(&mut mesh, uv_normalizer);
                let c = scale_alpha(base_color, *alpha_scale);
                for idx in row.visuals.glyph_vertex_range.clone() {
                    if let Some(v) = mesh.vertices.get_mut(idx) {
                        v.color = c;
                    }
                }
                mesh.translate(galley_pos.to_vec2() + egui::vec2(*dx, *dy));
                ui.painter().add(egui::Shape::mesh(mesh));
            }
        }
    }
}

fn shadow_offsets(x: f32, y: f32, radius: f32) -> Vec<(f32, f32, f32)> {
    // Match the large-shadow look by scaling the blur radius by 3.
    let blur_radius = (radius.max(0.0)) * 3.0;
    if blur_radius <= f32::EPSILON {
        return Vec::new();
    }

    // Approximate QPixmapDropShadowFilter with a normalized Gaussian kernel.
    // Keep tap count bounded for runtime stability in busy chats.
    let tap_radius = ((blur_radius * 0.75).round() as i32).clamp(1, 6);
    let step = (blur_radius / tap_radius as f32).max(0.5);
    let sigma = (blur_radius * 0.5).max(0.5);
    let two_sigma2 = 2.0 * sigma * sigma;

    let mut taps: Vec<(f32, f32, f32)> = Vec::new();
    let mut weight_sum = 0.0f32;
    for gy in -tap_radius..=tap_radius {
        for gx in -tap_radius..=tap_radius {
            let ox = gx as f32 * step;
            let oy = gy as f32 * step;
            let dist2 = ox * ox + oy * oy;
            let weight = (-dist2 / two_sigma2).exp();
            if weight < 0.001 {
                continue;
            }
            taps.push((x + ox, y + oy, weight));
            weight_sum += weight;
        }
    }

    if weight_sum <= f32::EPSILON {
        return Vec::new();
    }

    for tap in &mut taps {
        tap.2 /= weight_sum;
    }
    taps
}

fn scale_alpha(color: Color32, factor: f32) -> Color32 {
    let a = (color.a() as f32 * factor.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

fn font_uv_normalizer(ui: &Ui) -> egui::Vec2 {
    ui.fonts(|fonts| {
        let [w, h] = fonts.font_image_size();
        egui::vec2(1.0 / (w.max(1) as f32), 1.0 / (h.max(1) as f32))
    })
}

fn normalize_text_mesh_uvs(mesh: &mut egui::epaint::Mesh, uv_normalizer: egui::Vec2) {
    for v in &mut mesh.vertices {
        v.uv = (v.uv.to_vec2() * uv_normalizer).to_pos2();
    }
}

fn normalized_paint_stops(
    paint: &SenderNamePaint,
    user_color: Color32,
) -> Option<Vec<(f32, Color32)>> {
    let mut stops: Vec<(f32, Color32)> = paint
        .stops
        .iter()
        .filter_map(|s| parse_hex_color(&s.color).map(|c| (s.at, overlay_colors(user_color, c))))
        .collect();
    if stops.is_empty() {
        return None;
    }
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Some(stops)
}

fn paint_sample_t(
    paint: &SenderNamePaint,
    rect: egui::Rect,
    sample: egui::Pos2,
    stops: &[(f32, Color32)],
) -> f32 {
    match paint.function.as_str() {
        "radial-gradient" | "radial_gradient" | "radial" => {
            radial_gradient_t(rect, sample, paint.repeat, stops)
        }
        _ => linear_gradient_t(rect, sample, paint.angle.unwrap_or(0.0)),
    }
}

fn linear_gradient_t(rect: egui::Rect, sample: egui::Pos2, angle_deg: f32) -> f32 {
    let mut start = rect.left_bottom();
    let mut end = rect.right_top();
    // Use `int(angle / 90) % 4` with truncation toward zero.
    let angle_step = ((angle_deg / 90.0).trunc() as i32) % 4;
    if angle_step == 1 {
        start = rect.left_top();
        end = rect.right_bottom();
    } else if angle_step == 2 {
        start = rect.right_top();
        end = rect.left_bottom();
    } else if angle_step == 3 {
        start = rect.right_bottom();
        end = rect.left_top();
    }

    let center = rect.center();
    let gradient_angle = 90.0 - angle_deg;
    let color_axis_angle = -angle_deg;

    let (gradient_origin, gradient_dir) = line_from_angle(center, gradient_angle);
    let (start_origin, start_dir) = line_from_angle(start, color_axis_angle);
    let (end_origin, end_dir) = line_from_angle(end, color_axis_angle);

    let gradient_start =
        line_intersection(gradient_origin, gradient_dir, start_origin, start_dir).unwrap_or(start);
    let gradient_end =
        line_intersection(gradient_origin, gradient_dir, end_origin, end_dir).unwrap_or(end);

    let axis = gradient_end - gradient_start;
    let len2 = axis.length_sq().max(f32::EPSILON);
    (sample - gradient_start).dot(axis) / len2
}

fn radial_gradient_t(
    rect: egui::Rect,
    sample: egui::Pos2,
    repeat: bool,
    stops: &[(f32, Color32)],
) -> f32 {
    let mut radius = rect.width().max(rect.height()) * 0.5;
    if repeat {
        let tail = stops.last().map(|(at, _)| *at).unwrap_or(1.0);
        radius *= tail.max(f32::EPSILON);
    }
    if radius <= f32::EPSILON {
        return 0.0;
    }
    sample.distance(rect.center()) / radius
}

fn line_from_angle(origin: egui::Pos2, angle_deg: f32) -> (egui::Pos2, egui::Vec2) {
    let radians = angle_deg.to_radians();
    let dir = egui::vec2(radians.cos(), -radians.sin());
    (origin, dir)
}

fn line_intersection(
    p1: egui::Pos2,
    d1: egui::Vec2,
    p2: egui::Pos2,
    d2: egui::Vec2,
) -> Option<egui::Pos2> {
    let det = d1.x * d2.y - d1.y * d2.x;
    if det.abs() <= f32::EPSILON {
        return None;
    }
    let delta = p2 - p1;
    let t = (delta.x * d2.y - delta.y * d2.x) / det;
    Some(p1 + d1 * t)
}

fn gradient_color_at_t(stops: &[(f32, Color32)], t: f32, repeat: bool) -> Color32 {
    if stops.is_empty() {
        return Color32::WHITE;
    }

    if repeat && stops.len() >= 2 {
        let start = stops[0].0;
        let end = stops[stops.len() - 1].0;
        let len = end - start;
        if len.abs() > f32::EPSILON {
            let wrapped = ((t - start) / len).rem_euclid(1.0);
            let normalized: Vec<(f32, Color32)> = stops
                .iter()
                .map(|(at, c)| ((at - start) / len, *c))
                .collect();
            return gradient_color_at_t(&normalized, wrapped, false);
        }
    }

    if t <= stops[0].0 {
        return stops[0].1;
    }
    for window in stops.windows(2) {
        let (at0, c0) = window[0];
        let (at1, c1) = window[1];
        if t <= at1 {
            let span = (at1 - at0).max(f32::EPSILON);
            let local_t = ((t - at0) / span).clamp(0.0, 1.0);
            return Color32::from_rgba_unmultiplied(
                lerp_u8(c0.r(), c1.r(), local_t),
                lerp_u8(c0.g(), c1.g(), local_t),
                lerp_u8(c0.b(), c1.b(), local_t),
                255,
            );
        }
    }
    stops.last().map(|(_, c)| *c).unwrap_or(Color32::WHITE)
}

fn overlay_colors(background: Color32, foreground: Color32) -> Color32 {
    let alpha = foreground.a() as f32 / 255.0;
    let r = ((1.0 - alpha) * background.r() as f32) + (alpha * foreground.r() as f32);
    let g = ((1.0 - alpha) * background.g() as f32) + (alpha * foreground.g() as f32);
    let b = ((1.0 - alpha) * background.b() as f32) + (alpha * foreground.b() as f32);
    Color32::from_rgb(r.round() as u8, g.round() as u8, b.round() as u8)
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

fn parse_hex_color(s: &str) -> Option<Color32> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 && s.len() != 8 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    let a = if s.len() == 8 {
        u8::from_str_radix(&s[6..8], 16).ok()?
    } else {
        255
    };
    Some(Color32::from_rgba_unmultiplied(r, g, b, a))
}

fn parse_hex_color_opaque_rgb(s: &str) -> Option<Color32> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 && s.len() != 8 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

fn normalized_badge_name(name: &str) -> Cow<'_, str> {
    if name.contains('_') {
        Cow::Owned(name.replace('_', "-"))
    } else {
        Cow::Borrowed(name)
    }
}

/// Build a human-readable badge tooltip from the set name and version.
fn pretty_badge_name(name: &str, version: &str) -> String {
    let canonical = normalized_badge_name(name);
    let label = canonical
        .split(|c: char| c == '-' || c == '_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => {
                    let mut s = c.to_uppercase().to_string();
                    s.push_str(chars.as_str());
                    s
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    // For subscriber badges the version usually indicates months
    if canonical.as_ref() == "subscriber" {
        if let Ok(months) = version.parse::<u32>() {
            if months == 0 || months == 1 {
                return format!("{label} (New)");
            }
            return format!("{label} ({months} months)");
        }
    }

    // For bits/sub-gifter the version is the tier/count
    if version != "1" && version != "0" {
        return format!("{label} ({version})");
    }

    label
}

fn render_badge_fallback(ui: &mut Ui, name: &str, version: &str, tooltip: &str) {
    let (bg, fg) = badge_chip_colors(name);
    let chip_text = badge_chip_text(name, version);
    let stroke = if t::is_light() {
        bg.gamma_multiply(0.72)
    } else {
        bg.gamma_multiply(1.25)
    };
    let response = egui::Frame::new()
        .fill(bg)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(3, 1))
        .show(ui, |ui| {
            ui.add(Label::new(
                RichText::new(&chip_text).font(t::tiny()).color(fg).strong(),
            ));
        })
        .response;
    response.on_hover_ui_at_pointer(|ui| {
        ui.vertical_centered(|ui| {
            ui.add(Label::new(
                RichText::new(&chip_text).size(18.0).color(fg).strong(),
            ));
            ui.add_space(4.0);
            ui.label(RichText::new(tooltip).strong());
        });
    });
}

fn badge_chip_text(name: &str, version: &str) -> String {
    match normalized_badge_name(name).as_ref() {
        "subscriber" => {
            if let Ok(months) = version.parse::<u32>() {
                if months > 1 {
                    return format!("SUB{months}");
                }
            }
            "SUB".to_owned()
        }
        "sub-gifter" => "GIFT".to_owned(),
        "founder" => "FND".to_owned(),
        "moderator" => "MOD".to_owned(),
        "broadcaster" => "CAST".to_owned(),
        "vip" => "VIP".to_owned(),
        "verified" => "VER".to_owned(),
        "staff" => "STAFF".to_owned(),
        "global-mod" => "GMOD".to_owned(),
        "artist-badge" => "ART".to_owned(),
        "premium" => "PRIME".to_owned(),
        _ => {
            let first = name
                .split(|c: char| c == '-' || c == '_')
                .find(|part| !part.is_empty())
                .unwrap_or(name);
            first.chars().take(4).collect::<String>().to_uppercase()
        }
    }
}

fn badge_chip_colors(name: &str) -> (Color32, Color32) {
    let light = t::is_light();
    match normalized_badge_name(name).as_ref() {
        "subscriber" => {
            if light {
                (
                    Color32::from_rgb(210, 246, 218),
                    Color32::from_rgb(32, 66, 38),
                )
            } else {
                (
                    Color32::from_rgb(52, 86, 58),
                    Color32::from_rgb(210, 246, 218),
                )
            }
        }
        "sub-gifter" => {
            if light {
                (
                    Color32::from_rgb(252, 232, 180),
                    Color32::from_rgb(64, 47, 14),
                )
            } else {
                (
                    Color32::from_rgb(84, 67, 34),
                    Color32::from_rgb(252, 222, 154),
                )
            }
        }
        "founder" => {
            if light {
                (
                    Color32::from_rgb(215, 218, 255),
                    Color32::from_rgb(40, 42, 75),
                )
            } else {
                (
                    Color32::from_rgb(60, 62, 95),
                    Color32::from_rgb(200, 206, 255),
                )
            }
        }
        "moderator" => {
            if light {
                (
                    Color32::from_rgb(200, 244, 217),
                    Color32::from_rgb(23, 69, 39),
                )
            } else {
                (
                    Color32::from_rgb(43, 89, 59),
                    Color32::from_rgb(196, 244, 217),
                )
            }
        }
        "broadcaster" => {
            if light {
                (
                    Color32::from_rgb(255, 220, 220),
                    Color32::from_rgb(82, 25, 25),
                )
            } else {
                (
                    Color32::from_rgb(102, 45, 45),
                    Color32::from_rgb(255, 206, 206),
                )
            }
        }
        "vip" => {
            if light {
                (
                    Color32::from_rgb(255, 220, 248),
                    Color32::from_rgb(92, 37, 78),
                )
            } else {
                (
                    Color32::from_rgb(112, 57, 98),
                    Color32::from_rgb(255, 206, 242),
                )
            }
        }
        "verified" => {
            if light {
                (
                    Color32::from_rgb(210, 235, 255),
                    Color32::from_rgb(26, 48, 87),
                )
            } else {
                (
                    Color32::from_rgb(46, 68, 107),
                    Color32::from_rgb(191, 223, 255),
                )
            }
        }
        "staff" => {
            if light {
                (
                    Color32::from_rgb(228, 234, 240),
                    Color32::from_rgb(56, 64, 72),
                )
            } else {
                (
                    Color32::from_rgb(76, 84, 92),
                    Color32::from_rgb(220, 226, 233),
                )
            }
        }
        _ => {
            if light {
                (
                    Color32::from_rgb(220, 220, 228),
                    Color32::from_rgb(50, 50, 54),
                )
            } else {
                (
                    Color32::from_rgb(70, 70, 74),
                    Color32::from_rgb(210, 210, 215),
                )
            }
        }
    }
}

fn style_system_info_text(raw: &str) -> (Color32, String) {
    let s = raw.trim();
    let Some((code, payload)) = parse_bracket_numeric_prefix(s) else {
        return (t::text_secondary(), s.to_owned());
    };

    match code {
        "375" => (t::link(), format!("IRC MOTD: {}", payload.trim())),
        "372" => (t::text_secondary(), format!("  {}", payload.trim())),
        "376" => (t::green(), "IRC MOTD complete".to_owned()),
        "001" => (t::green(), payload.trim().to_owned()),
        "002" | "003" | "004" | "005" => (t::link(), payload.trim().to_owned()),
        "251" | "252" | "253" | "254" | "255" | "265" | "266" | "250" => {
            (t::text_secondary(), payload.trim().to_owned())
        }
        _ => (t::text_secondary(), s.to_owned()),
    }
}

fn parse_bracket_numeric_prefix(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('[')?;
    let end = rest.find(']')?;
    let code = &rest[..end];
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let payload = rest[end + 1..].trim_start();
    Some((code, payload))
}

fn is_irc_motd_line(text: &str) -> bool {
    text.starts_with("IRC MOTD:") || text.starts_with("  ")
}

/// Map short provider codes to human-readable labels.
fn provider_label(provider: &str) -> &'static str {
    match provider {
        "bttv" => "BetterTTV",
        "ffz" => "FrankerFaceZ",
        "7tv" => "7TV",
        "twitch" => "Twitch",
        "kick" => "Kick",
        _ => "Emote",
    }
}

/// Extract just the hostname from a URL for display (e.g. `"youtube.com"`).
fn url_hostname(url: &str) -> String {
    let s = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = s.split('/').next().unwrap_or(s);
    // Strip www. prefix for cleanliness
    host.trim_start_matches("www.").to_owned()
}

/// Badge-style (background, foreground) colours for known site names shown
/// in the link-preview tooltip.  Roughly matches each site's brand colour.
fn site_badge_colors(site: &str) -> (Color32, Color32) {
    let light = t::is_light();
    match site {
        "YouTube" => {
            if light {
                (
                    Color32::from_rgb(255, 220, 220),
                    Color32::from_rgb(180, 18, 18),
                )
            } else {
                (
                    Color32::from_rgb(120, 20, 20),
                    Color32::from_rgb(255, 100, 100),
                )
            }
        }
        "Twitter" | "X" => {
            if light {
                (
                    Color32::from_rgb(210, 235, 255),
                    Color32::from_rgb(20, 100, 175),
                )
            } else {
                (
                    Color32::from_rgb(20, 55, 90),
                    Color32::from_rgb(100, 180, 255),
                )
            }
        }
        "Twitch" | "Twitch Clip" => {
            if light {
                (
                    Color32::from_rgb(230, 215, 255),
                    Color32::from_rgb(100, 65, 165),
                )
            } else {
                (
                    Color32::from_rgb(60, 40, 100),
                    Color32::from_rgb(190, 160, 255),
                )
            }
        }
        "Reddit" => {
            if light {
                (
                    Color32::from_rgb(255, 225, 210),
                    Color32::from_rgb(200, 70, 20),
                )
            } else {
                (
                    Color32::from_rgb(100, 35, 10),
                    Color32::from_rgb(255, 135, 80),
                )
            }
        }
        "GitHub" => {
            if light {
                (
                    Color32::from_rgb(225, 225, 235),
                    Color32::from_rgb(40, 40, 50),
                )
            } else {
                (
                    Color32::from_rgb(40, 40, 50),
                    Color32::from_rgb(210, 210, 220),
                )
            }
        }
        "Instagram" => {
            if light {
                (
                    Color32::from_rgb(255, 220, 235),
                    Color32::from_rgb(175, 30, 100),
                )
            } else {
                (
                    Color32::from_rgb(90, 15, 50),
                    Color32::from_rgb(255, 120, 175),
                )
            }
        }
        "TikTok" => {
            if light {
                (
                    Color32::from_rgb(230, 245, 250),
                    Color32::from_rgb(20, 20, 30),
                )
            } else {
                (
                    Color32::from_rgb(20, 20, 30),
                    Color32::from_rgb(230, 245, 250),
                )
            }
        }
        "Wikipedia" => {
            if light {
                (
                    Color32::from_rgb(230, 230, 230),
                    Color32::from_rgb(50, 50, 50),
                )
            } else {
                (
                    Color32::from_rgb(50, 50, 55),
                    Color32::from_rgb(220, 220, 225),
                )
            }
        }
        "Steam" => {
            if light {
                (
                    Color32::from_rgb(210, 220, 240),
                    Color32::from_rgb(25, 40, 80),
                )
            } else {
                (
                    Color32::from_rgb(25, 35, 65),
                    Color32::from_rgb(150, 180, 230),
                )
            }
        }
        _ => {
            if light {
                (
                    Color32::from_rgb(225, 230, 240),
                    Color32::from_rgb(60, 65, 80),
                )
            } else {
                (
                    Color32::from_rgb(50, 55, 65),
                    Color32::from_rgb(180, 185, 200),
                )
            }
        }
    }
}

/// Return `(label_text, stripe_color)` for messages with a chat notification.
/// Returns `None` for ordinary messages.
fn redemption_status_presentation(status: Option<&str>) -> (Cow<'static, str>, Color32) {
    let normalized = status
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("UNFULFILLED");

    if normalized.eq_ignore_ascii_case("fulfilled") {
        return (Cow::Borrowed("FULFILLED"), t::green());
    }
    if normalized.eq_ignore_ascii_case("canceled") || normalized.eq_ignore_ascii_case("cancelled") {
        return (Cow::Borrowed("CANCELED"), t::red());
    }
    if normalized.eq_ignore_ascii_case("unfulfilled") {
        return (Cow::Borrowed("UNFULFILLED"), t::gold().gamma_multiply(0.85));
    }

    (Cow::Owned(normalized.to_ascii_uppercase()), t::accent())
}

fn redemption_accent(status: Option<&str>) -> Color32 {
    let (_, color) = redemption_status_presentation(status);
    color
}

fn notification_label(
    flags: &MessageFlags,
    kind: &MsgKind,
    keyword_highlight: bool,
    keyword_highlight_color: Option<Color32>,
) -> Option<(&'static str, Color32)> {
    if keyword_highlight {
        Some((
            "Keyword Highlight",
            keyword_highlight_color.unwrap_or_else(t::twitch_purple),
        ))
    } else if flags.is_mention {
        Some(("Mention", t::bits_orange()))
    } else if let MsgKind::ChannelPointsReward { status, .. } = kind {
        let label = match status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("UNFULFILLED")
            .to_ascii_lowercase()
            .as_str()
        {
            "fulfilled" => "Points Reward Fulfilled",
            "canceled" | "cancelled" => "Points Reward Rejected",
            _ => "Points Redemption",
        };
        Some((label, redemption_accent(status.as_deref())))
    } else if flags.custom_reward_id.is_some() && flags.is_highlighted {
        Some(("Points Highlight", t::accent()))
    } else if flags.custom_reward_id.is_some() {
        Some(("Points Reward Message", t::accent_dim()))
    } else if flags.is_highlighted {
        Some(("Highlighted Message", t::twitch_purple()))
    } else if flags.is_pinned {
        Some(("Pinned Message", t::gold()))
    } else if matches!(kind, MsgKind::Bits { .. }) {
        Some(("Bits Cheer", t::bits_orange()))
    } else if flags.is_first_msg {
        Some(("First Message", t::green()))
    } else if matches!(kind, MsgKind::SuspiciousUserMessage) {
        Some(("Suspicious User", t::red()))
    } else {
        None
    }
}

fn message_row_background(
    flags: &MessageFlags,
    kind: &MsgKind,
    highlight_alpha: f32,
    keyword_highlight: bool,
    keyword_highlight_color: Option<Color32>,
) -> Color32 {
    if highlight_alpha > 0.0 {
        let a = (50.0 * highlight_alpha) as u8;
        t::alpha(t::link(), a)
    } else if keyword_highlight {
        if let Some(color) = keyword_highlight_color {
            t::alpha(color, 18)
        } else {
            t::alpha(t::accent(), 18)
        }
    } else if flags.is_mention {
        t::alpha(t::bits_orange(), 24)
    } else if let MsgKind::ChannelPointsReward { status, .. } = kind {
        let accent = redemption_accent(status.as_deref());
        t::alpha(accent, 16)
    } else if flags.custom_reward_id.is_some() {
        let alpha = if flags.is_highlighted { 24 } else { 16 };
        t::alpha(t::accent_dim(), alpha)
    } else if flags.is_highlighted {
        t::alpha(t::accent(), 22)
    } else if flags.is_pinned {
        let gold = t::gold();
        t::alpha(gold, 26)
    } else if flags.is_first_msg {
        let fg = t::green();
        t::alpha(fg, 22)
    } else if matches!(kind, MsgKind::SuspiciousUserMessage) {
        t::alpha(t::red(), 12)
    } else if flags.is_deleted {
        t::alpha(t::red(), 12)
    } else if matches!(kind, MsgKind::Bits { .. }) {
        t::alpha(t::bits_orange(), 14)
    } else {
        Color32::TRANSPARENT
    }
}

fn message_left_accent_color(
    flags: &MessageFlags,
    kind: &MsgKind,
    keyword_highlight: bool,
    keyword_highlight_color: Option<Color32>,
) -> Option<Color32> {
    if flags.is_mention && matches!(kind, MsgKind::Sub { is_gift: true, .. }) {
        Some(t::raid_cyan())
    } else if flags.is_mention {
        Some(t::accent())
    } else if keyword_highlight {
        Some(keyword_highlight_color.unwrap_or_else(t::twitch_purple))
    } else if let MsgKind::ChannelPointsReward { status, .. } = kind {
        Some(redemption_accent(status.as_deref()))
    } else if flags.custom_reward_id.is_some() && flags.is_highlighted {
        Some(t::accent_dim())
    } else if flags.custom_reward_id.is_some() {
        Some(t::accent())
    } else if flags.is_highlighted {
        Some(t::gold())
    } else if flags.is_pinned {
        Some(t::gold())
    } else if flags.is_first_msg {
        Some(t::green())
    } else if matches!(kind, MsgKind::SuspiciousUserMessage) {
        Some(t::red())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::model::{ChannelId, MessageId, Sender, UserId};

    fn test_msg(login: &str, user_id: &str, color: Option<&str>) -> ChatMessage {
        ChatMessage {
            id: MessageId(1),
            server_id: None,
            timestamp: chrono::Utc::now(),
            channel: ChannelId::new("rustlang"),
            sender: Sender {
                user_id: UserId(user_id.to_owned()),
                login: login.to_owned(),
                display_name: login.to_owned(),
                color: color.map(|c| c.to_owned()),
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: "hello".to_owned(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
        }
    }

    #[test]
    fn hot_window_plan_below_trigger_keeps_full_history() {
        let plan = compute_hot_window_plan(550, false, true, HOT_WINDOW_ROWS);

        assert_eq!(plan.active_start, 0);
        assert_eq!(plan.active_len, 550);
        assert_eq!(plan.hidden_rows, 0);
        assert!(!plan.has_boundary());
    }

    #[test]
    fn hot_window_plan_when_following_bottom_compacts_to_recent_rows() {
        let plan = compute_hot_window_plan(900, false, true, HOT_WINDOW_ROWS);

        assert_eq!(plan.active_start, 500);
        assert_eq!(plan.active_len, HOT_WINDOW_ROWS);
        assert_eq!(plan.hidden_rows, 500);
        assert!(plan.has_boundary());
    }

    #[test]
    fn hot_window_plan_disables_compaction_while_search_is_active() {
        let plan = compute_hot_window_plan(900, true, true, HOT_WINDOW_ROWS);

        assert_eq!(plan.active_start, 0);
        assert_eq!(plan.active_len, 900);
        assert_eq!(plan.hidden_rows, 0);
        assert!(!plan.has_boundary());
    }

    #[test]
    fn first_message_rows_get_background_tint() {
        let flags = MessageFlags {
            is_first_msg: true,
            ..MessageFlags::default()
        };
        let bg = message_row_background(&flags, &MsgKind::Chat, 0.0, false, None);
        assert_ne!(bg, Color32::TRANSPARENT);
    }

    #[test]
    fn first_message_rows_get_left_accent() {
        let flags = MessageFlags {
            is_first_msg: true,
            ..MessageFlags::default()
        };
        assert_eq!(
            message_left_accent_color(&flags, &MsgKind::Chat, false, None),
            Some(t::green())
        );
    }

    #[test]
    fn pinned_message_rows_show_pinned_label() {
        let flags = MessageFlags {
            is_pinned: true,
            ..MessageFlags::default()
        };
        assert_eq!(
            notification_label(&flags, &MsgKind::Chat, false, None),
            Some(("Pinned Message", t::gold()))
        );
    }

    #[test]
    fn custom_reward_highlight_prefers_points_label_and_accent() {
        let flags = MessageFlags {
            is_highlighted: true,
            custom_reward_id: Some("reward-123".to_owned()),
            ..MessageFlags::default()
        };

        assert_eq!(
            notification_label(&flags, &MsgKind::Chat, false, None),
            Some(("Points Highlight", t::accent()))
        );
        assert_eq!(
            message_left_accent_color(&flags, &MsgKind::Chat, false, None),
            Some(t::accent_dim())
        );
    }

    #[test]
    fn channel_points_reward_status_label_reflects_terminal_state() {
        let flags = MessageFlags::default();
        let kind = MsgKind::ChannelPointsReward {
            user_login: "viewer".to_owned(),
            reward_title: "Hydrate".to_owned(),
            cost: 250,
            reward_id: Some("reward-id".to_owned()),
            redemption_id: Some("redeem-id".to_owned()),
            user_input: None,
            status: Some("FULFILLED".to_owned()),
        };

        assert_eq!(
            notification_label(&flags, &kind, false, None),
            Some(("Points Reward Fulfilled", t::green()))
        );
        assert_eq!(
            message_left_accent_color(&flags, &kind, false, None),
            Some(t::green())
        );
    }

    #[test]
    fn keyword_highlight_uses_custom_color_when_present() {
        let flags = MessageFlags::default();
        let custom = t::green().gamma_multiply(0.9);
        assert_eq!(
            message_left_accent_color(&flags, &MsgKind::Chat, true, Some(custom)),
            Some(custom)
        );
        assert_eq!(
            notification_label(&flags, &MsgKind::Chat, true, Some(custom)),
            Some(("Keyword Highlight", custom))
        );
        assert_eq!(
            message_row_background(&flags, &MsgKind::Chat, 0.0, true, Some(custom)),
            t::alpha(custom, 18)
        );
    }

    #[test]
    fn hot_window_expands_in_fixed_chunks() {
        assert_eq!(
            expand_hot_window_rows(HOT_WINDOW_ROWS, 1_500),
            HOT_WINDOW_ROWS + HOT_WINDOW_EXPAND_CHUNK
        );
        assert_eq!(expand_hot_window_rows(1_450, 1_500), 1_500);
    }

    #[test]
    fn anchor_compensation_stays_stable_when_oldest_rows_are_evicted() {
        let anchor = ScrollAnchor {
            message_id: 12,
            distance_to_viewport_top: 10.0,
        };
        let active_ids = vec![12, 13, 14];
        let mut heights = HashMap::new();
        heights.insert(12, 20.0);
        heights.insert(13, 20.0);
        heights.insert(14, 20.0);

        let offset =
            compensate_anchor_offset(&anchor, &active_ids, &heights, COMPACT_BOUNDARY_HEIGHT);

        assert_eq!(offset, Some(COMPACT_BOUNDARY_HEIGHT - 10.0));
    }

    #[test]
    fn snapshot_id_mapping_tracks_shifted_indices_after_eviction() {
        let snapshot = vec![102, 103, 104];
        let live = vec![101, 102, 103, 104, 105];

        let mapped = map_snapshot_ids_to_indices(&snapshot, &live);

        assert_eq!(mapped, vec![1, 2, 3]);
    }

    #[test]
    fn snapshot_id_mapping_drops_missing_messages() {
        let snapshot = vec![100, 101, 102, 103];
        let live = vec![102, 103, 104, 105];

        let mapped = map_snapshot_ids_to_indices(&snapshot, &live);

        assert_eq!(mapped, vec![0, 1]);
    }

    #[test]
    fn collapse_message_for_display_collapses_after_newline_budget() {
        let text = "line1\nline2\nline3\nline4";
        let collapsed = collapse_message_for_display(text, 3);
        assert_eq!(collapsed, Some("line1\nline2\nline3 …".to_owned()));
    }

    #[test]
    fn collapse_message_for_display_uses_soft_wrap_fallback() {
        let text = "x".repeat(1_000);
        let collapsed = collapse_message_for_display(&text, 4);
        assert!(collapsed.is_some());
        let rendered = collapsed.unwrap_or_default();
        assert!(rendered.ends_with('…'));
        assert!(rendered.len() < text.len());
    }

    #[test]
    fn collapse_message_for_display_keeps_short_text_unchanged() {
        let text = "short message";
        assert_eq!(collapse_message_for_display(text, 4), None);
    }

    #[test]
    fn resolve_sender_color_prefers_explicit_tag_color_and_caches_it() {
        let mut cache = HashMap::new();
        let msg = test_msg("alice", "12345", Some("#1E90FF"));

        let color = resolve_sender_color(&msg, &mut cache);

        assert_eq!(color, Color32::from_rgb(0x1E, 0x90, 0xFF));
        assert_eq!(cache.get("alice").copied(), Some(color));
    }

    #[test]
    fn resolve_sender_color_uses_cached_channel_color_when_tag_is_missing() {
        let mut cache = HashMap::new();
        cache.insert("alice".to_owned(), Color32::from_rgb(10, 20, 30));
        let msg = test_msg("alice", "12345", None);

        let color = resolve_sender_color(&msg, &mut cache);

        assert_eq!(color, Color32::from_rgb(10, 20, 30));
    }

    #[test]
    fn resolve_sender_color_forces_opaque_alpha_for_sender_color_tags() {
        let mut cache = HashMap::new();
        let msg = test_msg("alice", "12345", Some("#11223300"));

        let color = resolve_sender_color(&msg, &mut cache);

        assert_eq!(color, Color32::from_rgb(0x11, 0x22, 0x33));
        assert_eq!(cache.get("alice").copied(), Some(color));
    }

    #[test]
    fn resolve_sender_color_falls_back_to_crust_twitch_seed_palette() {
        let mut cache = HashMap::new();
        let msg = test_msg("alice", "12345", None);

        let color = resolve_sender_color(&msg, &mut cache);

        // 12345 % 15 = 0 -> first Twitch fallback color (red).
        assert_eq!(color, Color32::from_rgb(255, 0, 0));
        assert_eq!(cache.get("alice").copied(), Some(color));
    }

    #[test]
    fn sender_name_gradient_maps_first_and_last_stop_to_edges() {
        let stops = vec![
            (0.0, Color32::from_rgb(0xFF, 0x00, 0x00)),
            (1.0, Color32::from_rgb(0x00, 0x00, 0xFF)),
        ];
        assert_eq!(
            gradient_color_at_t(&stops, 0.0, false),
            Color32::from_rgb(0xFF, 0x00, 0x00)
        );
        assert_eq!(
            gradient_color_at_t(&stops, 1.0, false),
            Color32::from_rgb(0x00, 0x00, 0xFF)
        );
    }

    #[test]
    fn sender_name_gradient_interpolates_midpoint_color() {
        let stops = vec![
            (0.0, Color32::from_rgb(0x00, 0x00, 0x00)),
            (1.0, Color32::from_rgb(0xFF, 0xFF, 0xFF)),
        ];
        assert_eq!(
            gradient_color_at_t(&stops, 0.5, false),
            Color32::from_rgb(128, 128, 128)
        );
    }

    #[test]
    fn sender_name_gradient_repeat_wraps_out_of_range_positions() {
        let stops = vec![
            (0.0, Color32::from_rgb(255, 0, 0)),
            (1.0, Color32::from_rgb(0, 0, 255)),
        ];
        // 1.25 wraps to 0.25
        assert_eq!(
            gradient_color_at_t(&stops, 1.25, true),
            gradient_color_at_t(&stops, 0.25, false)
        );
    }

    #[test]
    fn normalize_external_image_url_accepts_protocol_relative_urls() {
        assert_eq!(
            normalize_external_image_url("//cdn.7tv.app/paint/health/1x.webp"),
            Some("https://cdn.7tv.app/paint/health/1x.webp".to_owned())
        );
        assert_eq!(
            normalize_external_image_url("https://cdn.7tv.app/paint/health/1x.webp"),
            Some("https://cdn.7tv.app/paint/health/1x.webp".to_owned())
        );
        assert_eq!(normalize_external_image_url(""), None);
    }
}
