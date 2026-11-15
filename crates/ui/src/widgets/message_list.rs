use crate::app::UiCommandSender;
use crate::image_cache::UiImageCache;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use egui::{Color32, Id, Label, RichText, ScrollArea, Ui, Vec2};

use crust_core::{
    events::{AppCommand, LinkPreview},
    model::{
        filters::FilterAction, Badge, ChannelId, ChatMessage, LowTrustStatus, MessageFlags,
        MessageId, MsgKind, ReplyInfo, Span,
    },
};

use crate::theme as t;
use crate::widgets::message_search::MessageSearchState;
use crate::widgets::name_paint::show_sender_name;
use crate::widgets::sender_cosmetics;

mod preview;
pub use preview::{MessagePreviewActions, MessagePreviewContext};
pub(crate) use preview::{MessagePreviewCache, MessagePreviewResult};
#[cfg(test)]
pub(crate) use preview::PreviewTestState;

/// Returned from [`MessageList::show`].
pub struct MessageListResult {
    /// Set when the user right-clicked a message and chose "Reply".
    pub reply: Option<ReplyInfo>,
    /// A message snapshot keeps historical thread roots available after eviction.
    pub open_thread: Option<ChatMessage>,
    /// Set when a username was clicked: (login, sender_badges).
    pub profile_request: Option<(String, Vec<Badge>)>,
    /// Lightweight per-frame counters for the debug performance overlay.
    pub perf_stats: MessageListPerfStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageListPerfStats {
    /// Messages in the channel's retained deque, before filtering or pausing.
    pub retained_rows: usize,
    /// Rows remaining after filters and any frozen reading snapshot.
    pub eligible_rows: usize,
    pub active_rows: usize,
    pub rendered_rows: usize,
    pub boundary_hidden_rows: usize,
    pub prefix_rebuilt: bool,
    pub height_cache_misses: usize,
}

#[derive(Clone, Copy)]
struct MessageListView {
    pane: u64,
    last_pass: u64,
}

fn view_state_id(channel: &ChannelId, pane: Option<u64>, name: &str) -> Id {
    let id = Id::new(name).with(channel.as_str());
    pane.map_or(id, |pane| id.with(pane))
}

/// Refresh reply identities after an existing row gains a server ID or sender.
pub(crate) fn invalidate_reply_sender_index(ctx: &egui::Context, channel: &ChannelId) {
    ctx.data_mut(|data| {
        let key = Id::new("reply_sender_revision").with(channel.as_str());
        let revision = data.get_temp::<u64>(key).unwrap_or(0).wrapping_add(1);
        data.insert_temp(key, revision);
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReplyBufferKey {
    first: Option<MessageId>,
    last: Option<MessageId>,
    count: usize,
    revision: u64,
}

#[derive(Clone, Copy)]
struct ReplySenderPosition {
    ordinal: usize,
    id: MessageId,
}

struct ReplySenderRow {
    id: MessageId,
    login: Option<Arc<str>>,
    server_id: Option<Arc<str>>,
}

struct ReplySenderIndex {
    key: ReplyBufferKey,
    // Ordinals keep every map position stable when the deque drops old rows.
    base: usize,
    rows: VecDeque<ReplySenderRow>,
    by_login: HashMap<Arc<str>, ReplySenderPosition>,
    by_server_id: HashMap<Arc<str>, ReplySenderPosition>,
}

fn reply_lookup_key(value: &str) -> Cow<'_, str> {
    let value = value.trim();
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(value.to_ascii_lowercase())
    } else {
        Cow::Borrowed(value)
    }
}

impl ReplySenderIndex {
    fn new(key: ReplyBufferKey, messages: &VecDeque<ChatMessage>) -> Self {
        let mut index = Self {
            key,
            base: 0,
            rows: VecDeque::with_capacity(messages.len()),
            by_login: HashMap::new(),
            by_server_id: HashMap::new(),
        };
        for message in messages {
            index.push(message);
        }
        index
    }

    fn push(&mut self, message: &ChatMessage) {
        let position = ReplySenderPosition {
            ordinal: self.base + self.rows.len(),
            id: message.id,
        };
        let insert = |map: &mut HashMap<Arc<str>, ReplySenderPosition>, value: &str| {
            let value = reply_lookup_key(value);
            if value.is_empty() {
                return None;
            }
            let key = map
                .get_key_value(value.as_ref())
                .map_or_else(|| Arc::<str>::from(value.as_ref()), |(key, _)| key.clone());
            map.insert(key.clone(), position);
            Some(key)
        };
        self.rows.push_back(ReplySenderRow {
            id: message.id,
            login: insert(&mut self.by_login, &message.sender.login),
            server_id: message
                .server_id
                .as_deref()
                .and_then(|id| insert(&mut self.by_server_id, id)),
        });
    }

    /// Reuse retained keys for an append, including rotation of a full deque.
    fn update(&mut self, key: ReplyBufferKey, messages: &VecDeque<ChatMessage>) -> bool {
        if self.key.revision != key.revision || self.rows.is_empty() || messages.is_empty() {
            return false;
        }
        let Some(dropped) = self.rows.iter().position(|row| Some(row.id) == key.first) else {
            return false;
        };
        let retained = self.rows.len() - dropped;
        if messages.len() < retained
            || messages.get(retained - 1).map(|message| message.id) != self.key.last
            || self
                .base
                .checked_add(dropped)
                .and_then(|base| base.checked_add(messages.len()))
                .is_none()
        {
            return false;
        }
        for _ in 0..dropped {
            let row = self.rows.pop_front().unwrap();
            if let Some(login) = row.login {
                if self
                    .by_login
                    .get(&login)
                    .is_some_and(|position| position.ordinal == self.base)
                {
                    self.by_login.remove(&login);
                }
            }
            if let Some(id) = row.server_id {
                if self
                    .by_server_id
                    .get(&id)
                    .is_some_and(|position| position.ordinal == self.base)
                {
                    self.by_server_id.remove(&id);
                }
            }
            self.base += 1;
        }
        for message in messages.iter().skip(retained) {
            self.push(message);
        }
        self.key = key;
        true
    }

    fn message<'a>(
        &self,
        messages: &'a VecDeque<ChatMessage>,
        reply: &ReplyInfo,
    ) -> Option<&'a ChatMessage> {
        let login = reply_lookup_key(&reply.parent_user_login);
        let position = if login.is_empty() {
            self.by_server_id
                .get(reply_lookup_key(&reply.parent_msg_id).as_ref())
        } else {
            self.by_login.get(login.as_ref())
        }?;
        let message = messages.get(position.ordinal.checked_sub(self.base)?)?;
        (message.id == position.id).then_some(message)
    }
}

#[derive(Clone)]
struct ReplySenderCacheEntry {
    channel: ChannelId,
    index: Arc<ReplySenderIndex>,
    used: u64,
}

#[derive(Clone, Default)]
struct ReplySenderCache {
    entries: Vec<ReplySenderCacheEntry>,
}

fn cached_reply_senders(
    ctx: &egui::Context,
    channel: &ChannelId,
    messages: &VecDeque<ChatMessage>,
) -> Arc<ReplySenderIndex> {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|data| {
        let revision = data
            .get_temp::<u64>(Id::new("reply_sender_revision").with(channel.as_str()))
            .unwrap_or(0);
        let key = ReplyBufferKey {
            first: messages.front().map(|message| message.id),
            last: messages.back().map(|message| message.id),
            count: messages.len(),
            revision,
        };
        let cache = data.get_temp_mut_or_default::<ReplySenderCache>(Id::new("reply_sender_cache"));
        if let Some(entry) = cache
            .entries
            .iter_mut()
            .find(|entry| entry.channel == *channel)
        {
            entry.used = pass;
            if entry.index.key != key {
                let updated =
                    Arc::get_mut(&mut entry.index).is_some_and(|index| index.update(key, messages));
                if !updated {
                    entry.index = Arc::new(ReplySenderIndex::new(key, messages));
                }
            }
            return entry.index.clone();
        }
        // Four panes can be visible at once. Navigating channels must not retain
        // an unbounded collection of history indices in egui's temporary data.
        if cache.entries.len() >= 4 {
            let oldest = cache
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(index, _)| index)
                .unwrap();
            cache.entries.remove(oldest);
        }
        let index = Arc::new(ReplySenderIndex::new(key, messages));
        cache.entries.push(ReplySenderCacheEntry {
            channel: channel.clone(),
            index: index.clone(),
            used: pass,
        });
        index
    })
}

/// Discard measurements in each visible pane and mark hidden views for refresh.
pub fn invalidate_message_layouts(ctx: &egui::Context, channel: &ChannelId, ids: &[MessageId]) {
    if ids.is_empty() {
        return;
    }
    let current_pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|data| {
        let revision_key = Id::new("message_layout_revision").with(channel.as_str());
        let revision = data.get_temp::<u64>(revision_key).unwrap_or(0).wrapping_add(1);
        data.insert_temp(revision_key, revision);
        let views_key = Id::new("message_list_views").with(channel.as_str());
        let mut views = data.get_temp::<Vec<MessageListView>>(views_key).unwrap_or_default();
        views.retain(|view| current_pass.saturating_sub(view.last_pass) <= 2);
        for pane in std::iter::once(None).chain(views.iter().map(|view| Some(view.pane))) {
            let heights_key = view_state_id(channel, pane, "msg_row_h");
            if let Some(mut heights) = data.remove_temp::<HashMap<u64, f32>>(heights_key) {
                for id in ids {
                    heights.remove(&id.0);
                }
                data.insert_temp(heights_key, heights);
            }
            data.insert_temp(view_state_id(channel, pane, "msg_prefix_dirty"), true);
            data.insert_temp(view_state_id(channel, pane, "applied_layout_revision"), revision);
        }
        data.insert_temp(views_key, views);
    });
}

// Zero-allocation visible-index abstraction
// When no search filter is active, `VisibleIndices::All(n)` avoids
// allocating a Vec<usize> with one entry per message every frame.

enum VisibleIndices<'a> {
    /// No filter - visible index `i` maps directly to message index `i`.
    All(usize),
    /// Search filter active - sparse set of matching message indices.
    Filtered(Vec<usize>),
    Cached(&'a [usize]),
}

impl VisibleIndices<'_> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::All(n) => *n,
            Self::Filtered(v) => v.len(),
            Self::Cached(v) => v.len(),
        }
    }

    /// Map a virtual row number to the real message index.
    #[inline]
    fn get(&self, i: usize) -> usize {
        match self {
            Self::All(_) => i,
            Self::Filtered(v) => v[i],
            Self::Cached(v) => v[i],
        }
    }

    /// Find the first virtual row matching a predicate on the real index.
    fn position<F: Fn(usize) -> bool>(&self, pred: F) -> Option<usize> {
        match self {
            Self::All(n) => (0..*n).position(|i| pred(i)),
            Self::Filtered(v) => v.iter().position(|&idx| pred(idx)),
            Self::Cached(v) => v.iter().position(|&idx| pred(idx)),
        }
    }

    fn contains_index(&self, index: usize) -> bool {
        match self {
            Self::All(n) => index < *n,
            Self::Filtered(indices) => indices.binary_search(&index).is_ok(),
            Self::Cached(indices) => indices.contains(&index),
        }
    }
}

/// Extra context to locate a reply parent when direct msg-id lookup fails.
#[derive(Clone, Default)]
struct ReplyScrollHint {
    parent_user_login: String,
    parent_msg_body: String,
    child_msg_local_id: u64,
}

/// Pre-computed per-frame values passed to `render_message` to avoid
/// redundant `data_mut` lock acquisitions and `Id::new` constructions.
struct RenderCtx {
    reply_key: Id,
    scroll_to_key: Id,
    scroll_hint_key: Id,
    reply_senders: OnceCell<Arc<ReplySenderIndex>>,
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
const ROW_HEIGHT_CACHE_SLACK: usize = 256;
/// Row left/right padding (px)
const ROW_PAD_X: f32 = 6.0;
/// Row top/bottom padding (px)
const ROW_PAD_Y: f32 = 2.0;
/// Fallback height for rows we have never rendered before.
const EST_H: f32 = 26.0;
// Compaction thresholds tuned so typical chat sessions never hit the
// virtualised "hot window" path. Lowering these aggressively (e.g.
// TRIGGER=200) caused a first-render race: the snap-to-bottom dance
// (`vertical_scroll_offset(f32::MAX)` -> clamped to 0 -> `apply_snap`
// restores `max_scroll` next frame) interacted with repeated prefix
// rebuilds (from just-measured row heights), so the viewport could
// briefly settle in dead space above the hot window. Symptom: the
// channel opens blank until the user clicks "Resume scrolling".
// TRIGGER is deliberately high enough to cover full Twitch backlog
// replays, and ROWS matches the virtual-window test expectations.
const HOT_WINDOW_TRIGGER: usize = 600;
const HOT_WINDOW_ROWS: usize = 400;
const HOT_WINDOW_EXPAND_CHUNK: usize = 200;
const HOT_WINDOW_EXPAND_THRESHOLD_PX: f32 = 24.0;
const COMPACT_BOUNDARY_HEIGHT: f32 = 22.0;

// Large-but-finite "snap to bottom" offset. Using `f32::MAX` here is UNSAFE:
// egui positions content at `inner_rect.min - offset`, which overflows to
// -inf and poisons `content_size` with NaN. The subsequent end-of-frame
// clamp `offset.min(max_scroll)` then no-ops (NaN comparisons are false),
// leaving `offset.y = f32::MAX` *permanently* and the viewport blank until
// something external rewrites the offset (e.g. the Resume-scrolling click).
// 1e9 is >> any realistic chat backlog pixel height but far from float
// overflow, so layout produces finite numbers and clamping works correctly.
const SNAP_TO_BOTTOM_TARGET: f32 = 1.0e9;

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

#[derive(Clone, PartialEq)]
struct RowLayoutKey {
    width: f32,
    pixels_per_point: f32,
    body_font: egui::FontId,
    chat_font_size: f32,
    timestamp_font_size: f32,
    chip_font_size: f32,
    item_spacing: Vec2,
    interact_size: Vec2,
    collapse_lines: Option<usize>,
    show_timestamps: bool,
    show_timestamp_seconds: bool,
    use_24h_timestamps: bool,
    can_moderate: bool,
}

fn prune_row_heights(heights: &mut HashMap<u64, f32>, messages: &VecDeque<ChatMessage>) {
    // Prune in batches so a full chat buffer does not require a scan every frame.
    if heights.len() > messages.len().saturating_add(ROW_HEIGHT_CACHE_SLACK) {
        let retained: HashSet<u64> = messages.iter().map(|message| message.id.0).collect();
        heights.retain(|id, _| retained.contains(id));
    }
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

#[derive(Clone, Default)]
struct PausedIndexCache {
    snapshot_ids: Vec<u64>,
    live_ids: Vec<u64>,
    indices: Vec<usize>,
}

impl PausedIndexCache {
    fn refresh(&mut self, snapshot_ids: &[u64], messages: &VecDeque<ChatMessage>) -> bool {
        if self.snapshot_ids == snapshot_ids
            && self.live_ids.len() == messages.len()
            && self.live_ids.iter().zip(messages).all(|(id, message)| *id == message.id.0)
        {
            return false;
        }
        self.snapshot_ids.clear();
        self.snapshot_ids.extend_from_slice(snapshot_ids);
        self.live_ids.clear();
        self.live_ids.extend(messages.iter().map(|message| message.id.0));
        self.indices = map_snapshot_ids_to_indices(snapshot_ids, &self.live_ids);
        true
    }
}

#[derive(Clone, Copy)]
pub struct KeywordHighlightMatch {
    pub color: Option<egui::Color32>,
}

/// Scrollable, bottom-anchored list of chat messages with inline emote images.
pub struct MessageList<'a> {
    messages: &'a VecDeque<ChatMessage>,
    /// Raw image bytes keyed by CDN URL: CDN url -> (width, height, raw_bytes)
    emote_bytes: &'a UiImageCache,
    /// For sending on-demand image-fetch requests (e.g. HD emote on hover).
    cmd_tx: &'a UiCommandSender,
    /// Channel identifier.
    channel: &'a ChannelId,
    /// Stable pane identity for independent scroll and layout state.
    pane: Option<u64>,
    focused: bool,
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
    /// Structured ignored-user matcher (regex/case-sensitive); hides on top of `ignored_logins`.
    ignored_users: &'a crust_core::ignores::CompiledIgnoredUsers,
    /// Compiled highlight rules used for local keyword highlighting.
    highlight_rules: &'a [crust_core::highlight::HighlightMatch],
    /// Compiled filter records used for hiding messages.
    filter_records: &'a [crust_core::model::filters::CompiledFilter],
    /// Moderation action presets
    mod_action_presets: &'a [crust_core::model::mod_actions::ModActionPreset],
    /// Per-user low-trust treatment for this channel, keyed by lowercased login.
    low_trust_users: &'a HashMap<String, crust_core::model::LowTrustEntry>,
    /// When `Some`, scroll to the message with this id on this frame.
    scroll_to: Option<MessageId>,
    /// When true, link preview tooltips are suppressed (streamer mode).
    hide_link_previews: bool,
    /// Optional "live" flag for this channel, exposed to filter expressions
    /// as `channel.live`. `None` leaves the identifier evaluating to `false`.
    channel_live: Option<bool>,
    /// Whether this channel is the currently-watched stream, exposed to
    /// filter expressions as `channel.watching`.
    channel_watching: bool,
    /// Search and user-card rows keep their interactions separate from chat panes.
    preview_id: Option<Id>,
}

impl<'a> MessageList<'a> {
    fn message_filter_action(&self, msg: &ChatMessage) -> Option<FilterAction> {
        if self.filter_records.is_empty() {
            return None;
        }

        crust_core::model::filters::check_filters_message(
            self.filter_records,
            Some(self.channel),
            msg,
            self.channel.display_name(),
            self.channel_live,
            self.channel_watching,
        )
    }

    pub(crate) fn new(
        messages: &'a VecDeque<ChatMessage>,
        emote_bytes: &'a UiImageCache,
        cmd_tx: &'a UiCommandSender,
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
        ignored_users: &'a crust_core::ignores::CompiledIgnoredUsers,
        highlight_rules: &'a [crust_core::highlight::HighlightMatch],
        filter_records: &'a [crust_core::model::filters::CompiledFilter],
        mod_action_presets: &'a [crust_core::model::mod_actions::ModActionPreset],
        low_trust_users: &'a HashMap<String, crust_core::model::LowTrustEntry>,
    ) -> Self {
        Self {
            messages,
            emote_bytes,
            cmd_tx,
            channel,
            pane: None,
            focused: true,
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
            ignored_users,
            highlight_rules,
            filter_records,
            mod_action_presets,
            low_trust_users,
            scroll_to: None,
            hide_link_previews: false,
            channel_live: None,
            channel_watching: false,
            preview_id: None,
        }
    }

    fn state_id(&self, name: &str) -> Id {
        self.preview_id.map_or_else(
            || view_state_id(self.channel, self.pane, name),
            |id| id.with(name),
        )
    }

    /// Give duplicate channel panes independent reading positions and measurements.
    pub fn with_pane(mut self, pane: u64, focused: bool) -> Self {
        self.pane = Some(pane);
        self.focused = focused;
        self
    }

    /// Preserve a reading position when a split becomes the only remaining pane.
    pub fn with_optional_pane(mut self, pane: Option<u64>, focused: bool) -> Self {
        self.pane = pane;
        self.focused = focused;
        self
    }

    /// Request scrolling to a specific message id on this frame.
    pub fn with_scroll_to(mut self, id: Option<MessageId>) -> Self {
        self.scroll_to = id;
        self
    }

    /// Suppress link preview hover tooltips for this frame (streamer mode).
    pub fn with_hide_link_previews(mut self, hide: bool) -> Self {
        self.hide_link_previews = hide;
        self
    }

    /// Expose the current stream live/watching state to filter expressions.
    pub fn with_channel_status(mut self, live: Option<bool>, watching: bool) -> Self {
        self.channel_live = live;
        self.channel_watching = watching;
        self
    }

    /// Render the message list with auto-scroll behaviour.
    ///
    /// * Auto-scrolls to the bottom when new messages arrive.
    /// * Pauses auto-scroll when the user scrolls up.
    /// * Shows a floating "↓ Resume scrolling" button while paused.
    /// * Returns a [`MessageListResult`] that may contain a reply request.
    pub fn show(&self, ui: &mut Ui) -> MessageListResult {
        let current_pass = ui.ctx().cumulative_pass_nr();
        let missed_layout_update = ui.ctx().data_mut(|data| {
            if let Some(pane) = self.pane {
                let key = Id::new("message_list_views").with(self.channel.as_str());
                let mut views = data.get_temp::<Vec<MessageListView>>(key).unwrap_or_default();
                views.retain(|view| view.pane != pane && current_pass.saturating_sub(view.last_pass) <= 2);
                views.push(MessageListView { pane, last_pass: current_pass });
                if views.len() > 64 {
                    views.remove(0);
                }
                data.insert_temp(key, views);
            }
            let revision = data.get_temp::<u64>(Id::new("message_layout_revision").with(self.channel.as_str())).unwrap_or(0);
            let key = self.state_id("applied_layout_revision");
            let changed = data.get_temp::<u64>(key).unwrap_or(0) != revision;
            data.insert_temp(key, revision);
            changed
        });
        let reply_key = self.state_id("ml_reply_req");
        let user_color_cache_key = Id::new("ml_user_color_cache").with(self.channel.as_str());
        // PERF: remove_temp transfers ownership so we don't clone the entire
        // HashMap every frame.  The cache is re-inserted at the end of show().
        // On busy channels (1000+ distinct senders) this alone can save
        // several hundred μs per frame.
        let mut user_color_cache: HashMap<String, Color32> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(user_color_cache_key).unwrap_or_default());
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
                // ignored_logins is always lowercase (populated via
                // parse_settings_lines with lowercase=true).  Skip the
                // allocating `to_ascii_lowercase()` when the login already
                // is lowercase, which is the overwhelmingly common case
                // on Twitch/IRC.
                let hit = if login.bytes().any(|b| b.is_ascii_uppercase()) {
                    self.ignored_logins.contains(&login.to_ascii_lowercase())
                } else {
                    self.ignored_logins.contains(login)
                };
                if hit {
                    return false;
                }
            }

            if !self.ignored_users.is_empty() && self.ignored_users.is_ignored(&msg.sender.login) {
                return false;
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
            _ if self.ignored_logins.is_empty()
                && self.ignored_users.is_empty()
                && self.filter_records.iter().all(|filter| filter.action != FilterAction::Hide) =>
            {
                VisibleIndices::All(self.messages.len())
            }
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
        let scroll_id = self.state_id("message_list");
        let paused_key = self.state_id("scroll_paused");
        let paused_snapshot_key = self.state_id("paused_snapshot_ids");

        // "Force snap" signal from the app layer: set whenever the user
        // explicitly activates this channel (tab click, keyboard nav,
        // sidebar click, etc.).  Drain it here and treat it as an
        // authoritative request to show the latest messages - overrides
        // any stale paused state that was frozen in from a previous
        // visit.  This complements the cumulative_pass_nr heuristic below
        // and closes the "channel opens black until I click Resume
        // scrolling" bug class for cases the heuristic can miss.
        let force_snap_key = egui::Id::new("ml_force_snap").with(self.channel.as_str());
        let force_snap: bool = ui
            .ctx()
            .data_mut(|d| self.focused && d.remove_temp(force_snap_key).unwrap_or(false));
        if force_snap {
            ui.ctx().data_mut(|d| {
                d.insert_temp(paused_key, false);
                d.remove_temp::<Vec<u64>>(paused_snapshot_key);
            });
        }

        let paused_before_show: bool = ui
            .ctx()
            .data_mut(|d| d.get_temp(paused_key).unwrap_or(false));

        // Paused snapshot behavior:
        // freeze the rendered message-id set while paused so incoming chat does
        // not keep shifting the lines the user is reading.
        let paused_index_key = self.state_id("paused_snapshot_indices");
        let (mut snapshot_ids, mut paused_indices): (Vec<u64>, PausedIndexCache) = ui.ctx().data_mut(|data| {
            (data.remove_temp(paused_snapshot_key).unwrap_or_default(),
             data.remove_temp(paused_index_key).unwrap_or_default())
        });
        let use_snapshot = paused_before_show
            && !search_filtering
            && self.scroll_to.is_none();
        let visible_indices: VisibleIndices = if use_snapshot {
            if snapshot_ids.is_empty() {
                snapshot_ids = self.messages.iter().map(|message| message.id.0).collect();
            }
            paused_indices.refresh(&snapshot_ids, self.messages);
            if paused_indices.indices.is_empty() {
                snapshot_ids.clear();
                live_visible_indices
            } else if matches!(live_visible_indices, VisibleIndices::All(_)) {
                VisibleIndices::Cached(&paused_indices.indices)
            } else {
                VisibleIndices::Filtered(paused_indices.indices.iter().copied()
                    .filter(|&index| live_visible_indices.contains_index(index)).collect())
            }
        } else {
            live_visible_indices
        };
        let n = visible_indices.len();
        let paused_snapshot_new_rows = live_visible_count.saturating_sub(n);

        // Pause before applying a wheel gesture over chat. A window covering
        // the panel owns its input and must not pause the conversation beneath it.
        let wheel_over_panel = ui.rect_contains_pointer(panel_rect)
            && ui.input(|input| input.raw_scroll_delta.y.abs() > 0.0);
        // Store whether the wheel was used this frame, so show_resume_button
        // doesn't immediately clear the paused flag before the delta is applied.
        let wheel_key = self.state_id("scroll_wheel_this_frame");
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
        let init_key = self.state_id("msg_list_init");
        let first_render = !ui.ctx().data_mut(|d| {
            let seen: bool = d.get_temp(init_key).unwrap_or(false);
            if !seen {
                d.insert_temp(init_key, true);
            }
            seen
        });

        // Channel re-entry detector. `first_render` only fires the very
        // first time a channel is ever visited in this session; clicking
        // back into a channel you already opened once would otherwise
        // inherit whatever scroll offset egui persisted for it - which
        // on stale or never-populated state is 0 (top of an empty
        // buffer), making the panel render as dead space until the user
        // clicks "Resume scrolling". We fix this by detecting that this
        // message list wasn't rendered on the immediately-preceding
        // pass: that's the signal that the user just made this channel
        // active again, and (if they weren't intentionally scrolled up
        // when they last left) we should re-snap to the bottom.
        let last_pass_key = self.state_id("ml_last_pass");
        let current_pass = ui.ctx().cumulative_pass_nr();
        let last_pass: u64 = ui
            .ctx()
            .data_mut(|d| d.get_temp(last_pass_key).unwrap_or(0));
        let reentered_channel = last_pass != 0 && current_pass.saturating_sub(last_pass) > 1;
        ui.ctx()
            .data_mut(|d| d.insert_temp(last_pass_key, current_pass));
        // "Needs snap" covers four cases:
        //   1. Explicit user activation (`force_snap`) - tab click,
        //      keyboard nav, sidebar click.  Authoritative, overrides
        //      paused state.
        //   2. Brand-new channel (`first_render`).
        //   3. User just clicked back into a channel they left unpaused
        //      (`reentered_channel && !paused_before_show`).  Safety net
        //      for activation paths that bypass `force_snap`.
        //   4. Stick-to-bottom can't rescue a stale persisted offset on
        //      its own because egui only re-sticks when the scroll was
        //      already at the bottom on the previous frame.
        let needs_snap_to_bottom =
            force_snap || first_render || (reentered_channel && !paused_before_show);

        // Threshold: below this, render all rows directly (no virtual scrolling).
        // This avoids height-estimation and stale-offset edge cases that cause
        // layout glitches with very few messages.  Kept low so virtual
        // scrolling kicks in early and only visible rows are rendered.
        const VIRTUAL_THRESHOLD: usize = 40;

        // Height cache
        // Keyed by MessageId (u64). Persisted in egui temp storage so that
        // off-screen rows are not re-measured every frame.  Shared between
        // the simple and virtual paths so heights stay consistent.
        //
        // PERF: use remove_temp (ownership via mem::take) instead of get_temp
        // (which clones the entire HashMap every frame).  The cache is put
        // back via insert_temp at the end of each path.
        let hc_id = self.state_id("msg_row_h");
        let mut height_cache: std::collections::HashMap<u64, f32> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(hc_id).unwrap_or_default());

        let layout_key = self.state_id("msg_row_layout");
        let layout = RowLayoutKey {
            width: ui.available_width(),
            pixels_per_point: ui.ctx().pixels_per_point(),
            body_font: ui.style().override_font_id.clone().unwrap_or_else(|| {
                ui.style()
                    .override_text_style
                    .clone()
                    .unwrap_or(egui::TextStyle::Body)
                    .resolve(ui.style())
            }),
            chat_font_size: t::chat_font_size(),
            timestamp_font_size: t::timestamps_font_size(),
            chip_font_size: t::chips_font_size(),
            item_spacing: ui.spacing().item_spacing,
            interact_size: ui.spacing().interact_size,
            collapse_lines: self.collapse_long_messages.then_some(self.collapse_long_message_lines),
            show_timestamps: self.show_timestamps,
            show_timestamp_seconds: self.show_timestamp_seconds,
            use_24h_timestamps: self.use_24h_timestamps,
            can_moderate: self.can_moderate,
        };
        let layout_changed = ui.ctx().data_mut(|d| {
            let changed = d.get_temp::<RowLayoutKey>(layout_key).as_ref() != Some(&layout);
            if changed {
                d.insert_temp(layout_key, layout);
            }
            changed
        }) || missed_layout_update;
        if layout_changed {
            height_cache.clear();
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
        let scroll_to_key = self.state_id("ml_scroll_to");
        let scroll_hint_key = self.state_id("ml_scroll_hint");
        let highlight_key = self.state_id("ml_highlight_msg");
        let highlight_time_key = self.state_id("ml_highlight_t");
        let hot_state_key = self.state_id("ml_hot_window");
        let anchor_key = self.state_id("ml_scroll_anchor");
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
            // Also consider an externally-requested scroll_to (e.g. from global search).
            let idx = idx.or_else(|| {
                self.scroll_to.and_then(|target_id| {
                    visible_indices.position(|msg_idx| self.messages[msg_idx].id == target_id)
                })
            });
            idx.map(|idx| {
                // A requested row must stay in view when new chat arrives.
                ui.ctx().data_mut(|d| d.insert_temp(paused_key, true));
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
            search_filtering || scroll_target_pending || self.scroll_to.is_some(),
            !paused_before_show,
            hot_state.active_rows,
        );
        let active_ids_key = self.state_id("msg_active_ids");
        let mut active_ids: Vec<u64> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(active_ids_key).unwrap_or_default());
        let active_id = |i: usize| {
            self.messages[visible_indices.get(hot_plan.active_start + i)].id.0
        };
        let active_ids_changed = active_ids.len() != hot_plan.active_len
            || active_ids.iter().enumerate().any(|(i, id)| *id != active_id(i));
        if active_ids_changed {
            active_ids.clear();
            active_ids.extend((0..hot_plan.active_len).map(active_id));
        }
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
            retained_rows: self.messages.len(),
            eligible_rows: n,
            active_rows: hot_plan.active_len,
            rendered_rows: 0,
            boundary_hidden_rows: hot_plan.hidden_rows,
            prefix_rebuilt: false,
            height_cache_misses: 0,
        };

        // Pre-compute highlight state once per frame
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
                reply_senders: OnceCell::new(),
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
                d.insert_temp(hot_state_key, HotWindowState { active_rows: 0 });
                d.insert_temp(user_color_cache_key, user_color_cache);
                d.insert_temp(active_ids_key, active_ids);
                d.remove_temp::<ScrollAnchor>(anchor_key);
                if use_snapshot {
                    d.insert_temp(paused_snapshot_key, snapshot_ids);
                    d.insert_temp(paused_index_key, paused_indices);
                }
            });
            self.show_resume_button(ui, &output, panel_rect, paused_snapshot_new_rows);
            self.apply_snap(ui, &output);
            return MessageListResult {
                reply: self.take_reply(ui, rctx.reply_key),
                open_thread: self.take_thread_request(ui),
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
            } else if needs_snap_to_bottom {
                // Snap to the bottom so the most-recent content is in
                // view, overriding whatever offset egui had persisted
                // from a previous visit. See `SNAP_TO_BOTTOM_TARGET`
                // for why this is a large finite value, not f32::MAX.
                //
                // State is clamped to the real `max_scroll` at the END
                // of the frame, so *this* frame's render is still blank
                // and the *next* frame renders correctly - which is why
                // we also request a repaint below and kick `apply_snap`
                // into a keep-rewriting loop for any frames where new
                // content arrives before stick_to_bottom engages.
                sa = sa.vertical_scroll_offset(SNAP_TO_BOTTOM_TARGET);
                let snap_key = self.state_id("snap_to_bottom");
                ui.ctx().data_mut(|d| d.insert_temp(snap_key, true));
                ui.ctx().request_repaint();
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
            prune_row_heights(&mut height_cache, self.messages);
            ui.ctx().data_mut(|d| {
                d.insert_temp(hc_id, height_cache);
                d.insert_temp(hot_state_key, HotWindowState { active_rows: 0 });
                d.insert_temp(user_color_cache_key, user_color_cache);
                d.insert_temp(active_ids_key, active_ids);
                if use_snapshot {
                    d.insert_temp(paused_snapshot_key, snapshot_ids);
                    d.insert_temp(paused_index_key, paused_indices);
                }
            });
            perf_stats.rendered_rows = hot_plan.active_len;
            return MessageListResult {
                reply: self.take_reply(ui, rctx.reply_key),
                open_thread: self.take_thread_request(ui),
                profile_request: self.take_profile_request(ui),
                perf_stats,
            };
        }

        // prefix[i] is the y-offset of row i. Reuse both vectors and compare the
        // complete row order: filters can change interior rows without changing
        // the count or either endpoint.
        let ps_id = self.state_id("msg_prefix_sum");
        let ps_dirty_id = self.state_id("msg_prefix_dirty");
        let mut prefix: Vec<f32> = ui
            .ctx()
            .data_mut(|d| d.remove_temp(ps_id).unwrap_or_default());
        let heights_changed: bool = ui
            .ctx()
            .data_mut(|d| d.remove_temp(ps_dirty_id).unwrap_or(false));
        let prefix_stale = layout_changed
            || active_ids_changed
            || heights_changed
            || prefix.len() != hot_plan.active_len + 1;
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
        }
        perf_stats.prefix_rebuilt = prefix_stale;
        let boundary_h = hot_plan.boundary_height();
        let total_h = boundary_h + *prefix.last().unwrap_or(&0.0);

        // Virtual-scrolling render pass
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
        } else if needs_snap_to_bottom {
            // Same rationale as the simple-path branch above: use a
            // large finite offset to request snap, set the snap key so
            // `apply_snap` re-writes `max_scroll` next frame, and force
            // a repaint so the second (correctly-rendered) frame
            // actually runs instead of leaving the user stuck on this
            // frame's off-screen render.
            sa = sa.vertical_scroll_offset(SNAP_TO_BOTTOM_TARGET);
            let snap_key = self.state_id("snap_to_bottom");
            ui.ctx().data_mut(|d| d.insert_temp(snap_key, true));
            ui.ctx().request_repaint();
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
                ui.ctx()
                    .data_mut(|d| d.insert_temp(ps_dirty_id, true));
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
        prune_row_heights(&mut height_cache, self.messages);
        // Persist height cache and prefix-sum Vec for next frame.
        ui.ctx().data_mut(|d| {
            d.insert_temp(hc_id, height_cache);
            d.insert_temp(ps_id, prefix);
            d.insert_temp(user_color_cache_key, user_color_cache);
            d.insert_temp(active_ids_key, active_ids);
            if use_snapshot {
                d.insert_temp(paused_snapshot_key, snapshot_ids);
                d.insert_temp(paused_index_key, paused_indices);
            }
        });
        MessageListResult {
            reply: self.take_reply(ui, rctx.reply_key),
            open_thread: self.take_thread_request(ui),
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
            egui::Stroke::new(1.0_f32, stroke),
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

    fn take_thread_request(&self, ui: &Ui) -> Option<ChatMessage> {
        let key = self.state_id("open_thread");
        ui.ctx().data_mut(|data| {
            let message = data.get_temp::<ChatMessage>(key);
            data.remove::<ChatMessage>(key);
            message
        })
    }

    /// Read and clear the profile-request stored by a username click this frame.
    fn take_profile_request(&self, ui: &Ui) -> Option<(String, Vec<Badge>)> {
        let key = self.state_id("ml_profile_req");
        ui.ctx().data_mut(|d| {
            let v: Option<(String, Vec<Badge>)> = d.get_temp(key);
            if v.is_some() {
                d.remove::<(String, Vec<Badge>)>(key);
            }
            v
        })
    }

    fn reply_info_for_message(msg: &ChatMessage) -> Option<ReplyInfo> {
        ReplyInfo::for_message(msg)
    }

    fn reply_sender_message(
        &self,
        ctx: &egui::Context,
        rctx: &RenderCtx,
        reply: &ReplyInfo,
    ) -> Option<&ChatMessage> {
        // Prefer the latest retained appearance so live paint changes also reach
        // replies to older messages. The shared index stores identities only.
        rctx.reply_senders
            .get_or_init(|| cached_reply_senders(ctx, self.channel, self.messages))
            .message(self.messages, reply)
    }

    fn render_reply_preview(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        reply: &ReplyInfo,
        rctx: &RenderCtx,
        user_color_cache: &mut HashMap<String, Color32>,
    ) -> egui::Response {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.style_mut().override_font_id = Some(t::small());
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            let (stripe, _) = ui.allocate_exact_size(egui::vec2(2.0, 12.0), egui::Sense::hover());
            ui.painter().rect_filled(stripe, 0.0, t::accent());
            let body = if let Some((cut, _)) = reply.parent_msg_body.char_indices().nth(80) {
                format!("{}...", &reply.parent_msg_body[..cut])
            } else {
                reply.parent_msg_body.clone()
            };
            let reply_color = t::text_secondary();
            let arrow = ui.add(
                Label::new(RichText::new("↩").color(reply_color))
                    .selectable(false)
                    .sense(egui::Sense::click()),
            );
            let parent = self.reply_sender_message(ui.ctx(), rctx, reply);
            let appearance = if msg.channel.is_twitch() {
                let twitch_parent = parent.filter(|parent| parent.channel.is_twitch());
                if let Some(parent) = twitch_parent {
                    sender_cosmetics::remember_sender(ui.ctx(), &parent.sender);
                }
                sender_cosmetics::get(
                    ui.ctx(),
                    twitch_parent.map(|parent| parent.sender.user_id.0.as_str()),
                    &reply.parent_user_login,
                )
            } else {
                None
            };
            let parent_color = appearance
                .as_ref()
                .and_then(|appearance| appearance.color)
                .unwrap_or_else(|| {
                    parent.map_or(reply_color, |parent| {
                        resolve_sender_color(parent, user_color_cache)
                    })
                });
            let parent_paint = match appearance.as_ref() {
                Some(appearance) => appearance.paint.as_deref(),
                None => parent.and_then(|parent| parent.sender.name_paint.as_ref()),
            };
            let display_name = if reply.parent_display_name.is_empty() {
                &reply.parent_user_login
            } else {
                &reply.parent_display_name
            };
            // Leave space for the quoted message even when the name is long.
            let sender_width =
                ((ui.available_width() - ui.spacing().item_spacing.x) * 0.5).max(0.0);
            let sender = ui
                .allocate_ui_with_layout(
                    egui::vec2(sender_width, t::small().size),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        show_sender_name(
                            ui,
                            display_name,
                            parent_color,
                            parent_paint,
                            self.emote_bytes,
                            self.animate_emotes,
                        )
                    },
                )
                .inner;
            let body = ui.add(
                Label::new(
                    RichText::new(format!(": {body}"))
                        .font(t::small())
                        .color(reply_color)
                        .italics(),
                )
                .selectable(false)
                .sense(egui::Sense::click())
                .truncate(),
            );
            let response = arrow.union(sender).union(body);
            response.context_menu(|ui| self.show_message_context_menu(ui, msg, rctx.reply_key));
            if response.clicked() {
                let target = reply.parent_msg_id.trim().to_owned();
                let hint = ReplyScrollHint {
                    parent_user_login: reply.parent_user_login.clone(),
                    parent_msg_body: reply.parent_msg_body.clone(),
                    child_msg_local_id: msg.id.0,
                };
                ui.ctx().data_mut(|data| {
                    if !target.is_empty() {
                        data.insert_temp(rctx.scroll_to_key, target);
                    }
                    data.insert_temp(rctx.scroll_hint_key, hint);
                });
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            response
        })
        .inner
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

        if msg.channel.is_twitch() && resolved_server_id.is_some() && ui.button("Open thread").clicked() {
            let mut seed = msg.clone();
            seed.server_id = resolved_server_id.clone();
            ui.ctx().data_mut(|data| data.insert_temp(self.state_id("open_thread"), seed));
            ui.close_menu();
        }

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
            ui.ctx().copy_text(msg.plain_text().into_owned());
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
                let key = self.state_id("ml_profile_req");
                ui.ctx().data_mut(|data| {
                    data.insert_temp(key, (msg.sender.login.clone(), msg.sender.badges.clone()));
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
                        composer: None,
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
                                    composer: None,
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
        let snap_key = self.state_id("snap_to_bottom");
        let snapping: bool = ui.ctx().data_mut(|d| d.get_temp(snap_key).unwrap_or(false));
        if !snapping {
            return;
        }

        // If user has scrolled up, never keep forcing snap-to-bottom.
        let paused_key = self.state_id("scroll_paused");
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

        // Safety guard: if layout produced non-finite numbers (NaN/Inf),
        // `at_bottom` can read as true even though offset is garbage,
        // which would prematurely clear the snap flag and strand the
        // viewport in dead space. Rewrite offset to 0 (a safe, always-
        // visible position) and keep snapping until numbers recover.
        if !output.state.offset.y.is_finite() || !output.content_size.y.is_finite() {
            let mut state = output.state;
            state.offset.y = 0.0;
            state.store(ui.ctx(), output.id);
            ui.ctx().request_repaint();
            return;
        }

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

        let paused_key = self.state_id("scroll_paused");
        let wheel_key = self.state_id("scroll_wheel_this_frame");
        let prev_offset_key = self.state_id("scroll_prev_offset");
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
            // was_paused and no explicit un-pause action -> stay paused.
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
                trimmed.push_str("...");
                trimmed
            } else {
                raw_text
            };
            painter.text(
                paused_rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                t::popups_font(),
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
                egui::Stroke::new(1.0_f32, t::accent()),
                egui::epaint::StrokeKind::Outside,
            );
            // Button label
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Resume scrolling",
                t::popups_font(),
                t::text_on_accent(),
            );

            // Detect click on the painted rect
            let btn_response = ui.interact(
                btn_rect,
                self.state_id("resume_scroll_btn"),
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
                let snap_key = self.state_id("snap_to_bottom");
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
        user_color_cache: &mut HashMap<String, Color32>,
    ) {
        if msg.channel.is_twitch() {
            sender_cosmetics::remember_sender(ui.ctx(), &msg.sender);
        }
        // Dispatch non-chat (and non-bits) events to the compact system-event renderer.
        match &msg.msg_kind {
            MsgKind::Chat | MsgKind::Bits { .. } => {}
            MsgKind::SuspiciousUserMessage => {
                self.render_suspicious_user_message(
                    ui,
                    msg,
                    dimmed,
                    rctx,
                    user_color_cache,
                );
                return;
            }
            _ => {
                self.render_system_event(ui, msg, rctx.reply_key);
                return;
            }
        }

        let reply_key = rctx.reply_key;

        // Message background
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

        let row = message_row_scope(ui, self.state_id("msg_bg_click").with(msg.id.0), self.preview_id.is_none(), |ui| {
            let mut prepared = egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
                .begin(ui);
            {
                let ui = &mut prepared.content_ui;
                // Text selection: opt-in ON globally inside the message row
                // so users can click-drag across timestamp / sender / body to
                // copy. Widgets that must NOT be selectable (badges, emote
                // placeholders, painted sender names) still set
                // `.selectable(false)` explicitly.
                ui.style_mut().interaction.selectable_labels = true;

                if dimmed {
                    ui.set_opacity(if msg.flags.is_history { 0.35 } else { 0.45 });
                }

                // History messages are rendered at reduced opacity so they
                // read as older context while still being fully legible.
                if msg.flags.is_history {
                    ui.set_opacity(if dimmed { 0.35 } else { 0.55 });
                }
                if let Some(ref rep) = msg.reply {
                    self.render_reply_preview(ui, msg, rep, rctx, user_color_cache);
                }
                // Notification banner (pinned / first / highlighted / rewards)
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
                                RichText::new(ts)
                                    .color(t::timestamp())
                                    .font(t::timestamps_font()),
                            ));

                            // Separator dot between timestamp and badges/name
                            ui.add(Label::new(
                                RichText::new("|")
                                    .color(t::separator())
                                    .font(t::timestamps_font()),
                            ));
                        }

                        if let Some(shared) = msg.shared.as_ref() {
                            render_shared_chat_chip(ui, shared, self.emote_bytes);
                        }

                        // Shared messages append distinct source moderator/VIP badges.
                        let shared_badge_tooltip_suffix =
                            msg.shared.as_ref().and_then(|s| {
                                s.display_name
                                    .as_deref()
                                    .filter(|v| !v.is_empty())
                                    .or_else(|| s.login.as_deref().filter(|v| !v.is_empty()))
                            });
                        for (badge, from_source) in message_badges(msg) {
                            let mut tooltip_label =
                                pretty_badge_name(&badge.name, &badge.version);
                            if from_source {
                                if let Some(src) = shared_badge_tooltip_suffix {
                                    tooltip_label = format!("{tooltip_label} ({src})");
                                }
                            }
                            if let Some(url) = &badge.url {
                                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(url.as_str()) {
                                    let size = fit_size(w, h, BADGE_SIZE);
                                    let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
                                    let url_key = super::bytes_uri(url, raw);
                                    // Closures capture by reference - clones
                                    // only happen when the tooltip is actually
                                    // shown (on hover), not every frame.
                                    let badge_response = self.show_image(
                                        ui,
                                        &url_key,
                                        raw,
                                        size,
                                        Some(url.as_str()),
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
                                    message_context_menu(&badge_response, |ui| {
                                        self.show_message_context_menu(ui, msg, reply_key);
                                    });
                                    continue;
                                }
                            }
                            render_badge_fallback(ui, &badge.name, &badge.version, &tooltip_label);
                        }

                        if let Some(entry) = self
                            .low_trust_users
                            .get(&msg.sender.login.to_ascii_lowercase())
                        {
                            render_low_trust_chip(ui, entry.status);
                        }

                        // Sender name - clickable to open user profile card.
                        let appearance = cached_sender_appearance(ui.ctx(), msg);
                        let name_color = appearance
                            .as_ref()
                            .and_then(|appearance| appearance.color)
                            .unwrap_or_else(|| resolve_sender_color(msg, user_color_cache));
                        let name_paint = match appearance.as_ref() {
                            Some(appearance) => appearance.paint.as_deref(),
                            None => msg.sender.name_paint.as_ref(),
                        };
                        let name = if msg.flags.is_action {
                            format!("* {}", msg.sender.display_name)
                        } else {
                            msg.sender.display_name.clone()
                        };
                        let name_resp = show_sender_name(
                            ui,
                            &name,
                            name_color,
                            name_paint,
                            self.emote_bytes,
                            self.animate_emotes,
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
                            let key = self.state_id("ml_profile_req");
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
                                .stroke(egui::Stroke::new(1.0_f32, chip_stroke))
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
                        // Keep the parent's wrapping origin: a child scope here
                        // would indent every body line past the sender prefix.
                        {
                            let previous_spacing = ui.spacing().item_spacing.x;
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
                                let response = ui.add(
                                    Label::new(
                                        RichText::new(format!("✂ {}", deleted_text))
                                            .strikethrough()
                                            .italics()
                                            .color(t::text_secondary()),
                                    )
                                    .wrap()
                                    .sense(pointer_click_sense()),
                                );
                                self.record_preview_click(ui, response.clicked());
                                message_context_menu(&response, |ui| {
                                    self.show_message_context_menu(ui, msg, reply_key)
                                });
                            } else if let Some(collapsed_text) = collapsed_body {
                                // Long-message collapse:
                                // render a compact text summary instead of
                                // laying out all original spans/emotes.
                                let rich = if msg.flags.is_action {
                                    RichText::new(collapsed_text).italics()
                                } else {
                                    RichText::new(collapsed_text)
                                };
                                let resp = ui.add(Label::new(rich).wrap().selectable(true)
                                    .sense(pointer_click_sense()));
                                self.record_preview_click(ui, resp.clicked());
                                message_context_menu(&resp, |ui| {
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
                                self.render_spans(ui, msg, reply_key);
                            }
                            ui.spacing_mut().item_spacing.x = previous_spacing;
                        }
                    },
                );
            }
            let msg_frame_resp = prepared.end(ui);

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
        });
        self.record_preview_click(ui, row.response.clicked());
        message_context_menu(&row.response, |ui| self.show_message_context_menu(ui, msg, reply_key));
    }

    fn message_keyword_highlight(&self, msg: &ChatMessage) -> Option<KeywordHighlightMatch> {
        if self.highlight_rules.is_empty() {
            return None;
        }
        if msg.raw_text.is_empty() {
            return None;
        }
        crust_core::highlight::first_match_context_message(
            self.highlight_rules,
            msg,
            self.channel.display_name(),
            self.channel_live,
            self.channel_watching,
        )
        .map(
            |(color, _show_in_mentions, _has_alert, _has_sound)| KeywordHighlightMatch {
                color: color.map(|[r, g, b]| Color32::from_rgb(r, g, b)),
            },
        )
    }

    /// Render a notice card, followed by a subscriber's message when present.
    fn render_system_event(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        reply_key: Id,
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
                source_login,
            } => {
                let mut text = format!(
                    "🎉  {display_name} is raiding with {viewer_count} viewers!"
                );
                if let Some(login) = source_login
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case(display_name))
                {
                    text.push_str(&format!(" (from {login})"));
                }
                (t::raid_cyan(), Some(text))
            }
            MsgKind::Timeout { login, seconds } => {
                let text = if let Some(text) = moderation_notice_text(msg) {
                    text.to_owned()
                } else {
                    let dur = if *seconds < 60 {
                        format!("{seconds}s")
                    } else if *seconds < 3600 {
                        format!("{}m", seconds / 60)
                    } else {
                        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
                    };
                    format!("⏱  {login} was timed out for {dur}.")
                };
                (t::yellow(), Some(text))
            }
            MsgKind::Ban { login } => (
                t::red(),
                Some(moderation_notice_text(msg).map(str::to_owned)
                    .unwrap_or_else(|| format!("🚫  {login} was permanently banned."))),
            ),
            MsgKind::ChatCleared => (
                t::text_secondary(),
                Some(moderation_notice_text(msg).map(str::to_owned)
                    .unwrap_or_else(|| "🗑  Chat was cleared by a moderator.".to_owned())),
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

        let row = message_row_scope(ui, self.state_id("msg_bg_click").with(msg.id.0), self.preview_id.is_none(), |ui| {
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
                                        RichText::new(ts)
                                            .color(t::timestamp())
                                            .font(t::timestamps_font()),
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
                                    RichText::new(ts)
                                        .color(t::timestamp())
                                        .font(t::timestamps_font()),
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
                        if matches!(&msg.msg_kind, MsgKind::Sub { sub_msg, .. } if !sub_msg.trim().is_empty()) {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                self.render_spans(ui, msg, reply_key);
                            });
                        }
                    }
                });
        });
        self.record_preview_click(ui, row.response.clicked());
        message_context_menu(&row.response, |ui| self.show_message_context_menu(ui, msg, reply_key));
    }

    fn render_spans(&self, ui: &mut Ui, msg: &ChatMessage, reply_key: Id) {
        let clicked = MessageBody {
            emote_bytes: self.emote_bytes,
            cmd_tx: self.cmd_tx,
            link_previews: self.link_previews,
            animate_emotes: self.animate_emotes,
            hide_link_previews: self.hide_link_previews,
        }
        .show(ui, msg, &mut |ui| {
            self.show_message_context_menu(ui, msg, reply_key)
        });
        self.record_preview_click(ui, clicked);
    }

    fn record_preview_click(&self, ui: &Ui, clicked: bool) {
        if clicked && self.preview_id.is_some() {
            ui.ctx().data_mut(|data| data.insert_temp(self.state_id("preview_clicked"), true));
        }
    }

    fn render_suspicious_user_message(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        dimmed: bool,
        rctx: &RenderCtx,
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

        let row = message_row_scope(ui, self.state_id("msg_bg_click").with(msg.id.0), self.preview_id.is_none(), |ui| {
            let mut prepared = egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
                .begin(ui);

            {
                let ui = &mut prepared.content_ui;
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
                                RichText::new(ts).color(t::timestamp()).font(t::timestamps_font()),
                            ));
                            ui.add(Label::new(
                                RichText::new("|").color(t::separator()).font(t::timestamps_font()),
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
                                        .stroke(egui::Stroke::new(1.0_f32, stroke_color))
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
                                    composer: None,
                                });
                            }
                        }

                        let low_trust = self
                            .low_trust_users
                            .get(&msg.sender.login.to_ascii_lowercase())
                            .map(|e| e.status)
                            .unwrap_or(LowTrustStatus::Restricted);
                        render_low_trust_chip(ui, low_trust);

                        if let Some(shared) = msg.shared.as_ref() {
                            render_shared_chat_chip(ui, shared, self.emote_bytes);
                        }

                        let name = if msg.flags.is_action {
                            format!("* {}:", msg.sender.display_name)
                        } else {
                            format!("{}:", msg.sender.display_name)
                        };
                        let appearance = cached_sender_appearance(ui.ctx(), msg);
                        let name_color = appearance
                            .as_ref()
                            .and_then(|appearance| appearance.color)
                            .unwrap_or_else(|| resolve_sender_color(msg, user_color_cache));
                        let name_paint = match appearance.as_ref() {
                            Some(appearance) => appearance.paint.as_deref(),
                            None => msg.sender.name_paint.as_ref(),
                        };
                        let name_resp = show_sender_name(
                            ui,
                            &name,
                            name_color,
                            name_paint,
                            self.emote_bytes,
                            self.animate_emotes,
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
                            let key = self.state_id("ml_profile_req");
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

                        // Body continuation lines share the full message width.
                        {
                            let previous_spacing = ui.spacing().item_spacing.x;
                            ui.spacing_mut().item_spacing.x = 0.0;
                            if msg.flags.is_deleted {
                                ui.add(
                                    Label::new(
                                        RichText::new(format!("✂ {}", msg.raw_text))
                                            .strikethrough()
                                            .italics()
                                            .color(t::text_secondary()),
                                    )
                                    .wrap(),
                                );
                            } else {
                                self.render_spans(ui, msg, reply_key);
                            }
                            ui.spacing_mut().item_spacing.x = previous_spacing;
                        }
                    },
                );
            }
            let msg_frame_resp = prepared.end(ui);
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
        self.record_preview_click(ui, row.response.clicked());
        message_context_menu(&row.response, |ui| self.show_message_context_menu(ui, msg, reply_key));
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
    ) -> egui::Response {
        show_message_image(
            ui,
            uri,
            raw,
            size,
            url_hint,
            self.emote_bytes,
            self.animate_emotes,
        )
    }
}

/// Register a row once, before its children, with an ID unaffected by virtual scrolling.
/// Updating a second click sensor after rendering adds it to Tab order twice.
fn message_row_scope<R>(ui: &mut Ui, id: Id, focusable: bool, contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let mut builder = egui::UiBuilder::new()
        .layer_id(ui.layer_id())
        .max_rect(ui.available_rect_before_wrap())
        .layout(*ui.layout())
        .style(ui.style().clone())
        .sense(if focusable { egui::Sense::click() } else { pointer_click_sense() });
    if !ui.is_enabled() { builder = builder.disabled(); }
    if !ui.is_visible() { builder = builder.invisible(); }
    if ui.is_sizing_pass() { builder = builder.sizing_pass(); }
    let mut row = Ui::new(ui.ctx().clone(), id, builder);
    row.set_clip_rect(ui.clip_rect());
    row.set_opacity(ui.opacity());
    let inner = contents(&mut row);
    let rect = row.min_rect();
    // Dropping the UI updates its original widget's rectangle without registering
    // another focus target, leaving names and links above the background sensor.
    drop(row);
    ui.advance_cursor_after_rect(rect);
    let response = ui.ctx().read_response(id).expect("message row was registered");
    egui::InnerResponse::new(inner, response)
}

fn pointer_click_sense() -> egui::Sense {
    egui::Sense::click().difference(egui::Sense::focusable_noninteractive())
}

fn message_context_menu(response: &egui::Response, contents: impl FnOnce(&mut Ui)) {
    // These sensors already handle clicks. Avoid egui 0.31's context-menu
    // helper registering them again just to add a keyboard focus target.
    let mut menu_response = response.clone();
    menu_response.sense |= egui::Sense::click();
    menu_response.context_menu(contents);
}

/// Shared body rendering for channel chat and cross-channel history views.
#[derive(Clone, Copy)]
pub(crate) struct MessageBody<'a> {
    pub emote_bytes: &'a UiImageCache,
    pub cmd_tx: &'a UiCommandSender,
    pub link_previews: &'a HashMap<String, LinkPreview>,
    pub animate_emotes: bool,
    pub hide_link_previews: bool,
}

impl MessageBody<'_> {
    pub(crate) fn show(
        &self,
        ui: &mut Ui,
        msg: &ChatMessage,
        context_menu: &mut dyn FnMut(&mut Ui),
    ) -> bool {
        let (raw_text, spans) = match &msg.msg_kind {
            MsgKind::Sub { sub_msg, .. } if !sub_msg.trim().is_empty() => {
                let spans = if msg.raw_text == *sub_msg { msg.spans.as_slice() } else { &[] };
                (sub_msg.as_str(), spans)
            }
            _ => (msg.raw_text.as_str(), msg.spans.as_slice()),
        };
        if msg.flags.is_deleted || spans.is_empty() {
            let text = if raw_text.trim().is_empty() {
                "(no text)"
            } else {
                raw_text
            };
            let rich = if msg.flags.is_deleted {
                RichText::new(format!("✂ {text}"))
                    .strikethrough()
                    .italics()
                    .color(t::text_secondary())
            } else if msg.flags.is_action {
                RichText::new(text).italics().color(t::text_secondary())
            } else {
                RichText::new(text)
            };
            let response = ui.add(Label::new(rich).wrap().selectable(true)
                .sense(pointer_click_sense()));
            message_context_menu(&response, context_menu);
            return response.clicked();
        }
        let mut clicked = false;
        for item in crust_core::format::display_spans(spans) {
            match item {
                crust_core::format::DisplaySpan::Single(span) => {
                    clicked |= self.render_span(
                        ui,
                        span,
                        &msg.channel,
                        msg.flags.is_action,
                        context_menu,
                    );
                }
                crust_core::format::DisplaySpan::EmoteLayers(layers) => {
                    let response = show_emote_layers(
                        ui,
                        layers,
                        self.emote_bytes,
                        EMOTE_SIZE,
                        self.animate_emotes,
                    );
                    clicked |= response.clicked();
                    message_context_menu(&response, &mut *context_menu);
                }
            }
        }
        clicked
    }

    fn render_span(
        &self,
        ui: &mut Ui,
        span: &Span,
        channel: &ChannelId,
        is_action: bool,
        context_menu: &mut dyn FnMut(&mut Ui),
    ) -> bool {
        let mut clicked = false;
        let action_color = t::text_secondary();
        match span {
            Span::Text { text, .. } => {
                let cleaned = strip_invisible_chars(text);
                if cleaned.is_empty() {
                    return false;
                }
                let rt = if is_action {
                    RichText::new(&*cleaned).italics().color(action_color)
                } else {
                    RichText::new(&*cleaned)
                };
                let resp = ui.add(Label::new(rt).wrap().selectable(true)
                    .sense(pointer_click_sense()));
                clicked = resp.clicked();
                message_context_menu(&resp, &mut *context_menu);
            }
            Span::Emote {
                url,
                url_hd,
                code,
                provider,
                ..
            } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.peek(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let url_key = super::bytes_uri(url, raw);

                    // Capture shared references - string/Arc clones only
                    // happen when the tooltip is actually shown (on hover).
                    let emote_bytes = self.emote_bytes;

                    let response = self
                        .show_image(ui, &url_key, raw, size, Some(url.as_str()))
                        .on_hover_ui_at_pointer(|ui| {
                            // Check HD availability at hover time, not every frame.
                            let hd_entry = url_hd.as_deref().and_then(|u| emote_bytes.get(u));

                            let (tt_url, tt_key, tt_raw, tt_w, tt_h) = match hd_entry {
                                Some(&(hw, hh, ref href)) => (
                                    url_hd.as_deref().unwrap(),
                                    super::bytes_uri(url_hd.as_deref().unwrap(), href),
                                    href.clone(),
                                    hw,
                                    hh,
                                ),
                                None => (url.as_str(), url_key.clone(), raw.clone(), w, h),
                            };
                            let tt_size = fit_size(tt_w, tt_h, TOOLTIP_EMOTE_SIZE);

                            ui.set_max_width(280.0);
                            ui.vertical_centered(|ui| {
                                self.show_image(ui, &tt_key, &tt_raw, tt_size, Some(tt_url));
                                ui.add_space(4.0);
                                ui.label(RichText::new(code.as_str()).strong());
                                ui.label(
                                    RichText::new(provider_label(provider))
                                        .small()
                                        .color(t::text_secondary()),
                                );
                            });
                        });
                    clicked = response.clicked();
                    message_context_menu(&response, &mut *context_menu);
                } else {
                    // Image not yet loaded - show text code as placeholder
                    let response = ui.add(
                        Label::new(
                            RichText::new(code)
                                .italics()
                                .font(t::small())
                                .color(t::green()),
                        )
                        .selectable(true)
                        .sense(pointer_click_sense()),
                    );
                    if ui.is_rect_visible(response.rect) {
                        self.emote_bytes.get(url);
                    }
                    clicked = response.clicked();
                    message_context_menu(&response, &mut *context_menu);
                }
            }
            Span::Emoji { text, url } => {
                if let Some(&(w, h, ref raw)) = self.emote_bytes.peek(url.as_str()) {
                    let size = fit_size(w, h, EMOTE_SIZE);
                    let tooltip_size = fit_size(w, h, TOOLTIP_EMOTE_SIZE);
                    let url_key = super::bytes_uri(url, raw);
                    let response = self
                        .show_image(ui, &url_key, raw, size, Some(url.as_str()))
                        .on_hover_ui_at_pointer(|ui| {
                            ui.set_max_width(200.0);
                            ui.vertical_centered(|ui| {
                                self.show_image(
                                    ui,
                                    &url_key,
                                    raw,
                                    tooltip_size,
                                    Some(url.as_str()),
                                );
                                ui.add_space(4.0);
                                ui.label(RichText::new(text.as_str()).strong());
                                ui.label(
                                    RichText::new("Twemoji").small().color(t::text_secondary()),
                                );
                            });
                        });
                    clicked = response.clicked();
                    message_context_menu(&response, &mut *context_menu);
                } else {
                    let response = ui.add(Label::new(RichText::new(text)).selectable(true)
                        .sense(pointer_click_sense()));
                    if ui.is_rect_visible(response.rect) {
                        self.emote_bytes.get(url);
                    }
                    clicked = response.clicked();
                    message_context_menu(&response, &mut *context_menu);
                }
            }
            Span::Mention { login } => {
                let name = format!("@{login}");
                if channel.is_twitch() {
                    let appearance = super::sender_cosmetics::get(ui.ctx(), None, login);
                    let color = appearance
                        .as_ref()
                        .and_then(|appearance| appearance.color)
                        .unwrap_or_else(t::mention);
                    let paint = appearance
                        .as_ref()
                        .and_then(|appearance| appearance.paint.as_deref());
                    let response = show_sender_name(
                        ui,
                        &name,
                        color,
                        paint,
                        self.emote_bytes,
                        self.animate_emotes,
                    );
                    if response.clicked() {
                        let _ = self.cmd_tx.try_send(AppCommand::ShowUserCard {
                            login: login.clone(),
                            channel: channel.clone(),
                        });
                    }
                    response.context_menu(&mut *context_menu);
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                } else {
                    let response = ui.add(
                        Label::new(RichText::new(name).color(t::mention()).strong())
                            .selectable(true)
                            .sense(pointer_click_sense()),
                    );
                    clicked = response.clicked();
                    message_context_menu(&response, &mut *context_menu);
                }
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
                resp.context_menu(&mut *context_menu);
                if self.hide_link_previews {
                    return false;
                }
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
                                    RichText::new("Loading preview...")
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
        clicked
    }

    fn show_image(
        &self,
        ui: &mut Ui,
        uri: &str,
        raw: &Arc<[u8]>,
        size: Vec2,
        url_hint: Option<&str>,
    ) -> egui::Response {
        show_message_image(
            ui,
            uri,
            raw,
            size,
            url_hint,
            self.emote_bytes,
            self.animate_emotes,
        )
    }
}

fn show_message_image(
    ui: &mut Ui,
    uri: &str,
    raw: &Arc<[u8]>,
    size: Vec2,
    url_hint: Option<&str>,
    images: &UiImageCache,
    animate: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, pointer_click_sense());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::Image));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let cache_key = url_hint.unwrap_or(uri);
    images.get(cache_key);
    if !animate && is_animation_format(raw) {
        if let Some(tex) = images.static_texture(ui.ctx(), cache_key) {
            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                t::text_on_accent(),
            );
            return response;
        }
        ui.painter()
            .rect_filled(rect, 2.0, t::alpha(t::text_muted(), 28));
        return response;
    }

    // Image schedules the next real frame from the loader's FrameDurations.
    // A still WebP has no frame deadline and can stay idle.
    egui::Image::from_bytes(uri.to_owned(), egui::load::Bytes::Shared(raw.clone()))
        .fit_to_exact_size(size)
        .paint_at(ui, rect);
    response
}

pub(crate) fn format_message_timestamp(
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
        "...".to_owned()
    } else {
        format!("{trimmed} ...")
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

/// Draw a base emote and its overlays as a single wrapping and hover target.
pub(crate) fn show_emote_layers(
    ui: &mut Ui,
    layers: &[Span],
    images: &UiImageCache,
    target_height: f32,
    animate: bool,
) -> egui::Response {
    paint_emote_layers(ui, layers, images, target_height, animate, false)
        .on_hover_ui_at_pointer(|ui| {
            ui.set_max_width(280.0);
            ui.vertical_centered(|ui| {
                paint_emote_layers(ui, layers, images, TOOLTIP_EMOTE_SIZE, animate, true);
                ui.add_space(4.0);
                for span in layers {
                    if let Span::Emote { code, provider, .. } = span {
                        ui.label(RichText::new(code).strong());
                        ui.label(RichText::new(provider_label(provider)).small().color(t::text_secondary()));
                    }
                }
            });
        })
}

fn layer_image<'a>(
    span: &'a Span,
    images: &'a UiImageCache,
    preview: bool,
) -> Option<(&'a str, &'a crate::image_cache::ImageBytes)> {
    let Span::Emote { url, url_hd, .. } = span else {
        return None;
    };
    if preview {
        if let Some(hd) = url_hd.as_deref() {
            if let Some(image) = images.peek(hd) {
                return Some((hd, image));
            }
        }
    }
    images.peek(url).map(|image| (url.as_str(), image))
}

fn request_emote_layers(layers: &[Span], images: &UiImageCache, preview: bool) {
    for span in layers {
        if let Span::Emote { url, url_hd, .. } = span {
            images.get(url);
            if preview {
                if let Some(url) = url_hd {
                    images.get(url);
                }
            }
        }
    }
}

fn layer_rect(bounds: egui::Rect, size: Vec2) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(bounds.center().x - size.x / 2.0, bounds.bottom() - size.y),
        size,
    )
}

fn paint_emote_layers(
    ui: &mut Ui,
    layers: &[Span],
    images: &UiImageCache,
    target_height: f32,
    animate: bool,
    preview: bool,
) -> egui::Response {
    let mut size = Vec2::ZERO;
    for span in layers {
        if let Some((_, (w, h, _))) = layer_image(span, images, preview) {
            size = size.max(fit_size(*w, *h, target_height));
        }
    }
    // Measuring offscreen rows must not request or decode their images.
    if layers
        .first()
        .and_then(|span| layer_image(span, images, preview))
        .is_none()
    {
        let text: String = layers
            .iter()
            .map(|span| match span {
                Span::Emote { code, .. } => code.as_str(),
                Span::Text { text, .. } => text.as_str(),
                _ => "",
            })
            .collect();
        let response = ui.add(
            Label::new(
                RichText::new(text)
                    .italics()
                    .font(t::small())
                    .color(t::green()),
            )
            .selectable(true),
        );
        if ui.is_rect_visible(response.rect) {
            request_emote_layers(layers, images, preview);
        }
        return response;
    }
    let (bounds, response) = ui.allocate_exact_size(size, pointer_click_sense());
    if !ui.is_rect_visible(bounds) {
        return response;
    }
    request_emote_layers(layers, images, preview);
    for span in layers {
        let Some((url, (w, h, raw))) = layer_image(span, images, preview) else {
            continue;
        };
        let rect = layer_rect(bounds, fit_size(*w, *h, target_height));
        if !animate && is_animation_format(raw) {
            if let Some(texture) = images.static_texture(ui.ctx(), url) {
                ui.painter().image(
                    texture.id(),
                    rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        } else {
            egui::Image::from_bytes(
                super::bytes_uri(url, raw),
                egui::load::Bytes::Shared(raw.clone()),
            )
            .fit_to_exact_size(rect.size())
            .paint_at(ui, rect);
        }
    }
    response
}

/// Scale image dimensions to a target height, preserving aspect ratio.
fn fit_size(w: u32, h: u32, target_h: f32) -> Vec2 {
    if h == 0 {
        return Vec2::new(target_h, target_h);
    }
    let scale = target_h / h as f32;
    Vec2::new(w as f32 * scale, target_h)
}

pub(crate) fn is_animation_format(raw: &[u8]) -> bool {
    raw.starts_with(b"GIF87a") || raw.starts_with(b"GIF89a")
        || (raw.len() >= 12 && raw.starts_with(b"RIFF") && &raw[8..12] == b"WEBP")
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

fn cached_sender_appearance(
    ctx: &egui::Context,
    msg: &ChatMessage,
) -> Option<sender_cosmetics::Appearance> {
    if msg.channel.is_twitch() {
        sender_cosmetics::get(ctx, Some(&msg.sender.user_id.0), &msg.sender.login)
    } else {
        None
    }
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

fn parse_hex_color_opaque_rgb(s: &str) -> Option<Color32> {
    t::parse_hex_rgb_opaque(s)
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

fn moderation_notice_text(msg: &ChatMessage) -> Option<&str> {
    if msg.server_id.as_deref().is_some_and(|id| id.starts_with("eventsub:moderation:")) {
        let text = msg.raw_text.trim();
        (!text.is_empty()).then_some(text)
    } else {
        None
    }
}

fn message_badges(msg: &ChatMessage) -> impl Iterator<Item = (&Badge, bool)> {
    let source_badges = msg.shared.iter().flat_map(|source| source.badges.iter()).filter(move |badge| {
        (badge.name == "moderator" || badge.name == "vip")
            && !msg.sender.badges.iter().any(|own| {
                own.name == badge.name && own.version == badge.version
            })
    });
    msg.sender.badges.iter().map(|badge| (badge, false))
        .chain(source_badges.map(|badge| (badge, true)))
}

/// Show the source channel's avatar or login before a shared message's badges.
fn render_shared_chat_chip(
    ui: &mut Ui,
    shared: &crust_core::model::SharedChatSource,
    emote_bytes: &UiImageCache,
) {
    let who = shared
        .display_name
        .as_deref()
        .or(shared.login.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("another channel");
    let tooltip = format!("Shared Message from {who}");

    // Happy path: we have the source profile picture loaded.
    if let Some(url) = shared.profile_url.as_deref().filter(|s| !s.is_empty()) {
        if let Some(&(w, h, ref raw)) = emote_bytes.get(url) {
            let size = fit_size(w, h, BADGE_SIZE);
            let url_key = super::bytes_uri(url, raw);
            // Rounded corners distinguish the source avatar from role badges.
            let resp = egui::Frame::new()
                .corner_radius(egui::CornerRadius::same(3))
                .inner_margin(egui::Margin::same(0))
                .show(ui, |ui| {
                    ui.add(
                        egui::Image::from_bytes(
                            url_key.clone(),
                            egui::load::Bytes::Shared(raw.clone()),
                        )
                        .fit_to_exact_size(size)
                        .corner_radius(egui::CornerRadius::same(3)),
                    );
                })
                .response;
            resp.on_hover_ui_at_pointer(|ui| {
                let tooltip_size = fit_size(w, h, TOOLTIP_BADGE_SIZE);
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
                    ui.label(RichText::new(&tooltip).strong());
                    if let Some(id) = &shared.source_message_id {
                        ui.label(
                            RichText::new(format!("source-id: {id}"))
                                .color(t::separator())
                                .font(t::tiny()),
                        );
                    }
                });
            });
            return;
        }
    }

    // Fallback: text-only chip with the resolved login (or a generic label
    // while the Helix lookup is still in flight).
    let label_text = shared
        .display_name
        .as_deref()
        .or(shared.login.as_deref())
        .filter(|s| !s.is_empty())
        .map(|s| format!("↗ {s}"))
        .unwrap_or_else(|| "↗ Shared".to_owned());
    let bg = Color32::from_rgb(112, 46, 158);
    let fg = Color32::from_rgb(248, 235, 255);
    let stroke = if t::is_light() {
        bg.gamma_multiply(0.72)
    } else {
        bg.gamma_multiply(1.25)
    };
    let response = egui::Frame::new()
        .fill(bg)
        .stroke(egui::Stroke::new(1.0_f32, stroke))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(3, 1))
        .show(ui, |ui| {
            ui.add(Label::new(
                RichText::new(&label_text)
                    .font(t::tiny())
                    .color(fg)
                    .strong(),
            ));
        })
        .response;
    response.on_hover_ui_at_pointer(|ui| {
        ui.label(RichText::new(&tooltip).strong());
        if let Some(id) = &shared.source_message_id {
            ui.label(
                RichText::new(format!("source-id: {id}"))
                    .color(t::separator())
                    .font(t::tiny()),
            );
        }
    });
}

fn render_low_trust_chip(ui: &mut Ui, status: LowTrustStatus) {
    let (label, tooltip, bg, fg) = match status {
        LowTrustStatus::Restricted => (
            "RESTRICTED",
            "Restricted suspicious user",
            Color32::from_rgb(176, 42, 55),
            Color32::from_rgb(255, 235, 238),
        ),
        LowTrustStatus::Monitored => (
            "MONITORED",
            "Monitored suspicious user",
            Color32::from_rgb(155, 108, 20),
            Color32::from_rgb(255, 243, 214),
        ),
    };
    let stroke = if t::is_light() {
        bg.gamma_multiply(0.72)
    } else {
        bg.gamma_multiply(1.25)
    };
    let response = egui::Frame::new()
        .fill(bg)
        .stroke(egui::Stroke::new(1.0_f32, stroke))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(3, 1))
        .show(ui, |ui| {
            ui.add(Label::new(
                RichText::new(label).font(t::tiny()).color(fg).strong(),
            ));
        })
        .response;
    response.on_hover_ui_at_pointer(|ui| {
        ui.label(RichText::new(tooltip).strong());
    });
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
        .stroke(egui::Stroke::new(1.0_f32, stroke))
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
                RichText::new(&chip_text)
                    .size(t::chips_font_size())
                    .color(fg)
                    .strong(),
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

    fn emote_repaint_delay(raw: Vec<u8>, layered: bool, animate: bool) -> std::time::Duration {
        let ctx = egui::Context::default();
        let mut images = UiImageCache::default();
        images.begin_frame(&ctx);
        let url = "https://example.test/frame.webp";
        assert!(images.complete(&ctx, url.into(), 2, 1, raw));
        let raw = images.get(url).unwrap().2.clone();
        let uri = super::super::bytes_uri(url, &raw);
        ctx.include_bytes(uri.clone(), egui::load::Bytes::Shared(raw.clone()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // Paused rendering has its own first-frame decoder and texture cache.
            // Warming the animated URI does not settle that separate request.
            let ready = if !animate && is_animation_format(&raw) {
                images.static_texture(&ctx, url).is_some()
            } else {
                match ctx.try_load_texture(&format!("{uri}#0"), egui::TextureOptions::LINEAR,
                    egui::load::SizeHint::default()) {
                    Ok(egui::load::TexturePoll::Ready { .. }) => true,
                    Ok(egui::load::TexturePoll::Pending { .. }) => false,
                    Err(error) => panic!("fixture decode failed: {error}"),
                }
            };
            // A decoder publishes its pixels before requesting the completion
            // repaint. Wait for the job to finish before measuring idle frames.
            if ready && images.work_stats(&ctx).pending_decodes == 0 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "fixture decode timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let messages = VecDeque::new();
        let channel = ChannelId::new("room");
        let previews = HashMap::new();
        let ignored = HashSet::new();
        let ignored_users = crust_core::ignores::CompiledIgnoredUsers::new(&[]);
        let low_trust = HashMap::new();
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let sender = UiCommandSender::new(sender);
        let mut list = MessageList::new(&messages, &images, &sender, &channel, &previews, None,
            false, 3, false, false, false, true, false, &ignored, &ignored_users,
            &[], &[], &[], &low_trust);
        list.animate_emotes = animate;
        let layers = crust_core::format::tokenize_with_flags("Base Hat", false, &[], &|code| Some((
            code.into(), code.into(), url.into(), "7tv".into(), None, code == "Hat",
        )));
        let mut delay = std::time::Duration::ZERO;
        // Drain the decoder/texture upload and first-layout wakeups before checking
        // the image's own deadline at a fixed point near the start of a 100ms frame.
        for _ in 0..5 {
            let output = ctx.run(egui::RawInput { time: Some(1.001), ..Default::default() }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if layered {
                        show_emote_layers(ui, &layers, &images, 22.0, animate);
                    } else {
                        list.show_image(ui, &uri, &raw, egui::vec2(44.0, 22.0), Some(url));
                    }
                });
            });
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        delay
    }

    #[test]
    fn still_webp_emotes_can_idle_in_ordinary_and_layered_rendering() {
        let mut raw = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 1).write_to(&mut raw, image::ImageFormat::WebP).unwrap();
        for layered in [false, true] {
            let delay = emote_repaint_delay(raw.get_ref().clone(), layered, true);
            assert_eq!(delay, std::time::Duration::MAX, "still WebP must not poll every 33 ms");
        }
    }

    #[test]
    fn animated_gif_and_webp_emotes_schedule_their_actual_frames_and_can_pause() {
        for raw in [crate::image_cache::animated_gif_fixture(), crate::image_cache::animated_webp_fixture()] {
            for layered in [false, true] {
                let delay = emote_repaint_delay(raw.clone(), layered, true);
                assert!(delay > std::time::Duration::from_millis(40)
                    && delay <= std::time::Duration::from_millis(100),
                    "100ms animation frames should set their own deadline: {delay:?}");
                let paused = emote_repaint_delay(raw.clone(), layered, false);
                assert_eq!(paused, std::time::Duration::MAX, "paused emotes must not schedule frames");
            }
        }
    }

    fn overlay_spans() -> Vec<Span> {
        crust_core::format::tokenize_with_flags("Base Hat Sparkle", false, &[], &|code| Some((
            code.into(), code.into(), format!("https://example.test/{code}.png"),
            "7tv".into(), Some(format!("https://example.test/{code}-hd.png")), code != "Base",
        ))).into_vec()
    }

    #[test]
    fn layered_emotes_wrap_as_one_item_and_use_the_largest_width_once() {
        let ctx = egui::Context::default();
        let mut images = UiImageCache::default();
        let layers = overlay_spans();
        for (code, width) in [("Base", 40), ("Hat", 80), ("Sparkle", 20)] {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::RgbaImage::from_pixel(width, 40, image::Rgba([255, 0, 0, 255]))
                .write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            assert!(images.complete(&ctx, format!("https://example.test/{code}.png"), width, 40, bytes.into_inner()));
        }
        let mut before = egui::Rect::NOTHING;
        let mut group = egui::Rect::NOTHING;
        let mut after = egui::Rect::NOTHING;
        let _ = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(80.0, 200.0))),
            ..Default::default()
        }, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                assert_eq!(ui.available_width(), 80.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    before = ui.allocate_exact_size(egui::vec2(60.0, 22.0), egui::Sense::hover()).0;
                    group = show_emote_layers(ui, &layers, &images, 22.0, true).rect;
                    after = ui.allocate_exact_size(egui::vec2(12.0, 22.0), egui::Sense::hover()).0;
                });
            });
        });
        assert_eq!(group.size(), egui::vec2(44.0, 22.0));
        assert!(group.top() >= before.bottom(), "group {group:?} must wrap below {before:?}");
        assert!((after.left() - group.right()).abs() < 0.1);
        assert!((after.bottom() - group.bottom()).abs() < 0.1);
        let base = layer_rect(group, egui::vec2(22.0, 22.0));
        let sparkle = layer_rect(group, egui::vec2(11.0, 11.0));
        assert_eq!(base.center().x, group.center().x);
        assert_eq!(sparkle.center().x, group.center().x);
        assert_eq!(sparkle.bottom(), group.bottom());
        assert!(group.contains_rect(base) && group.contains_rect(sparkle));
    }

    #[test]
    fn missing_layers_keep_codes_readable_and_request_only_normal_assets() {
        let ctx = egui::Context::default();
        let images = UiImageCache::default();
        let layers = overlay_spans();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_emote_layers(ui, &layers, &images, 22.0, true);
            });
        });
        assert!(output.shapes.iter().any(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => text.galley.text() == "Base Hat Sparkle",
            _ => false,
        }));
        let mut urls = HashSet::new();
        images.end_frame(&ctx, |command| {
            if let AppCommand::FetchImage { url } = command { urls.insert(url); }
            true
        });
        assert_eq!(urls, HashSet::from([
            "https://example.test/Base.png".to_owned(),
            "https://example.test/Hat.png".to_owned(),
            "https://example.test/Sparkle.png".to_owned(),
        ]));
    }

    fn test_msg(login: &str, user_id: &str, color: Option<&str>) -> ChatMessage {
        ChatMessage {
            id: MessageId(1),
            server_id: None,
            timestamp: chrono::Utc::now(),
            channel: ChannelId::new("rustlang"),
            sender: Sender {
                original_display_name: None,
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
            shared: None,
        }
    }

    fn render_test_messages(
        ctx: &egui::Context,
        messages: &VecDeque<ChatMessage>,
        filters: &[crust_core::model::filters::CompiledFilter],
    ) -> MessageListPerfStats {
        render_test_messages_with(ctx, messages, filters, 500.0, |_| {})
    }

    fn render_test_messages_with(
        ctx: &egui::Context,
        messages: &VecDeque<ChatMessage>,
        filters: &[crust_core::model::filters::CompiledFilter],
        width: f32,
        configure: impl FnOnce(&mut MessageList<'_>),
    ) -> MessageListPerfStats {
        render_test_message_frame(
            ctx,
            messages,
            filters,
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 400.0),
                )),
                ..Default::default()
            },
            configure,
            |_| {},
        )
    }

    fn render_test_message_frame(
        ctx: &egui::Context,
        messages: &VecDeque<ChatMessage>,
        filters: &[crust_core::model::filters::CompiledFilter],
        input: egui::RawInput,
        configure: impl FnOnce(&mut MessageList<'_>),
        mut after_chat: impl FnMut(&egui::Context),
    ) -> MessageListPerfStats {
        let channel = ChannelId::new("rustlang");
        let images = UiImageCache::default();
        let previews = HashMap::new();
        let ignored_logins = HashSet::new();
        let ignored_users = crust_core::ignores::CompiledIgnoredUsers::new(&[]);
        let low_trust = HashMap::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let tx = UiCommandSender::new(tx);
        let mut list = MessageList::new(
            messages,
            &images,
            &tx,
            &channel,
            &previews,
            None,
            false,
            3,
            false,
            false,
            false,
            true,
            false,
            &ignored_logins,
            &ignored_users,
            &[],
            filters,
            &[],
            &low_trust,
        );
        configure(&mut list);
        let mut stats = MessageListPerfStats::default();
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                stats = list.show(ui).perf_stats;
            });
            after_chat(ctx);
        });
        stats
    }

    fn wrapping_message_fixture(
        suspicious: bool,
        painted: bool,
        count: usize,
    ) -> VecDeque<ChatMessage> {
        let prefix = "wrap-start alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu ";
        (1..=count)
            .map(|id| {
                let mut message = test_msg("long_sender_display_name", "42", Some("#00ff00"));
                message.id = MessageId(id as u64);
                message.server_id = Some(format!("wrapped-message-{id}"));
                if suspicious {
                    message.msg_kind = MsgKind::SuspiciousUserMessage;
                }
                if painted {
                    message.sender.name_paint = Some(inline_mention_paint());
                }
                message.sender.badges = ["moderator", "subscriber", "vip"]
                    .into_iter()
                    .map(|name| Badge {
                        name: name.to_owned(),
                        version: "1".to_owned(),
                        url: None,
                    })
                    .collect();
                message.raw_text = format!("{prefix}https://example.test/");
                message.spans = prefix
                    .split_inclusive(' ')
                    .map(|text| Span::Text {
                        text: text.to_owned(),
                        is_action: false,
                    })
                    .chain(std::iter::once(Span::Url {
                        text: "https://example.test/".to_owned(),
                        url: "https://example.test/".to_owned(),
                    }))
                    .collect();
                message
            })
            .collect()
    }

    fn wrapping_message_frame(
        ctx: &egui::Context,
        messages: &VecDeque<ChatMessage>,
        sender: &UiCommandSender,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<egui::Response>, f32) {
        let channel = ChannelId::new("rustlang");
        let images = UiImageCache::default();
        let previews = HashMap::new();
        let ignored = HashSet::new();
        let ignored_users = crust_core::ignores::CompiledIgnoredUsers::new(&[]);
        let low_trust = HashMap::new();
        let mut list = MessageList::new(
            messages,
            &images,
            sender,
            &channel,
            &previews,
            None,
            false,
            3,
            false,
            true,
            false,
            true,
            false,
            &ignored,
            &ignored_users,
            &[],
            &[],
            &[],
            &low_trust,
        );
        let target = messages.back().unwrap().id;
        list.scroll_to = Some(target);
        list.hide_link_previews = true;
        let row_id = list.state_id("msg_bg_click").with(target.0);
        let mut response = None;
        let mut row_spacing = 0.0;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(370.0, 600.0),
                )),
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let spacing = ui.spacing().item_spacing;
                    row_spacing = spacing.y;
                    list.show(ui);
                    assert_eq!(
                        ui.spacing().item_spacing,
                        spacing,
                        "message spacing must not leak"
                    );
                    response = ctx.read_response(row_id);
                });
            },
        );
        (output, response, row_spacing)
    }

    #[test]
    fn subscription_rows_wrap_and_copy_new_and_legacy_subscriber_text_once() {
        let body = "subscriber-start alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu subscriber-end";
        let notice = "Alice resubscribed with Tier 1! (12 months)";
        for legacy in [false, true] {
            let ctx = egui::Context::default();
            let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
            let sender = UiCommandSender::new(sender);
            let mut message = test_msg("alice", "10", None);
            message.server_id = Some("subscription-1".into());
            message.raw_text = if legacy { notice } else { body }.into();
            message.spans.push(Span::Text { text: message.raw_text.clone(), is_action: false });
            message.msg_kind = MsgKind::Sub {
                display_name: "Alice".into(), months: 12, plan: "Tier 1".into(),
                is_gift: false, sub_msg: body.into(),
            };
            let messages = VecDeque::from([message]);
            for _ in 0..3 {
                wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
            }
            let (output, response, _) = wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
            let body_shapes: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == body => Some(text),
                _ => None,
            }).collect();
            assert_eq!(body_shapes.len(), 1, "subscriber body must appear once (legacy={legacy})");
            let text = body_shapes[0];
            assert!(text.galley.rows.len() > 1, "long subscriber text must wrap");
            let row = response.expect("subscription row is interactive").rect;
            for line in &text.galley.rows {
                let rect = line.visuals.mesh_bounds.translate(text.pos.to_vec2());
                assert!(row.expand(1.0).contains_rect(rect), "subscriber text escaped its row: {rect:?} vs {row:?}");
            }
            let headers: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().contains("Alice resubscribed") => Some(text),
                _ => None,
            }).collect();
            assert_eq!(headers.len(), 1, "subscription notice must appear once");
            assert!(headers[0].pos.y + headers[0].galley.size().y <= text.pos.y + 1.0);

            let body_point = text.pos + egui::vec2(12.0, 8.0);
            for pressed in [true, false] {
                wrapping_message_frame(&ctx, &messages, &sender, vec![
                    egui::Event::PointerMoved(body_point),
                    egui::Event::PointerButton { pos: body_point, button: egui::PointerButton::Secondary,
                        pressed, modifiers: egui::Modifiers::NONE },
                ]);
            }
            assert!(ctx.is_context_menu_open());
            let output = wrapping_message_frame(&ctx, &messages, &sender, Vec::new()).0;
            let copy = body_text_rect(&output, "Copy message text").unwrap().center();
            let pointer = |pressed| vec![
                egui::Event::PointerMoved(copy),
                egui::Event::PointerButton { pos: copy, button: egui::PointerButton::Primary,
                    pressed, modifiers: egui::Modifiers::NONE },
            ];
            wrapping_message_frame(&ctx, &messages, &sender, pointer(true));
            let output = wrapping_message_frame(&ctx, &messages, &sender, pointer(false)).0;
            let expected = format!("{notice}\n{body}");
            assert!(output.platform_output.commands.iter().any(|command| {
                matches!(command, egui::OutputCommand::CopyText(text) if text == &expected)
            }));
            assert!(receiver.try_recv().is_err());
        }
    }

    #[test]
    fn subscription_body_links_keep_click_priority_over_the_card() {
        let ctx = egui::Context::default();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let sender = UiCommandSender::new(sender);
        let mut message = test_msg("alice", "10", None);
        message.raw_text = "thanks https://example.test/".into();
        message.spans = crust_core::format::tokenize_with_flags(&message.raw_text, false, &[], &|_| None);
        message.msg_kind = MsgKind::Sub {
            display_name: "Alice".into(), months: 1, plan: "Tier 1".into(),
            is_gift: false, sub_msg: message.raw_text.clone(),
        };
        let messages = VecDeque::from([message]);
        for _ in 0..3 { wrapping_message_frame(&ctx, &messages, &sender, Vec::new()); }
        let output = wrapping_message_frame(&ctx, &messages, &sender, Vec::new()).0;
        let link = body_text_rect(&output, "https://example.test/").unwrap().center();
        for pressed in [true, false] {
            wrapping_message_frame(&ctx, &messages, &sender, vec![
                egui::Event::PointerMoved(link),
                egui::Event::PointerButton { pos: link, button: egui::PointerButton::Primary,
                    pressed, modifiers: egui::Modifiers::NONE },
            ]);
        }
        assert!(matches!(receiver.try_recv().unwrap(), AppCommand::OpenUrl { url }
            if url == "https://example.test/"));
        assert!(receiver.try_recv().is_err());
    }

    fn wrapping_body_rects(
        output: &egui::FullOutput,
        wanted: &HashSet<&str>,
        bounds: egui::Rect,
    ) -> Vec<egui::Rect> {
        output
            .shapes
            .iter()
            .filter_map(|shape| {
                let egui::epaint::Shape::Text(text) = &shape.shape else {
                    return None;
                };
                wanted
                    .contains(text.galley.text())
                    .then_some((text, shape.clip_rect))
            })
            .flat_map(|(text, clip)| {
                text.galley.rows.iter().filter_map(move |row| {
                    if !row.glyphs.iter().any(|glyph| !glyph.chr.is_whitespace()) {
                        return None;
                    }
                    let rect = row.visuals.mesh_bounds.translate(text.pos.to_vec2());
                    (bounds.contains_rect(rect) && clip.contains_rect(rect)).then_some(rect)
                })
            })
            .collect()
    }

    #[test]
    fn long_sender_prefixes_leave_full_width_for_body_continuations_and_cached_heights() {
        for suspicious in [false, true] {
            for painted in [false, true] {
                for count in [1, 48] {
                    let ctx = egui::Context::default();
                    let messages = wrapping_message_fixture(suspicious, painted, count);
                    let (sender, _receiver) = tokio::sync::mpsc::channel(8);
                    let sender = UiCommandSender::new(sender);
                    let mut rendered = wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
                    for _ in 0..5 {
                        rendered = wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
                    }
                    let (output, response, row_spacing) = rendered;
                    let row = response
                        .expect("the requested last message is visible")
                        .rect;
                    let message = messages.back().unwrap();
                    let body_text: HashSet<_> = message
                        .spans
                        .iter()
                        .filter_map(|span| match span {
                            Span::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let body = wrapping_body_rects(&output, &body_text, row);
                    assert!(!body.is_empty());
                    let first_top = body
                        .iter()
                        .map(egui::Rect::top)
                        .fold(f32::INFINITY, f32::min);
                    let content_left = row.left() + ROW_PAD_X;
                    assert!(body.iter().any(|rect| rect.top() > first_top + 5.0 && rect.left() <= content_left + 2.0),
                        "continuation lines must return to {content_left}, suspicious={suspicious}, painted={painted}, count={count}, body={body:?}");
                    assert!(body
                        .iter()
                        .all(|rect| rect.right() <= row.right() - ROW_PAD_X + 1.0));
                    let cached = ctx.data(|data| {
                        data.get_temp::<HashMap<u64, f32>>(view_state_id(
                            &message.channel,
                            None,
                            "msg_row_h",
                        ))
                        .unwrap()[&message.id.0]
                    });
                    assert!((cached - row.height() - row_spacing).abs() <= 0.5,
                        "cached stride {cached} must match the drawn row {row:?} plus spacing {row_spacing}");
                }
            }
        }
    }

    #[test]
    fn wrapped_message_bodies_keep_links_and_message_context_menus() {
        for suspicious in [false, true] {
            let ctx = egui::Context::default();
            let messages = wrapping_message_fixture(suspicious, true, 1);
            let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
            let sender = UiCommandSender::new(sender);
            let mut rendered = wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
            for _ in 0..5 {
                rendered = wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
            }
            let link = wrapping_body_rects(
                &rendered.0,
                &HashSet::from(["https://example.test/"]),
                rendered.1.as_ref().unwrap().rect,
            )[0]
            .center();
            for pressed in [true, false] {
                let _ = wrapping_message_frame(
                    &ctx,
                    &messages,
                    &sender,
                    vec![
                        egui::Event::PointerMoved(link),
                        egui::Event::PointerButton {
                            pos: link,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(
                matches!(receiver.try_recv().unwrap(), AppCommand::OpenUrl { url } if url == "https://example.test/")
            );
            assert!(receiver.try_recv().is_err());
            for pressed in [true, false] {
                let _ = wrapping_message_frame(
                    &ctx,
                    &messages,
                    &sender,
                    vec![
                        egui::Event::PointerMoved(link),
                        egui::Event::PointerButton {
                            pos: link,
                            button: egui::PointerButton::Secondary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(ctx.is_context_menu_open());
            rendered = wrapping_message_frame(&ctx, &messages, &sender, Vec::new());
            let copy = body_text_rect(&rendered.0, "Copy message text")
                .expect("the message menu is visible")
                .center();
            for pressed in [true, false] {
                rendered = wrapping_message_frame(
                    &ctx,
                    &messages,
                    &sender,
                    vec![
                        egui::Event::PointerMoved(copy),
                        egui::Event::PointerButton {
                            pos: copy,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(rendered.0.platform_output.commands.iter().any(|command| {
                matches!(command, egui::OutputCommand::CopyText(text) if text == &messages[0].raw_text)
            }));
            assert!(receiver.try_recv().is_err());
        }
    }

    fn test_buffer(first_id: u64, count: usize) -> VecDeque<ChatMessage> {
        (first_id..first_id + count as u64)
            .map(|id| {
                let mut message = test_msg("sender", "1", None);
                message.id = MessageId(id);
                message
            })
            .collect()
    }

    fn message_body_frame(
        ctx: &egui::Context,
        body: &MessageBody<'_>,
        message: &ChatMessage,
        events: Vec<egui::Event>,
    ) -> (bool, egui::FullOutput) {
        let mut clicked = false;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 200.0),
                )),
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        clicked = body.show(ui, message, &mut |ui| {
                            if ui.button("Copy message").clicked() {
                                ui.ctx().copy_text(message.raw_text.clone());
                                ui.close_menu();
                            }
                        });
                    });
                });
            },
        );
        (clicked, output)
    }

    fn body_text_rect(output: &egui::FullOutput, wanted: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == wanted => {
                Some(text.galley.rect.translate(text.pos.to_vec2()))
            }
            _ => None,
        })
    }

    #[test]
    fn message_context_menu_opens_the_user_card_with_sender_badges() {
        let ctx = egui::Context::default();
        let mut state = PreviewTestState::default();
        let mut message = test_msg("alice", "42", None);
        message.channel = ChannelId::new("sourcechannel");
        message.sender.badges.push(Badge { name: "moderator".into(), version: "1".into(), url: None });
        message.spans = smallvec::smallvec![Span::Text { text: message.raw_text.clone(), is_action: false }];
        let frame = |events: Vec<egui::Event>| {
            let mut actions = MessagePreviewActions::default();
            let output = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0))),
                events,
                ..Default::default()
            }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    actions.append(state.context().show(ui, &message, Id::new("profile_menu_row"), false, false).actions);
                });
            });
            (output, actions)
        };
        let mut output = egui::FullOutput::default();
        for _ in 0..3 { output = frame(Vec::new()).0; }
        let body = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "hello" => Some(text.galley.rows.iter()
                .flat_map(|row| &row.glyphs)
                .fold(egui::Rect::NOTHING, |bounds, glyph| bounds.union(glyph.logical_rect()))
                .translate(text.pos.to_vec2()).center()),
            _ => None,
        }).expect("the message body must be painted");
        for pressed in [true, false] {
            let (_, actions) = frame(vec![
                egui::Event::PointerMoved(body),
                egui::Event::PointerButton {
                    pos: body, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE,
                },
            ]);
            assert!(actions.is_empty());
        }
        assert!(ctx.is_context_menu_open());
        output = frame(Vec::new()).0;
        let open = body_text_rect(&output, "Open user card").expect("the actual message menu must be visible").center();
        let mut actions = MessagePreviewActions::default();
        for pressed in [true, false] {
            actions.append(frame(vec![
                egui::Event::PointerMoved(open),
                egui::Event::PointerButton {
                    pos: open, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
                },
            ]).1);
        }
        assert!(frame(Vec::new()).1.is_empty(), "the profile request must be consumed once");
        let (channel, login, badges) = actions.profile_request.expect("the card must open before its fetch completes");
        assert_eq!(channel, message.channel);
        assert_eq!(login, message.sender.login);
        assert_eq!(badges.len(), 1);
        assert_eq!(badges[0].name, "moderator");
        assert_eq!(badges[0].version, "1");
        assert_eq!(badges[0].url, None);
        assert!(actions.jump_to.is_none() && actions.reply.is_none() && actions.open_thread.is_none());
        assert!(matches!(state.receiver.try_recv().unwrap(), AppCommand::ShowUserCard { login, channel }
            if login == message.sender.login && channel == message.channel));
        assert!(state.receiver.try_recv().is_err());
    }

    fn inline_mention_paint() -> crust_core::model::SenderNamePaint {
        crust_core::model::SenderNamePaint {
            function: "linear-gradient".to_owned(),
            stops: vec![crust_core::model::SenderNamePaintStop {
                at: 0.0,
                color: "#ff0080".to_owned(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn inline_mentions_use_current_paints_and_open_the_source_channel_user_card() {
        let ctx = egui::Context::default();
        let images = UiImageCache::default();
        let previews = HashMap::new();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let sender = UiCommandSender::new(sender);
        let body = MessageBody {
            emote_bytes: &images,
            cmd_tx: &sender,
            link_previews: &previews,
            animate_emotes: false,
            hide_link_previews: true,
        };
        let mut message = test_msg("author", "1", None);
        message.channel = ChannelId::new("sourcechannel");
        message.raw_text = "Hello @painted welcome".to_owned();
        message.spans = vec![
            Span::Text {
                text: "Hello ".to_owned(),
                is_action: false,
            },
            Span::Mention {
                login: "painted".to_owned(),
            },
            Span::Text {
                text: " welcome".to_owned(),
                is_action: false,
            },
        ]
        .into();
        let (_, output) = message_body_frame(&ctx, &body, &message, Vec::new());
        let name = body_text_rect(&output, "@painted").expect("unknown names remain visible");
        let mut mentioned = test_msg("painted", "42", Some("#00ff00")).sender;
        mentioned.name_paint = Some(inline_mention_paint());
        super::super::sender_cosmetics::remember_sender(&ctx, &mentioned);
        let mut output = output;
        for _ in 0..2 {
            (_, output) = message_body_frame(&ctx, &body, &message, Vec::new());
        }
        assert!(crate::widgets::name_paint::cached_name_count(&ctx) > 0);
        assert!(body_text_rect(&output, "@painted").is_none());
        assert!(body_text_rect(&output, "Hello ").is_some());
        assert!(body_text_rect(&output, " welcome").is_some());
        for pressed in [true, false] {
            let (row_clicked, _) = message_body_frame(
                &ctx,
                &body,
                &message,
                vec![
                    egui::Event::PointerMoved(name.center()),
                    egui::Event::PointerButton {
                        pos: name.center(),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            assert!(!row_clicked, "a profile click must not also jump the row");
        }
        assert!(
            matches!(receiver.try_recv().unwrap(), AppCommand::ShowUserCard { login, channel }
            if login == "painted" && channel == message.channel)
        );
        assert!(receiver.try_recv().is_err());
        for pressed in [true, false] {
            let (row_clicked, _) = message_body_frame(
                &ctx,
                &body,
                &message,
                vec![
                    egui::Event::PointerMoved(name.center()),
                    egui::Event::PointerButton {
                        pos: name.center(),
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            assert!(!row_clicked);
        }
        assert!(ctx.is_context_menu_open());
        let (_, output) = message_body_frame(&ctx, &body, &message, Vec::new());
        assert!(body_text_rect(&output, "Copy message").is_some());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn inline_mentions_keep_unknown_fallback_and_do_not_cross_platforms() {
        for (channel, login) in [
            (ChannelId::new("sourcechannel"), "unknown"),
            (ChannelId::kick("sourcechannel"), "painted"),
            (
                ChannelId::irc("irc.example.test", 6697, true, "sourcechannel"),
                "painted",
            ),
        ] {
            let ctx = egui::Context::default();
            let images = UiImageCache::default();
            let previews = HashMap::new();
            let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
            let sender = UiCommandSender::new(sender);
            let body = MessageBody {
                emote_bytes: &images,
                cmd_tx: &sender,
                link_previews: &previews,
                animate_emotes: false,
                hide_link_previews: true,
            };
            let mut known = test_msg("painted", "42", None).sender;
            known.name_paint = Some(inline_mention_paint());
            super::super::sender_cosmetics::remember_sender(&ctx, &known);
            let mut message = test_msg("author", "1", None);
            message.channel = channel;
            message.spans = vec![Span::Mention {
                login: login.to_owned(),
            }]
            .into();
            let (_, output) = message_body_frame(&ctx, &body, &message, Vec::new());
            let name =
                body_text_rect(&output, &format!("@{login}")).expect("plain fallback is visible");
            assert_eq!(crate::widgets::name_paint::cached_name_count(&ctx), 0);
            for pressed in [true, false] {
                message_body_frame(
                    &ctx,
                    &body,
                    &message,
                    vec![
                        egui::Event::PointerMoved(name.center()),
                        egui::Event::PointerButton {
                            pos: name.center(),
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            if message.channel.is_twitch() {
                assert!(
                    matches!(receiver.try_recv().unwrap(), AppCommand::ShowUserCard { login: target, channel }
                    if target == login && channel == message.channel)
                );
            }
            assert!(
                receiver.try_recv().is_err(),
                "other platforms must not open Twitch profiles"
            );
        }
    }

    #[test]
    fn ordinary_chat_messages_and_actions_use_the_selected_name_paint() {
        use crust_core::model::{SenderNamePaint, SenderNamePaintStop};
        for action in [false, true] {
            let ctx = egui::Context::default();
            let mut message = test_msg("painted", "1", Some("#ffffff"));
            message.flags.is_action = action;
            message.sender.name_paint = Some(SenderNamePaint {
                function: "linear-gradient".to_owned(),
                angle: Some(90.0),
                stops: vec![
                    SenderNamePaintStop { at: 0.0, color: "#ff0000".to_owned() },
                    SenderNamePaintStop { at: 1.0, color: "#0000ff".to_owned() },
                ],
                ..Default::default()
            });
            let messages = VecDeque::from([message]);
            for _ in 0..3 {
                render_test_messages(&ctx, &messages, &[]);
            }
            assert!(crate::widgets::name_paint::cached_name_count(&ctx) > 0,
                "the normal chat path must pass paint metadata to the renderer");
        }
    }

    #[test]
    fn reply_previews_use_the_parent_paint_and_keep_their_jump_target() {
        use crust_core::model::{SenderNamePaint, SenderNamePaintStop};
        let ctx = egui::Context::default();
        let mut parent = test_msg("parent", "1", Some("#ffffff"));
        parent.server_id = Some("parent-message".to_owned());
        parent.sender.name_paint = Some(SenderNamePaint {
            function: "linear-gradient".to_owned(),
            stops: vec![SenderNamePaintStop { at: 0.0, color: "#ff0080".to_owned() }],
            ..Default::default()
        });
        let mut child = test_msg("child", "2", None);
        child.id = MessageId(2);
        child.reply = MessageList::reply_info_for_message(&parent);
        let messages = VecDeque::from([parent, child]);
        for _ in 0..3 { render_test_messages(&ctx, &messages, &[]); }
        assert_eq!(crate::widgets::name_paint::cached_name_count(&ctx), 2,
            "the parent and its smaller reply preview need painted names");
        assert_eq!(messages[1].reply.as_ref().unwrap().parent_msg_id, "parent-message");
    }

    #[test]
    fn chat_and_suspicious_names_use_live_paints_and_keep_explicit_removals() {
        let paint = crust_core::model::SenderNamePaint {
            function: "linear-gradient".to_owned(),
            stops: vec![crust_core::model::SenderNamePaintStop {
                at: 0.0,
                color: "#ff0080".to_owned(),
            }],
            ..Default::default()
        };
        for suspicious in [false, true] {
            for removed in [false, true] {
                let ctx = egui::Context::default();
                let mut message = test_msg("sender", "1", Some("#ffffff"));
                if suspicious {
                    message.msg_kind = MsgKind::SuspiciousUserMessage;
                }
                // Old rows can retain a removed paint; new rows can be missing
                // a paint whose live update reached the UI before its sender.
                message.sender.name_paint = removed.then(|| paint.clone());
                sender_cosmetics::update(
                    &ctx,
                    "1",
                    Some("#00ff80"),
                    (!removed).then_some(&paint),
                );
                let messages = VecDeque::from([message]);
                for _ in 0..3 {
                    render_test_messages(&ctx, &messages, &[]);
                }
                assert_eq!(
                    crate::widgets::name_paint::cached_name_count(&ctx) > 0,
                    !removed,
                    "suspicious={suspicious}, removed={removed}"
                );
                let appearance = sender_cosmetics::get(&ctx, None, "sender").unwrap();
                assert_eq!(appearance.paint.is_some(), !removed);
                assert_eq!(appearance.color, Some(Color32::from_rgb(0, 255, 128)));
            }
        }
    }

    #[test]
    fn non_twitch_senders_do_not_use_or_seed_twitch_appearances() {
        let ctx = egui::Context::default();
        let paint = crust_core::model::SenderNamePaint {
            function: "linear-gradient".to_owned(),
            stops: vec![crust_core::model::SenderNamePaintStop {
                at: 0.0,
                color: "#ff0080".to_owned(),
            }],
            ..Default::default()
        };
        sender_cosmetics::update(&ctx, "1", None, Some(&paint));
        let mut messages = VecDeque::from([
            reply_sender_fixture(1, "alice", Some("one")),
            reply_sender_fixture(2, "bob", Some("two")),
        ]);
        for message in &mut messages {
            message.channel = ChannelId::kick("room");
        }
        for _ in 0..3 {
            render_test_messages(&ctx, &messages, &[]);
        }
        assert_eq!(crate::widgets::name_paint::cached_name_count(&ctx), 0);
        assert!(sender_cosmetics::get(&ctx, Some("2"), "bob").is_none());
    }

    #[test]
    fn reply_previews_use_cached_paints_without_a_retained_parent() {
        let ctx = egui::Context::default();
        let mut messages = narrow_reply_fixture(true);
        let parent = messages[0].sender.clone();
        sender_cosmetics::remember_identity(&ctx, &parent.user_id.0, &parent.login);
        sender_cosmetics::update(&ctx, &parent.user_id.0, None, parent.name_paint.as_ref());
        messages[0] = reply_sender_fixture(3, "unrelated", Some("other-message"));
        let _ = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
        assert_eq!(crate::widgets::name_paint::cached_name_count(&ctx), 1);
    }

    #[test]
    fn reply_previews_keep_live_paint_clears_over_retained_metadata() {
        let ctx = egui::Context::default();
        let messages = narrow_reply_fixture(true);
        sender_cosmetics::update(&ctx, &messages[0].sender.user_id.0, None, None);
        let _ = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
        assert_eq!(crate::widgets::name_paint::cached_name_count(&ctx), 0);
        assert!(sender_cosmetics::get(&ctx, None, "parent")
            .unwrap()
            .paint
            .is_none());
    }

    fn reply_sender_fixture(id: u64, login: &str, server_id: Option<&str>) -> ChatMessage {
        let mut message = test_msg(login, &id.to_string(), None);
        message.id = MessageId(id);
        message.server_id = server_id.map(str::to_owned);
        message
    }

    fn test_reply(login: &str, parent_id: &str) -> ReplyInfo {
        ReplyInfo {
            parent_msg_id: parent_id.to_owned(),
            parent_user_login: login.to_owned(),
            parent_display_name: login.to_owned(),
            parent_msg_body: "A quoted message that is long enough to truncate in a narrow pane."
                .to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn reply_sender_cache_reuses_misses_and_reads_current_styles() {
        let ctx = egui::Context::default();
        let channel = ChannelId::new("rustlang");
        let mut messages = VecDeque::from([
            reply_sender_fixture(1, "Alice", Some("old-alice")),
            reply_sender_fixture(2, "alice", Some("new-alice")),
            reply_sender_fixture(3, "bob", Some("bob-message")),
        ]);
        let index = cached_reply_senders(&ctx, &channel, &messages);
        assert_eq!(
            index
                .message(&messages, &test_reply(" ALICE ", "bob-message"))
                .unwrap()
                .id,
            MessageId(2)
        );
        assert_eq!(
            index
                .message(&messages, &test_reply("", " OLD-ALICE "))
                .unwrap()
                .id,
            MessageId(1)
        );
        for missing in 0..100 {
            let current = cached_reply_senders(&ctx, &channel, &messages);
            assert!(
                Arc::ptr_eq(&index, &current),
                "missing parents must reuse the complete index"
            );
            assert!(
                current
                    .message(
                        &messages,
                        &test_reply(&format!("missing-{missing}"), "old-alice")
                    )
                    .is_none(),
                "a nonempty login must never borrow another parent's appearance"
            );
            assert!(current
                .message(&messages, &test_reply("", &format!("missing-{missing}")))
                .is_none());
        }
        assert_eq!(index.by_login.len(), 2);
        assert_eq!(index.by_server_id.len(), 3);

        messages[1].sender.name_paint = Some(crust_core::model::SenderNamePaint {
            function: "radial-gradient".to_owned(),
            ..Default::default()
        });
        invalidate_message_layouts(&ctx, &channel, &[MessageId(2)]);
        let current = cached_reply_senders(&ctx, &channel, &messages);
        assert!(
            Arc::ptr_eq(&index, &current),
            "cosmetic layout changes do not change sender identities"
        );
        assert_eq!(
            current
                .message(&messages, &test_reply("alice", "old-alice"))
                .unwrap()
                .sender
                .name_paint
                .as_ref()
                .unwrap()
                .function,
            "radial-gradient"
        );
    }

    #[test]
    fn reply_sender_cache_updates_appends_and_rotations_without_rebuilding() {
        let ctx = egui::Context::default();
        let channel = ChannelId::new("rustlang");
        let mut messages = VecDeque::from([
            reply_sender_fixture(1, "alice", Some("first")),
            reply_sender_fixture(2, "bob", Some("second")),
            reply_sender_fixture(3, "alice", Some("third")),
        ]);
        let index = cached_reply_senders(&ctx, &channel, &messages);
        let allocation = Arc::as_ptr(&index);
        drop(index);
        messages.pop_front();
        messages.push_back(reply_sender_fixture(4, "carol", Some("fourth")));
        let index = cached_reply_senders(&ctx, &channel, &messages);
        assert_eq!(Arc::as_ptr(&index), allocation);
        assert_eq!(index.base, 1);
        assert!(index.message(&messages, &test_reply("", "first")).is_none());
        assert_eq!(
            index
                .message(&messages, &test_reply("alice", "first"))
                .unwrap()
                .id,
            MessageId(3)
        );
        drop(index);

        messages.pop_front();
        messages.push_back(reply_sender_fixture(5, "alice", Some("fifth")));
        let index = cached_reply_senders(&ctx, &channel, &messages);
        assert_eq!(Arc::as_ptr(&index), allocation);
        assert_eq!(index.base, 2);
        assert_eq!(index.by_login.len(), 2);
        assert_eq!(index.by_server_id.len(), messages.len());
        assert!(index
            .message(&messages, &test_reply("bob", "second"))
            .is_none());
        assert_eq!(
            index
                .message(&messages, &test_reply("alice", "third"))
                .unwrap()
                .id,
            MessageId(5)
        );
        drop(index);

        messages.push_back(reply_sender_fixture(6, "dave", Some("sixth")));
        let index = cached_reply_senders(&ctx, &channel, &messages);
        assert_eq!(Arc::as_ptr(&index), allocation);
        assert_eq!(index.base, 2);
        assert_eq!(
            index
                .message(&messages, &test_reply("dave", ""))
                .unwrap()
                .id,
            MessageId(6)
        );
        drop(index);

        messages.push_front(reply_sender_fixture(0, "history", Some("older")));
        let index = cached_reply_senders(&ctx, &channel, &messages);
        assert_eq!(
            index.base, 0,
            "prepending history rebuilds the ordered identity index"
        );
        assert_eq!(
            index
                .message(&messages, &test_reply("", "older"))
                .unwrap()
                .id,
            MessageId(0)
        );
        assert_eq!(
            index
                .message(&messages, &test_reply("alice", ""))
                .unwrap()
                .id,
            MessageId(5)
        );
    }

    #[test]
    fn reply_sender_cache_refreshes_in_place_identity_changes() {
        let ctx = egui::Context::default();
        let channel = ChannelId::new("rustlang");
        let mut messages = VecDeque::from([reply_sender_fixture(1, "pending", None)]);
        let before = cached_reply_senders(&ctx, &channel, &messages);
        assert!(before
            .message(&messages, &test_reply("", "confirmed"))
            .is_none());
        messages[0].server_id = Some("confirmed".to_owned());
        messages[0].sender.login = "confirmed-sender".to_owned();
        invalidate_reply_sender_index(&ctx, &channel);
        let after = cached_reply_senders(&ctx, &channel, &messages);
        assert!(!Arc::ptr_eq(&before, &after));
        assert!(after
            .message(&messages, &test_reply("pending", "confirmed"))
            .is_none());
        assert_eq!(
            after
                .message(&messages, &test_reply("confirmed-sender", ""))
                .unwrap()
                .id,
            MessageId(1)
        );
        assert_eq!(
            after
                .message(&messages, &test_reply("", "CONFIRMED"))
                .unwrap()
                .id,
            MessageId(1)
        );
    }

    #[test]
    fn reply_sender_cache_is_shared_and_bounded_across_channels() {
        let ctx = egui::Context::default();
        let messages = VecDeque::from([reply_sender_fixture(1, "alice", Some("message"))]);
        for channel in 0..5 {
            let channel = ChannelId::new(&format!("channel{channel}"));
            let first_pane = cached_reply_senders(&ctx, &channel, &messages);
            let second_pane = cached_reply_senders(&ctx, &channel, &messages);
            assert!(Arc::ptr_eq(&first_pane, &second_pane));
        }
        let cache = ctx.data(|data| {
            data.get_temp::<ReplySenderCache>(Id::new("reply_sender_cache"))
                .unwrap()
        });
        assert_eq!(cache.entries.len(), 4);
        assert!(!cache
            .entries
            .iter()
            .any(|entry| entry.channel == ChannelId::new("channel0")));
    }

    fn reply_preview_frame(
        ctx: &egui::Context,
        messages: &VecDeque<ChatMessage>,
        width: f32,
        events: Vec<egui::Event>,
    ) -> (egui::Response, egui::FullOutput) {
        let channel = ChannelId::new("rustlang");
        let images = UiImageCache::default();
        let previews = HashMap::new();
        let ignored = HashSet::new();
        let ignored_users = crust_core::ignores::CompiledIgnoredUsers::new(&[]);
        let low_trust = HashMap::new();
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let sender = UiCommandSender::new(sender);
        let list = MessageList::new(
            messages,
            &images,
            &sender,
            &channel,
            &previews,
            None,
            false,
            3,
            false,
            false,
            false,
            true,
            false,
            &ignored,
            &ignored_users,
            &[],
            &[],
            &[],
            &low_trust,
        );
        let rctx = RenderCtx {
            reply_key: list.state_id("ml_reply_req"),
            scroll_to_key: list.state_id("ml_scroll_to"),
            scroll_hint_key: list.state_id("ml_scroll_hint"),
            reply_senders: OnceCell::new(),
            highlight_server_id: None,
            highlight_alpha: 0.0,
            highlight_animating: false,
        };
        let mut response = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 200.0),
                )),
                events,
                focused: true,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let child = &messages[1];
                        response = Some(list.render_reply_preview(
                            ui,
                            child,
                            child.reply.as_ref().unwrap(),
                            &rctx,
                            &mut HashMap::new(),
                        ));
                    });
            },
        );
        (response.unwrap(), output)
    }

    fn narrow_reply_fixture(painted: bool) -> VecDeque<ChatMessage> {
        let mut parent = reply_sender_fixture(1, "parent", Some("parent-message"));
        parent.sender.display_name = "長いユーザー名Rainbow".repeat(6);
        parent.raw_text =
            "A long quoted message that should keep some visible text beside a long name."
                .to_owned();
        if painted {
            parent.sender.name_paint = Some(crust_core::model::SenderNamePaint {
                function: "linear-gradient".to_owned(),
                stops: vec![crust_core::model::SenderNamePaintStop {
                    at: 0.0,
                    color: "#ff0080".to_owned(),
                }],
                ..Default::default()
            });
        }
        let mut child = reply_sender_fixture(2, "child", Some("child-message"));
        child.reply = MessageList::reply_info_for_message(&parent);
        VecDeque::from([parent, child])
    }

    #[test]
    fn narrow_reply_previews_bound_long_names_and_leave_room_for_the_body() {
        for painted in [false, true] {
            let ctx = egui::Context::default();
            let messages = narrow_reply_fixture(painted);
            let (response, output) = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
            let pixel = 1.0 / ctx.pixels_per_point();
            assert!(
                response.rect.right() <= 180.0 + pixel,
                "reply header must fit the pane: {:?}",
                response.rect
            );
            assert!(
                response.rect.height() < t::small().size * 2.0,
                "reply preview must stay on one line"
            );
            let body = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) if text.galley.text().starts_with(": ") => {
                        Some(text)
                    }
                    _ => None,
                })
                .expect("the quoted message should still be drawn");
            assert!(body.galley.elided);
            assert!(
                body.galley.size().x >= 40.0,
                "a long sender must leave readable body space"
            );
            assert!(body.pos.x + body.galley.size().x <= 180.0 + pixel);
        }
    }

    #[test]
    fn reply_preview_clicks_jump_to_the_parent_without_opening_a_profile() {
        let channel = ChannelId::new("rustlang");
        for painted in [false, true] {
            for fraction in [0.02, 0.25, 0.8] {
                let ctx = egui::Context::default();
                let messages = narrow_reply_fixture(painted);
                let _ = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
                let (response, _) = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
                let point = response.rect.lerp_inside(egui::vec2(fraction, 0.5));
                for pressed in [true, false] {
                    let _ = reply_preview_frame(
                        &ctx,
                        &messages,
                        180.0,
                        vec![
                            egui::Event::PointerMoved(point),
                            egui::Event::PointerButton {
                                pos: point,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                }
                assert_eq!(
                    ctx.data(|data| data.get_temp::<String>(view_state_id(
                        &channel,
                        None,
                        "ml_scroll_to"
                    )))
                    .as_deref(),
                    Some("parent-message"),
                    "painted={painted}, click fraction={fraction}"
                );
                let hint = ctx
                    .data(|data| {
                        data.get_temp::<ReplyScrollHint>(view_state_id(
                            &channel,
                            None,
                            "ml_scroll_hint",
                        ))
                    })
                    .unwrap();
                assert_eq!(hint.child_msg_local_id, 2);
                assert_eq!(hint.parent_user_login, "parent");
                assert!(ctx
                    .data(|data| data.get_temp::<(String, Vec<Badge>)>(view_state_id(
                        &channel,
                        None,
                        "ml_profile_req"
                    )))
                    .is_none());
            }
        }
    }

    #[test]
    fn reply_preview_secondary_click_keeps_the_message_menu() {
        let ctx = egui::Context::default();
        let messages = narrow_reply_fixture(true);
        let _ = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
        let (response, _) = reply_preview_frame(&ctx, &messages, 180.0, Vec::new());
        let point = response.rect.lerp_inside(egui::vec2(0.25, 0.5));
        let mut opened = false;
        for pressed in [true, false] {
            let (response, _) = reply_preview_frame(
                &ctx,
                &messages,
                180.0,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            opened |= response.context_menu_opened();
        }
        assert!(
            opened,
            "the painted reply sender must still open the message context menu"
        );
        let channel = ChannelId::new("rustlang");
        assert!(ctx
            .data(|data| data.get_temp::<String>(view_state_id(&channel, None, "ml_scroll_to")))
            .is_none());
    }

    fn render_duplicate_panes(
        ctx: &egui::Context,
        messages: &VecDeque<ChatMessage>,
        events: Vec<egui::Event>,
    ) -> Vec<MessageListPerfStats> {
        let channel = ChannelId::new("rustlang");
        let images = UiImageCache::default();
        let previews = HashMap::new();
        let ignored = HashSet::new();
        let ignored_users = crust_core::ignores::CompiledIgnoredUsers::new(&[]);
        let low_trust = HashMap::new();
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let sender = UiCommandSender::new(sender);
        let mut stats = Vec::new();
        let _ = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 400.0))),
            events,
            ..Default::default()
        }, |ctx| {
            stats.clear();
            egui::CentralPanel::default().show(ctx, |ui| {
                for (pane, left, width) in [(1, 10.0, 300.0), (2, 320.0, 660.0)] {
                    let rect = egui::Rect::from_min_size(egui::pos2(left, 10.0), egui::vec2(width, 380.0));
                    let mut child = ui.new_child(egui::UiBuilder::new()
                        .id_salt(("duplicate_pane", pane))
                        .max_rect(rect)
                        .layout(egui::Layout::top_down(egui::Align::LEFT)));
                    child.set_clip_rect(rect);
                    let list = MessageList::new(
                        messages, &images, &sender, &channel, &previews, None,
                        false, 3, false, false, false, true, false,
                        &ignored, &ignored_users, &[], &[], &[], &low_trust,
                    ).with_pane(pane, pane == 1);
                    stats.push(list.show(&mut child).perf_stats);
                }
            });
        });
        stats
    }

    #[test]
    fn duplicate_channel_panes_keep_independent_scroll_and_layout_caches() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        for _ in 0..8 {
            render_duplicate_panes(&ctx, &messages, Vec::new());
        }
        let stats = render_duplicate_panes(&ctx, &messages, Vec::new());
        assert!(stats.iter().all(|stats| !stats.prefix_rebuilt && stats.height_cache_misses == 0));
        render_duplicate_panes(&ctx, &messages, vec![
            egui::Event::PointerMoved(egui::pos2(150.0, 180.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 120.0),
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let channel = ChannelId::new("rustlang");
        assert_eq!(ctx.data(|data| data.get_temp::<bool>(view_state_id(&channel, Some(1), "scroll_paused"))), Some(true));
        assert_eq!(ctx.data(|data| data.get_temp::<bool>(view_state_id(&channel, Some(2), "scroll_paused"))), Some(false));
    }

    #[test]
    fn message_edits_invalidate_visible_and_hidden_duplicate_panes() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        let channel = ChannelId::new("rustlang");
        for _ in 0..8 {
            render_duplicate_panes(&ctx, &messages, Vec::new());
        }
        invalidate_message_layouts(&ctx, &channel, &[MessageId(100)]);
        for pane in [1, 2] {
            let heights: HashMap<u64, f32> = ctx.data(|data| data.get_temp(view_state_id(&channel, Some(pane), "msg_row_h")).unwrap());
            assert!(!heights.contains_key(&100));
        }
        let stats = render_duplicate_panes(&ctx, &messages, Vec::new());
        assert!(stats.iter().all(|stats| stats.prefix_rebuilt));
        for _ in 0..4 {
            render_test_messages_with(&ctx, &messages, &[], 316.0, |list| list.pane = Some(1));
        }
        invalidate_message_layouts(&ctx, &channel, &[MessageId(100)]);
        let stats = render_test_messages_with(&ctx, &messages, &[], 676.0, |list| list.pane = Some(2));
        assert!(stats.prefix_rebuilt);
    }

    #[test]
    fn wheel_over_an_overlapping_window_keeps_chat_live_but_uncovered_chat_pauses() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        let paused_key = Id::new("scroll_paused").with("rustlang");
        let offset_key = Id::new("scroll_prev_offset").with("rustlang");
        let frame = |events| {
            render_test_message_frame(
                &ctx,
                &messages,
                &[],
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(500.0, 400.0),
                    )),
                    focused: true,
                    events,
                    ..Default::default()
                },
                |_| {},
                |ctx| {
                    egui::Window::new("Overlapping settings")
                        .order(egui::Order::Foreground)
                        .fixed_pos(egui::pos2(100.0, 60.0))
                        .fixed_size(egui::vec2(240.0, 250.0))
                        .collapsible(false)
                        .show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for row in 0..50 {
                                    ui.label(format!("Setting {row}"));
                                }
                            });
                        });
                },
            );
        };
        for _ in 0..6 {
            frame(Vec::new());
        }
        assert_eq!(
            ctx.data(|data| data.get_temp::<bool>(paused_key)),
            Some(false)
        );
        let offset_before = ctx.data(|data| data.get_temp::<f32>(offset_key).unwrap());
        assert!(
            offset_before > 0.0,
            "The chat fixture must have scrollable history"
        );

        let wheel_at = |point| {
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, 80.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        let covered = egui::pos2(180.0, 180.0);
        frame(wheel_at(covered));
        assert_eq!(
            ctx.layer_id_at(covered).unwrap().order,
            egui::Order::Foreground
        );
        assert_eq!(
            ctx.data(|data| data.get_temp::<bool>(paused_key)),
            Some(false)
        );
        assert_eq!(
            ctx.data(|data| data.get_temp::<f32>(offset_key)),
            Some(offset_before)
        );

        let uncovered = egui::pos2(450.0, 180.0);
        frame(wheel_at(uncovered));
        assert_eq!(
            ctx.layer_id_at(uncovered),
            Some(egui::LayerId::background())
        );
        assert_eq!(
            ctx.data(|data| data.get_temp::<bool>(paused_key)),
            Some(true)
        );
    }

    #[test]
    fn row_height_cache_stays_bounded_as_old_messages_leave_the_buffer() {
        let ctx = egui::Context::default();
        let cache_key = Id::new("msg_row_h").with("rustlang");
        for batch in 0..32 {
            let messages = test_buffer(1 + batch * 64, 64);
            render_test_messages(&ctx, &messages, &[]);
            render_test_messages(&ctx, &messages, &[]);
            let heights: HashMap<u64, f32> = ctx.data_mut(|d| d.get_temp(cache_key).unwrap());
            assert!(heights.len() <= messages.len() + ROW_HEIGHT_CACHE_SLACK);
            assert!(messages.iter().any(|message| heights.contains_key(&message.id.0)));
        }
    }

    #[test]
    fn unchanged_virtual_rows_reuse_the_prefix_and_id_storage() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        for _ in 0..6 {
            render_test_messages(&ctx, &messages, &[]);
        }
        let ids_key = Id::new("msg_active_ids").with("rustlang");
        let original = ctx.data_mut(|d| {
            let ids: Vec<u64> = d.remove_temp(ids_key).unwrap();
            let pointer = ids.as_ptr();
            d.insert_temp(ids_key, ids);
            pointer
        });
        assert!(!render_test_messages(&ctx, &messages, &[]).prefix_rebuilt);
        ctx.data_mut(|d| {
            let ids: Vec<u64> = d.remove_temp(ids_key).unwrap();
            assert_eq!(ids.as_ptr(), original);
            d.insert_temp(ids_key, ids);
        });
    }

    #[test]
    fn width_changes_invalidate_the_prefix_in_the_same_frame() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        for _ in 0..6 {
            render_test_messages(&ctx, &messages, &[]);
        }
        assert!(!render_test_messages(&ctx, &messages, &[]).prefix_rebuilt);
        assert!(render_test_messages_with(&ctx, &messages, &[], 499.0, |_| {}).prefix_rebuilt);
    }

    #[test]
    fn font_changes_discard_offscreen_row_measurements() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        let cache_key = Id::new("msg_row_h").with("rustlang");
        for target in [1, 100] {
            for _ in 0..4 {
                render_test_messages_with(&ctx, &messages, &[], 500.0, |list| {
                    list.scroll_to = Some(MessageId(target));
                });
            }
        }
        assert!(ctx.data_mut(|d| d.get_temp::<HashMap<u64, f32>>(cache_key).unwrap().contains_key(&1)));
        ctx.style_mut(|style| {
            style.text_styles.get_mut(&egui::TextStyle::Body).unwrap().size += 8.0;
        });
        let stats = render_test_messages_with(&ctx, &messages, &[], 500.0, |list| {
            list.scroll_to = Some(MessageId(100));
        });
        assert!(stats.prefix_rebuilt);
        assert!(!ctx.data_mut(|d| d.get_temp::<HashMap<u64, f32>>(cache_key).unwrap().contains_key(&1)));
    }

    #[test]
    fn row_display_settings_invalidate_the_prefix() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 100);
        for _ in 0..6 {
            render_test_messages(&ctx, &messages, &[]);
        }
        assert!(render_test_messages_with(&ctx, &messages, &[], 500.0, |list| {
            list.show_timestamps = true;
        }).prefix_rebuilt);
        assert!(render_test_messages_with(&ctx, &messages, &[], 500.0, |list| {
            list.show_timestamps = true;
            list.show_timestamp_seconds = true;
            list.collapse_long_messages = true;
        }).prefix_rebuilt);
    }

    #[test]
    fn interior_filter_changes_rebuild_the_prefix_with_unchanged_endpoints() {
        use crust_core::model::filters::{compile_filters, FilterRecord, FilterScope};

        let ctx = egui::Context::default();
        let mut messages = test_buffer(1, 100);
        messages[9].raw_text = "hide-first".to_owned();
        messages[10].raw_text = "hide-second".to_owned();
        let first = compile_filters(&[FilterRecord::new("first", "hide-first", FilterScope::Global)]);
        let second = compile_filters(&[FilterRecord::new("second", "hide-second", FilterScope::Global)]);
        for _ in 0..6 {
            render_test_messages(&ctx, &messages, &first);
        }
        assert!(!render_test_messages(&ctx, &messages, &first).prefix_rebuilt);
        let stats = render_test_messages(&ctx, &messages, &second);
        assert_eq!(stats.retained_rows, 100);
        assert_eq!(stats.eligible_rows, 99);
        assert!(stats.prefix_rebuilt);
    }

    #[test]
    fn external_message_jump_opens_the_older_history_window() {
        let ctx = egui::Context::default();
        let messages = test_buffer(1, 900);
        let stats = render_test_messages_with(&ctx, &messages, &[], 500.0, |list| {
            list.scroll_to = Some(MessageId(10));
        });
        assert_eq!(stats.active_rows, messages.len());
        assert_eq!(stats.boundary_hidden_rows, 0);
        let heights: HashMap<u64, f32> = ctx.data_mut(|d| {
            d.get_temp(Id::new("msg_row_h").with("rustlang")).unwrap()
        });
        assert!(heights.contains_key(&10));
        let next_frame = render_test_messages(&ctx, &messages, &[]);
        assert_eq!(next_frame.active_rows, messages.len());
        assert_eq!(next_frame.boundary_hidden_rows, 0);
    }

    #[test]
    fn eventsub_moderation_text_keeps_moderator_and_reason() {
        let mut message = test_msg("system", "", None);
        message.msg_kind = MsgKind::Timeout { login: "sender".to_owned(), seconds: 60 };
        message.raw_text = "Moderator timed out sender for 1m: spam".to_owned();
        assert!(moderation_notice_text(&message).is_none());
        message.server_id = Some("eventsub:moderation:event-id".to_owned());
        assert_eq!(moderation_notice_text(&message), Some("Moderator timed out sender for 1m: spam"));
        message.raw_text = "  ".to_owned();
        assert!(moderation_notice_text(&message).is_none());
    }

    #[test]
    fn shared_badges_keep_own_order_and_append_only_distinct_source_roles() {
        let badge = |name: &str| Badge {
            name: name.to_owned(),
            version: "1".to_owned(),
            url: None,
        };
        let mut message = test_msg("sender", "1", None);
        message.sender.badges = vec![badge("subscriber"), badge("moderator")];
        message.shared = Some(crust_core::model::SharedChatSource {
            badges: vec![badge("subscriber"), badge("moderator"), badge("vip")],
            ..Default::default()
        });
        let badges: Vec<_> = message_badges(&message)
            .map(|(badge, source)| (badge.name.as_str(), source))
            .collect();
        assert_eq!(badges, vec![("subscriber", false), ("moderator", false), ("vip", true)]);
    }

    #[test]
    fn hide_filters_apply_without_search_or_ignored_users_and_while_paused() {
        use crust_core::model::filters::{compile_filters, FilterRecord, FilterScope};

        let mut hidden = test_msg("sender", "1", None);
        hidden.raw_text = "hide this message".to_owned();
        let mut kept = test_msg("sender", "1", None);
        kept.id = MessageId(2);
        let messages = VecDeque::from([hidden, kept]);
        let filters = compile_filters(&[FilterRecord::new("hidden", "hide", FilterScope::Global)]);

        for paused in [false, true] {
            let ctx = egui::Context::default();
            assert_eq!(render_test_messages(&ctx, &messages, &[]).retained_rows, 2);
            if paused {
                ctx.data_mut(|d| {
                    d.insert_temp(Id::new("scroll_paused").with("rustlang"), true);
                    d.insert_temp(Id::new("paused_snapshot_ids").with("rustlang"), vec![1_u64, 2]);
                });
            }
            let stats = render_test_messages(&ctx, &messages, &filters);
            assert_eq!(stats.retained_rows, 2);
            assert_eq!(stats.eligible_rows, 1);
        }
    }

    #[test]
    fn paused_filters_can_reveal_messages_again_without_admitting_new_rows() {
        use crust_core::model::filters::{compile_filters, FilterRecord, FilterScope};
        let ctx = egui::Context::default();
        let mut messages = test_buffer(1, 80);
        messages[10].raw_text = "hidden-marker".to_owned();
        render_test_messages(&ctx, &messages, &[]);
        let paused = Id::new("scroll_paused").with("rustlang");
        let snapshot = Id::new("paused_snapshot_ids").with("rustlang");
        ctx.data_mut(|data| {
            data.insert_temp(paused, true);
            data.insert_temp(snapshot, messages.iter().map(|message| message.id.0).collect::<Vec<_>>());
        });
        let filters = compile_filters(&[FilterRecord::new("hidden", "hidden-marker", FilterScope::Global)]);
        assert_eq!(render_test_messages(&ctx, &messages, &filters).eligible_rows, 79);
        messages.push_back(test_buffer(81, 1).pop_front().unwrap());
        ctx.data_mut(|data| data.insert_temp(paused, true));
        let stats = render_test_messages(&ctx, &messages, &[]);
        assert_eq!(stats.retained_rows, 81);
        assert_eq!(stats.eligible_rows, 80);
        assert!(ctx.data_mut(|data| data.get_temp::<Vec<u64>>(snapshot).unwrap()).contains(&11));
    }

    #[test]
    fn paused_index_cache_reuses_storage_and_detects_interior_changes() {
        let mut messages = test_buffer(1, 80);
        let ids: Vec<_> = messages.iter().map(|message| message.id.0).collect();
        let mut cache = PausedIndexCache::default();
        assert!(cache.refresh(&ids, &messages));
        let pointer = cache.indices.as_ptr();
        assert!(!cache.refresh(&ids, &messages));
        assert_eq!(pointer, cache.indices.as_ptr());
        messages.swap(2, 3);
        assert!(cache.refresh(&ids, &messages));
        assert_eq!(&cache.indices[..4], &[0, 1, 3, 2]);
        messages.pop_front();
        assert!(cache.refresh(&ids, &messages));
        assert_eq!(cache.indices.len(), 79);
    }

    #[test]
    #[ignore = "isolated timing comparison"]
    fn perf_paused_message_indices() {
        use std::hint::black_box;
        use std::time::Instant;
        let messages = test_buffer(1, 1500);
        let ids: Vec<_> = messages.iter().map(|message| message.id.0).collect();
        let mut cache = PausedIndexCache::default();
        cache.refresh(&ids, &messages);
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        for round in 0..5 {
            for old in if round % 2 == 0 { [true, false] } else { [false, true] } {
                let start = Instant::now();
                for _ in 0..5000 {
                    if old {
                        let snapshot = black_box(&ids).clone();
                        let live: Vec<_> = messages.iter().map(|message| message.id.0).collect();
                        let mapped = map_snapshot_ids_to_indices(&snapshot, &live);
                        assert_eq!(mapped.len(), 1500);
                        black_box(mapped);
                    } else {
                        assert!(!cache.refresh(black_box(&ids), black_box(&messages)));
                        black_box(&cache.indices);
                    }
                }
                if old { old_times.push(start.elapsed()); } else { new_times.push(start.elapsed()); }
            }
        }
        old_times.sort();
        new_times.sort();
        eprintln!("5000 paused frames, 1500 rows: rebuild {:?}; cached {:?}", old_times[2], new_times[2]);
    }

    #[test]
    #[ignore = "isolated four-channel rendering workload"]
    fn perf_four_channel_backlog() {
        use std::time::Instant;

        let ctx = egui::Context::default();
        let channels: [ChannelId; 4] = std::array::from_fn(|index| ChannelId::new(format!("loadtest{index}")));
        let make_message = |id: u64, channel: &ChannelId| {
            let mut message = test_msg(&format!("viewer{}", id % 1_000), "1", None);
            message.id = MessageId(id);
            message.channel = channel.clone();
            message.raw_text = format!("Message {id}: rendering several channels while reading chat and receiving new messages.");
            message.spans.push(Span::Text { text: message.raw_text.clone(), is_action: false });
            message
        };
        let mut messages: [VecDeque<ChatMessage>; 4] = std::array::from_fn(|index| {
            (1..=5_000).map(|id| make_message(id, &channels[index])).collect()
        });
        let images = UiImageCache::default();
        let previews = HashMap::new();
        let ignored = HashSet::new();
        let ignored_users = crust_core::ignores::CompiledIgnoredUsers::new(&[]);
        let low_trust = HashMap::new();
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let sender = UiCommandSender::new(sender);
        let render = |messages: &[VecDeque<ChatMessage>; 4], events| {
            let mut stats: [MessageListPerfStats; 4] = std::array::from_fn(|_| Default::default());
            let _ = ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
                events,
                ..Default::default()
            }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for index in 0..4 {
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(8.0 + (index % 2) as f32 * 636.0, 8.0 + (index / 2) as f32 * 396.0),
                            egui::vec2(628.0, 388.0),
                        );
                        let mut child = ui.new_child(egui::UiBuilder::new()
                            .id_salt(("load_pane", index))
                            .max_rect(rect)
                            .layout(egui::Layout::top_down(egui::Align::LEFT)));
                        child.set_clip_rect(rect);
                        let list = MessageList::new(
                            &messages[index], &images, &sender, &channels[index], &previews, None,
                            false, 3, false, true, false, true, false,
                            &ignored, &ignored_users, &[], &[], &[], &low_trust,
                        ).with_pane(index as u64 + 1, index == 0);
                        stats[index] = list.show(&mut child).perf_stats;
                    }
                });
            });
            stats
        };
        for _ in 0..12 {
            render(&messages, Vec::new());
        }
        let mut next_id = 5_001;
        for (name, append, paused) in [("warm", false, false), ("live append", true, false), ("paused append", true, true)] {
            if paused {
                render(&messages, vec![
                    egui::Event::PointerMoved(egui::pos2(200.0, 180.0)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, 180.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
                for _ in 0..8 { render(&messages, Vec::new()); }
                assert_eq!(ctx.data(|data| data.get_temp::<bool>(view_state_id(&channels[0], Some(1), "scroll_paused"))), Some(true));
            }
            let mut elapsed = Vec::new();
            let mut rendered = Vec::new();
            let mut active = 0;
            let mut misses = 0;
            for _ in 0..120 {
                if append {
                    for index in 0..4 {
                        for id in next_id..next_id + 10 {
                            messages[index].pop_front();
                            messages[index].push_back(make_message(id, &channels[index]));
                        }
                    }
                    next_id += 10;
                }
                let started = Instant::now();
                let stats = render(&messages, Vec::new());
                elapsed.push(started.elapsed().as_secs_f64() * 1000.0);
                rendered.push(stats.iter().map(|stats| stats.rendered_rows).sum::<usize>());
                active = stats.iter().map(|stats| stats.active_rows).sum::<usize>();
                misses += stats.iter().map(|stats| stats.height_cache_misses).sum::<usize>();
                assert!(stats.iter().all(|stats| stats.retained_rows == 5_000));
            }
            elapsed.sort_by(f64::total_cmp);
            rendered.sort_unstable();
            eprintln!("4 channels x 5000 text rows, {name}, 120 frames: render_ms median={:.3} p95={:.3} max={:.3}; rendered median={} max={}; active={active}; height_misses={misses}",
                elapsed[60], elapsed[113], elapsed[119], rendered[60], rendered[119]);
        }
    }

    #[test]
    fn dim_filter_keeps_matching_messages_visible() {
        use crust_core::model::filters::{compile_filters, FilterRecord, FilterScope};

        let messages = VecDeque::from([test_msg("sender", "1", None)]);
        let mut filter = FilterRecord::new("dimmed", "hello", FilterScope::Global);
        filter.action = FilterAction::Dim;
        let filters = compile_filters(&[filter]);
        let ctx = egui::Context::default();
        assert_eq!(render_test_messages(&ctx, &messages, &filters).retained_rows, 1);
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
        assert_eq!(collapsed, Some("line1\nline2\nline3 ...".to_owned()));
    }

    #[test]
    fn collapse_message_for_display_uses_soft_wrap_fallback() {
        let text = "x".repeat(1_000);
        let collapsed = collapse_message_for_display(&text, 4);
        assert!(collapsed.is_some());
        let rendered = collapsed.unwrap_or_default();
        assert!(rendered.ends_with("..."));
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

}
