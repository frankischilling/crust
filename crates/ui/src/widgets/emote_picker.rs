use crate::app::UiCommandSender;
use crate::image_cache::UiImageCache;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{Color32, Context, RichText, ScrollArea, Vec2};
use tracing::info;

use crust_core::model::EmoteCatalogEntry;

use super::emoji_list::emoji_catalog_entries;
use crate::theme as t;

const EMOTE_SIZE: f32 = 28.0;
const CELL_SIZE: f32 = EMOTE_SIZE + 8.0;
const ROW_H: f32 = CELL_SIZE + 4.0;
/// Max recent emotes kept in memory.
const RECENT_LIMIT: usize = 80;

/// Provider tabs - Twitch first since this is a Twitch-first client.
const TABS: &[(&str, &str)] = &[
    ("twitch", "Twitch"),
    ("7tv", "7TV"),
    ("bttv", "BTTV"),
    ("ffz", "FFZ"),
    ("kick", "Kick"),
    ("emoji", "Emoji"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerView {
    All,
    Favorites,
    Recent,
    Provider(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmotePickerPreferences {
    pub favorites: Vec<String>,
    pub recent: Vec<String>,
    pub provider_boost: Option<String>,
}

/// Cached per-tab data.
struct CachedTab {
    indices: Vec<usize>, // indices into combined
}

/// Cached filtered/grouped view.
struct CachedView {
    filter: String,
    catalog_len: usize,
    tabs: Vec<CachedTab>, // one per TABS entry
    /// Merged indices from all provider tabs (for the "All" view).
    all_indices: Vec<usize>,
    /// Combined catalog (external emotes + emoji entries).
    combined: Vec<EmoteCatalogEntry>,
    /// First visible index per URL (stable lookup for favorites/recent).
    index_by_url: HashMap<String, usize>,
}

/// Cached ranked indices for the currently selected view.
struct CachedDisplay {
    filter: String,
    catalog_len: usize,
    active_view: PickerView,
    provider_boost: Option<String>,
    rank_revision: u64,
    indices: Arc<[usize]>,
}

/// Floating emote picker window with provider tabs.
pub struct EmotePicker {
    pub open: bool,
    id_namespace: Option<egui::Id>,
    filter: String,
    active_view: PickerView,
    /// Cached filtered/grouped view.
    cache: Option<CachedView>,
    /// Cached ranked indices for the current filter/tab/ranking preferences.
    display_cache: Option<CachedDisplay>,
    /// Last size we logged to avoid spamming every frame.
    last_logged_size: Option<Vec2>,
    /// Pre-generated emoji catalog entries (created once).
    emoji_entries: Vec<EmoteCatalogEntry>,
    /// Favorite emotes by unique URL.
    favorite_urls: HashSet<String>,
    /// Most recently used emotes by URL (most recent first).
    recent_urls: Vec<String>,
    /// Usage counts by URL for usage-based ranking.
    usage_by_url: HashMap<String, u32>,
    /// Optional provider boost key, e.g. "7tv".
    provider_boost: Option<String>,
    /// Bumped whenever ranking inputs change (favorites/recent/usage).
    rank_revision: u64,
    focus_search: bool,
    accept_search: bool,
    restore_search_focus_after: Option<u64>,
    focused_control: Option<egui::Id>,
}

impl Default for EmotePicker {
    fn default() -> Self {
        Self {
            open: false,
            id_namespace: None,
            filter: String::new(),
            active_view: PickerView::All,
            cache: None,
            display_cache: None,
            last_logged_size: None,
            emoji_entries: emoji_catalog_entries(),
            favorite_urls: HashSet::new(),
            recent_urls: Vec::new(),
            usage_by_url: HashMap::new(),
            provider_boost: None,
            rank_revision: 0,
            focus_search: false,
            accept_search: false,
            restore_search_focus_after: None,
            focused_control: None,
        }
    }
}

impl EmotePicker {
    /// Give a separate composer its own picker focus and window state.
    pub fn with_namespace(mut self, namespace: impl std::hash::Hash) -> Self {
        self.id_namespace = Some(egui::Id::new(namespace));
        self
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.accept_search = false;
        self.restore_search_focus_after = None;
        self.focused_control = None;
        if self.open {
            self.filter.clear();
            self.cache = None;
            self.display_cache = None;
            self.active_view = PickerView::All;
            self.last_logged_size = None;
            self.focus_search = true;
        }
    }

    pub fn invalidate_catalog(&mut self) {
        self.cache = None;
        self.display_cache = None;
    }

    fn scoped_id(&self, name: &'static str) -> egui::Id {
        let id = egui::Id::new(name);
        self.id_namespace.map_or(id, |namespace| id.with(namespace))
    }

    fn search_id(&self) -> egui::Id {
        self.scoped_id("emote_picker_search")
    }

    fn window_id(&self) -> egui::Id {
        self.scoped_id("Emotes")
    }

    fn window_layer(&self) -> egui::LayerId {
        egui::LayerId::new(egui::Order::Middle, self.window_id())
    }

    fn focused_widget(&self, ctx: &Context) -> Option<egui::Id> {
        if !self.open {
            return None;
        }
        // Escape clears egui's focus before widgets run. Retain the previous
        // control only while it still owned focus at the start of this pass.
        ctx.memory(|memory| {
            memory.focused().or_else(|| {
                self.focused_control
                    .filter(|id| memory.had_focus_last_frame(*id))
            })
        })
        .filter(|id| {
            ctx.read_response(*id)
                .is_some_and(|response| response.layer_id == self.window_layer())
        })
    }

    pub(crate) fn owns_keyboard(&self, ctx: &Context) -> bool {
        self.focused_widget(ctx).is_some()
    }

    /// Run before the application's shortcuts so this window owns its focused keys.
    pub fn handle_shortcuts(&mut self, ctx: &Context) {
        if !self.open {
            return;
        }
        let pass = ctx.cumulative_pass_nr();
        if self
            .restore_search_focus_after
            .is_some_and(|previous| previous < pass)
        {
            self.restore_search_focus_after = None;
            ctx.memory_mut(|memory| memory.request_focus(self.search_id()));
        }
        let focused = self.focused_widget(ctx);
        if ctx.is_context_menu_open() || focused.is_none() {
            return;
        }
        let search_owns_keys = focused == Some(self.search_id());
        let (close, next, previous, direct, accept, search) = ctx.input_mut(|input| {
            use crate::app::consume_exact_key;
            use egui::{Key, Modifiers};

            let close = consume_exact_key(input, Modifiers::NONE, Key::Escape);
            let previous = consume_exact_key(input, Modifiers::CTRL | Modifiers::SHIFT, Key::Tab)
                || consume_exact_key(input, Modifiers::CTRL, Key::PageUp);
            let next = consume_exact_key(input, Modifiers::CTRL, Key::Tab)
                || consume_exact_key(input, Modifiers::CTRL, Key::PageDown);
            let direct = [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .iter()
            .position(|key| consume_exact_key(input, Modifiers::ALT | Modifiers::CTRL, *key));
            let accept = search_owns_keys
                && consume_exact_key(input, Modifiers::NONE, Key::Enter);
            let search = consume_exact_key(input, Modifiers::CTRL, Key::F);
            (close, next, previous, direct, accept, search)
        });
        if close {
            self.open = false;
            self.accept_search = false;
            self.restore_search_focus_after = None;
            self.focused_control = None;
            return;
        }
        if search {
            self.focus_search = true;
            let id = self.search_id();
            if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(self.filter.chars().count()),
                )));
                egui::TextEdit::store_state(ctx, id, state);
            }
        }
        if previous || next {
            // egui decides Tab focus traversal before shortcuts are consumed,
            // and Shift+Tab applies its target at the start of the next pass.
            self.focus_search = true;
            self.restore_search_focus_after = Some(pass);
            ctx.request_repaint();
        }
        let current = match self.active_view {
            PickerView::All => 0,
            PickerView::Favorites => 1,
            PickerView::Recent => 2,
            PickerView::Provider(index) => index + 3,
        };
        let count = TABS.len() + 3;
        let selected = direct.unwrap_or_else(|| {
            if previous {
                (current + count - 1) % count
            } else if next {
                (current + 1) % count
            } else {
                current
            }
        });
        self.active_view = match selected {
            0 => PickerView::All,
            1 => PickerView::Favorites,
            2 => PickerView::Recent,
            index => PickerView::Provider(index - 3),
        };
        self.accept_search |= accept;
    }

    fn bump_rank_revision(&mut self) {
        self.rank_revision = self.rank_revision.wrapping_add(1);
        self.display_cache = None;
    }

    fn note_pick(&mut self, entry: &EmoteCatalogEntry) {
        let usage = self.usage_by_url.entry(entry.url.clone()).or_insert(0);
        *usage = usage.saturating_add(1);

        self.recent_urls.retain(|u| u != &entry.url);
        self.recent_urls.insert(0, entry.url.clone());
        if self.recent_urls.len() > RECENT_LIMIT {
            self.recent_urls.truncate(RECENT_LIMIT);
        }

        self.bump_rank_revision();
    }

    fn toggle_favorite_url(&mut self, url: &str) {
        if self.favorite_urls.contains(url) {
            self.favorite_urls.remove(url);
        } else {
            self.favorite_urls.insert(url.to_owned());
        }

        self.bump_rank_revision();
    }

    pub fn preferences(&self) -> EmotePickerPreferences {
        let mut favorites: Vec<String> = self.favorite_urls.iter().cloned().collect();
        favorites.sort_by_cached_key(|v| v.to_ascii_lowercase());
        EmotePickerPreferences {
            favorites,
            recent: self.recent_urls.clone(),
            provider_boost: self.provider_boost.clone(),
        }
    }

    pub fn apply_preferences(&mut self, prefs: &EmotePickerPreferences) {
        self.favorite_urls = prefs
            .favorites
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();

        self.recent_urls = prefs
            .recent
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        if self.recent_urls.len() > RECENT_LIMIT {
            self.recent_urls.truncate(RECENT_LIMIT);
        }

        self.provider_boost = prefs
            .provider_boost
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_ascii_lowercase)
            .filter(|v| {
                matches!(
                    v.as_str(),
                    "twitch" | "7tv" | "bttv" | "ffz" | "kick" | "emoji"
                )
            });

        self.bump_rank_revision();
    }

    /// Rebuild the cached view if the filter or catalog has changed.
    fn ensure_cache(&mut self, catalog: &[EmoteCatalogEntry]) {
        let need = match &self.cache {
            None => true,
            Some(c) => c.filter != self.filter || c.catalog_len != catalog.len(),
        };
        if !need {
            return;
        }

        self.display_cache = None;

        // Build the combined catalog: external emotes + emoji entries.
        let mut combined: Vec<EmoteCatalogEntry> = catalog.to_vec();
        let emoji_offset = combined.len();
        combined.extend(self.emoji_entries.iter().cloned());

        let filter_lower = self.filter.to_lowercase();
        let has_filter = !filter_lower.is_empty();

        // Bucket by provider in one pass.
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); TABS.len()];

        for (i, entry) in combined.iter().enumerate() {
            // For emoji, also search by descriptive name from source table.
            let matches_filter = if !has_filter {
                true
            } else if entry.provider == "emoji" {
                let emoji_local_idx = i.checked_sub(emoji_offset).unwrap_or(usize::MAX);
                let name_match = super::emoji_list::EMOJI_LIST
                    .get(emoji_local_idx)
                    .map(|&(_, name)| name.to_lowercase().contains(&filter_lower))
                    .unwrap_or(false);
                entry.code.to_lowercase().contains(&filter_lower) || name_match
            } else {
                entry.code.to_lowercase().contains(&filter_lower)
            };

            if !matches_filter {
                continue;
            }

            for (ti, &(provider_key, _)) in TABS.iter().enumerate() {
                if entry.provider == provider_key {
                    buckets[ti].push(i);
                    break;
                }
            }
        }

        let tabs: Vec<CachedTab> = buckets
            .into_iter()
            .map(|indices| CachedTab { indices })
            .collect();

        let all_indices: Vec<usize> = tabs
            .iter()
            .flat_map(|t| t.indices.iter().copied())
            .collect();

        let mut index_by_url: HashMap<String, usize> = HashMap::with_capacity(all_indices.len());
        for &idx in &all_indices {
            let url = combined[idx].url.clone();
            index_by_url.entry(url).or_insert(idx);
        }

        self.cache = Some(CachedView {
            filter: self.filter.clone(),
            catalog_len: catalog.len(),
            tabs,
            all_indices,
            combined,
            index_by_url,
        });
    }

    fn favorite_indices(&self, view: &CachedView) -> Vec<usize> {
        self.favorite_urls
            .iter()
            .filter_map(|url| view.index_by_url.get(url).copied())
            .collect()
    }

    fn favorite_count(&self, view: &CachedView) -> usize {
        self.favorite_urls
            .iter()
            .filter(|url| view.index_by_url.contains_key(*url))
            .count()
    }

    fn recent_indices(&self, view: &CachedView) -> Vec<usize> {
        let mut out = Vec::new();
        for url in &self.recent_urls {
            if let Some(&idx) = view.index_by_url.get(url) {
                out.push(idx);
            }
        }
        out
    }

    fn recent_count(&self, view: &CachedView) -> usize {
        self.recent_urls
            .iter()
            .filter(|url| view.index_by_url.contains_key(*url))
            .count()
    }

    fn display_indices_for(
        &mut self,
        active_view: PickerView,
        provider_boost: Option<&str>,
    ) -> Arc<[usize]> {
        let view = self.cache.as_ref().expect("cache initialized");
        let provider_boost = provider_boost.map(str::to_owned);

        let cache_hit = self
            .display_cache
            .as_ref()
            .map(|c| {
                c.filter == self.filter
                    && c.catalog_len == view.catalog_len
                    && c.active_view == active_view
                    && c.provider_boost == provider_boost
                    && c.rank_revision == self.rank_revision
            })
            .unwrap_or(false);

        if cache_hit {
            return self
                .display_cache
                .as_ref()
                .expect("display cache exists on hit")
                .indices
                .clone();
        }

        let mut indices: Vec<usize> = match active_view {
            PickerView::All => view.all_indices.clone(),
            PickerView::Favorites => self.favorite_indices(view),
            PickerView::Recent => self.recent_indices(view),
            PickerView::Provider(ti) => view
                .tabs
                .get(ti)
                .map(|t| t.indices.clone())
                .unwrap_or_default(),
        };

        self.rank_indices(
            &mut indices,
            &view.combined,
            active_view == PickerView::Recent,
            provider_boost.as_deref(),
        );

        let indices = Arc::<[usize]>::from(indices);
        self.display_cache = Some(CachedDisplay {
            filter: self.filter.clone(),
            catalog_len: view.catalog_len,
            active_view,
            provider_boost,
            rank_revision: self.rank_revision,
            indices: indices.clone(),
        });
        indices
    }

    fn rank_indices(
        &self,
        indices: &mut [usize],
        combined: &[EmoteCatalogEntry],
        keep_recent_order: bool,
        provider_boost: Option<&str>,
    ) {
        if keep_recent_order {
            return;
        }

        let query = self.filter.to_ascii_lowercase();
        let boosted = provider_boost;
        let recent_rank: HashMap<&str, usize> = self
            .recent_urls
            .iter()
            .enumerate()
            .map(|(i, url)| (url.as_str(), i))
            .collect();

        struct RankItem {
            idx: usize,
            prefix: bool,
            favorite: bool,
            boosted: bool,
            usage: u32,
            recent: usize,
            emoji: bool,
            code_len: usize,
            code_lower: String,
        }

        let mut ranked: Vec<RankItem> = indices
            .iter()
            .copied()
            .map(|idx| {
                let entry = &combined[idx];
                let code_lower = entry.code.to_ascii_lowercase();
                RankItem {
                    idx,
                    prefix: !query.is_empty() && code_lower.starts_with(&query),
                    favorite: self.favorite_urls.contains(entry.url.as_str()),
                    boosted: boosted.map(|p| entry.provider == p).unwrap_or(false),
                    usage: self
                        .usage_by_url
                        .get(entry.url.as_str())
                        .copied()
                        .unwrap_or(0),
                    recent: recent_rank
                        .get(entry.url.as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                    emoji: entry.provider == "emoji",
                    code_len: entry.code.len(),
                    code_lower,
                }
            })
            .collect();

        ranked.sort_by(|a, b| {
            b.prefix
                .cmp(&a.prefix)
                .then_with(|| b.favorite.cmp(&a.favorite))
                .then_with(|| b.boosted.cmp(&a.boosted))
                .then_with(|| b.usage.cmp(&a.usage))
                .then_with(|| a.recent.cmp(&b.recent))
                .then_with(|| a.emoji.cmp(&b.emoji))
                .then_with(|| a.code_len.cmp(&b.code_len))
                .then_with(|| a.code_lower.cmp(&b.code_lower))
        });

        for (slot, item) in indices.iter_mut().zip(ranked.into_iter()) {
            *slot = item.idx;
        }
    }

    /// Show the emote picker window. Returns the emote code to insert if one was clicked.
    pub fn show(
        &mut self,
        ctx: &Context,
        catalog: &[EmoteCatalogEntry],
        emote_bytes: &UiImageCache,
        _cmd_tx: &UiCommandSender,
        animate_emotes: bool,
    ) -> Option<String> {
        if !self.open {
            return None;
        }
        self.handle_shortcuts(ctx);
        if !self.open {
            return None;
        }
        let accept_search = std::mem::take(&mut self.accept_search);
        let search_id = self.search_id();
        let window_id = self.window_id();
        let cell_id = self.scoped_id("emote_picker_cell");

        let mut picked: Option<String> = None;
        let mut picked_entry: Option<EmoteCatalogEntry> = None;
        let mut still_open = self.open;
        let mut next_active_view = self.active_view;
        let mut next_provider_boost = self.provider_boost.clone();
        let mut pending_favorite_toggle: Option<String> = None;

        let bounds = ctx.screen_rect().shrink(8.0);
        let style = ctx.style();
        let frame = egui::Frame::window(&style);
        let title_height = ctx
            .fonts(|fonts| fonts.row_height(&egui::TextStyle::Heading.resolve(&style)))
            .max(style.spacing.interact_size.y)
            + frame.inner_margin.sum().y
            + frame.stroke.width;
        let max_size = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
            .max(egui::Vec2::splat(1.0));
        let window_resp = egui::Window::new("Emotes")
            .id(window_id)
            .open(&mut still_open)
            .default_size(egui::vec2(320.0, 380.0).min(max_size))
            .min_size(egui::vec2(200.0, 180.0).min(max_size))
            .max_size(max_size)
            .constrain_to(bounds)
            .resizable(true)
            .collapsible(false)
            .scroll(false)
            .show(ctx, |ui| {
                // Search bar
                ui.horizontal(|ui| {
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .id(search_id)
                            .hint_text("Search emotes...")
                            .desired_width(ui.available_width()),
                    );
                    if std::mem::take(&mut self.focus_search) {
                        search.request_focus();
                    }
                });
                ui.add_space(2.0);

                self.ensure_cache(catalog);

                // View tabs
                ui.horizontal_wrapped(|ui| {
                    let view = self.cache.as_ref().expect("cache initialized");
                    ui.spacing_mut().item_spacing.x = 2.0;

                    let all_count = view.all_indices.len();
                    if ui
                        .selectable_label(
                            next_active_view == PickerView::All,
                            RichText::new(format!("All ({all_count})")).small(),
                        )
                        .clicked()
                    {
                        next_active_view = PickerView::All;
                    }

                    let favorite_count = self.favorite_count(view);
                    if ui
                        .selectable_label(
                            next_active_view == PickerView::Favorites,
                            RichText::new(format!("Favorites ({favorite_count})")).small(),
                        )
                        .clicked()
                    {
                        next_active_view = PickerView::Favorites;
                    }

                    let recent_count = self.recent_count(view);
                    if ui
                        .selectable_label(
                            next_active_view == PickerView::Recent,
                            RichText::new(format!("Recent ({recent_count})")).small(),
                        )
                        .clicked()
                    {
                        next_active_view = PickerView::Recent;
                    }

                    for (ti, &(_, label)) in TABS.iter().enumerate() {
                        let count = view.tabs[ti].indices.len();
                        let is_active = next_active_view == PickerView::Provider(ti);
                        let text = format!("{label} ({count})");
                        if ui
                            .selectable_label(is_active, RichText::new(text).small())
                            .clicked()
                        {
                            next_active_view = PickerView::Provider(ti);
                        }
                    }
                });

                let order_label = TABS
                    .iter()
                    .find(|(key, _)| next_provider_boost.as_deref() == Some(*key))
                    .map(|(_, label)| format!("Prefer {label}"))
                    .unwrap_or_else(|| "Default order".to_owned());
                egui::ComboBox::from_id_salt("emote_picker_order")
                    .selected_text(order_label)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut next_provider_boost, None, "Default order");
                        for &(provider_key, label) in TABS {
                            ui.selectable_value(
                                &mut next_provider_boost,
                                Some(provider_key.to_owned()),
                                format!("Prefer {label}"),
                            );
                        }
                    })
                    .response
                    .on_hover_text("Prefer a provider after search matches and favorites.");
                ui.separator();

                let display_indices =
                    self.display_indices_for(next_active_view, next_provider_boost.as_deref());

                if display_indices.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            RichText::new("No emotes")
                                .color(t::placeholder_text())
                                .italics(),
                        );
                    });
                    return;
                }

                let available_w = ui.available_width();
                let available_h = ui.available_height().max(0.0);
                let cols = ((available_w / CELL_SIZE) as usize).max(1);
                let num_rows = (display_indices.len() + cols - 1) / cols;
                let view = self.cache.as_ref().expect("cache initialized");

                ScrollArea::vertical()
                    .max_height(available_h)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.set_min_width(available_w);
                        ui.set_max_width(available_w);

                        let total_h = num_rows as f32 * ROW_H;
                        let (grid_rect, _) = ui.allocate_exact_size(
                            egui::vec2(available_w, total_h),
                            egui::Sense::hover(),
                        );

                        let clip = ui.clip_rect();
                        let base_y = grid_rect.top();
                        let vis_top = clip.top() - base_y;
                        let vis_bottom = clip.bottom() - base_y;

                        let first_row = (vis_top / ROW_H).floor().max(0.0) as usize;
                        let last_row =
                            ((vis_bottom / ROW_H).ceil().max(0.0) as usize).min(num_rows);

                        let mut hovered_entry: Option<(usize, egui::Rect, egui::Response)> = None;

                        for row in first_row..last_row {
                            let start = row * cols;
                            let end = (start + cols).min(display_indices.len());

                            for slot in start..end {
                                let cat_idx = display_indices[slot];
                                let entry = &view.combined[cat_idx];
                                let col = slot - start;

                                let cell_x = grid_rect.left() + col as f32 * CELL_SIZE;
                                let cell_y = base_y + row as f32 * ROW_H;
                                let cell_rect = egui::Rect::from_min_size(
                                    egui::pos2(cell_x, cell_y),
                                    egui::vec2(EMOTE_SIZE, EMOTE_SIZE),
                                );

                                let cell_response = ui.interact(
                                    cell_rect,
                                    cell_id.with((entry.url.as_str(), cat_idx)),
                                    egui::Sense::click(),
                                );
                                cell_response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        ui.is_enabled(),
                                        &entry.code,
                                    )
                                });
                                let is_hovered = cell_response.hovered();
                                if cell_response.clicked() {
                                    picked = Some(entry.code.clone());
                                    picked_entry = Some(entry.clone());
                                }
                                cell_response.context_menu(|ui| {
                                    let label = if self.favorite_urls.contains(&entry.url) {
                                        "Remove from favorites"
                                    } else {
                                        "Add to favorites"
                                    };
                                    if ui.button(label).clicked() {
                                        pending_favorite_toggle = Some(entry.url.clone());
                                        ui.close_menu();
                                    }
                                });

                                if let Some(&(w, h, ref raw)) = emote_bytes.get(entry.url.as_str())
                                {
                                    // Keep 7TV animation hover-only to avoid repaint storms in dense lists.
                                    let animate_in_grid =
                                        animate_emotes && (entry.provider != "7tv" || is_hovered);
                                    let image_rect = egui::Rect::from_center_size(
                                        cell_rect.center(),
                                        fit_size(w, h, EMOTE_SIZE),
                                    );
                                    paint_picker_image(
                                        ui,
                                        emote_bytes,
                                        &entry.url,
                                        raw,
                                        image_rect,
                                        animate_in_grid,
                                    );
                                } else {
                                    ui.painter().rect_filled(
                                        cell_rect,
                                        3.0,
                                        t::section_header_bg(),
                                    );
                                }

                                if self.favorite_urls.contains(entry.url.as_str()) {
                                    ui.painter().text(
                                        egui::pos2(cell_rect.right() - 2.0, cell_rect.top() + 1.0),
                                        egui::Align2::RIGHT_TOP,
                                        "★",
                                        t::tiny(),
                                        t::gold(),
                                    );
                                }

                                if cell_response.has_focus() {
                                    ui.painter().rect_stroke(
                                        cell_rect.expand(2.0),
                                        4.0,
                                        egui::Stroke::new(2.0_f32, t::accent()),
                                        egui::epaint::StrokeKind::Outside,
                                    );
                                }

                                if is_hovered {
                                    hovered_entry = Some((cat_idx, cell_rect, cell_response));
                                }
                            }
                        }

                        if let Some((cat_idx, rect, click_resp)) = hovered_entry {
                            let entry = &view.combined[cat_idx];

                            ui.painter().rect_stroke(
                                rect.expand(2.0),
                                4.0,
                                egui::Stroke::new(1.5_f32, t::accent()),
                                egui::epaint::StrokeKind::Outside,
                            );

                            click_resp.on_hover_ui(|ui| {
                                if let Some(&(w, h, ref raw)) = emote_bytes.get(entry.url.as_str())
                                {
                                    let size = fit_size(w, h, 48.0);
                                    let (rect, _) =
                                        ui.allocate_exact_size(size, egui::Sense::hover());
                                    paint_picker_image(
                                        ui,
                                        emote_bytes,
                                        &entry.url,
                                        raw,
                                        rect,
                                        animate_emotes,
                                    );
                                }
                                let fav = if self.favorite_urls.contains(entry.url.as_str()) {
                                    " ★"
                                } else {
                                    ""
                                };
                                ui.label(RichText::new(format!("{}{}", entry.code, fav)).strong());
                                ui.label(
                                    RichText::new(format!("{}", entry.provider))
                                        .small()
                                        .color(t::text_muted()),
                                );
                            });
                        }
                    });
            });

        if picked.is_none()
            && accept_search
            && window_resp
                .as_ref()
                .is_some_and(|window| window.inner.is_some())
            && self.cache.is_some()
        {
            let active_view = next_active_view;
            let provider_boost = next_provider_boost.clone();
            let first_idx = {
                let provider_boost = provider_boost.as_deref();
                self.display_indices_for(active_view, provider_boost)
                    .first()
                    .copied()
            };
            if let Some(idx) = first_idx {
                if let Some(entry) = self
                    .cache
                    .as_ref()
                    .and_then(|view| view.combined.get(idx))
                    .cloned()
                {
                    picked = Some(entry.code.clone());
                    picked_entry = Some(entry);
                    self.open = false;
                }
            }
        }

        self.active_view = next_active_view;
        self.provider_boost = next_provider_boost;
        if let Some(url) = pending_favorite_toggle {
            self.toggle_favorite_url(&url);
        }

        if let Some(entry) = picked_entry.as_ref() {
            self.note_pick(entry);
        }

        if let Some(resp) = &window_resp {
            let size = resp.response.rect.size();
            let changed = self
                .last_logged_size
                .map(|prev| (prev.x - size.x).abs() > 0.5 || (prev.y - size.y).abs() > 0.5)
                .unwrap_or(true);
            if changed {
                info!("Emote window size: {:.1} x {:.1}", size.x, size.y);
                self.last_logged_size = Some(size);
            }
        }

        self.open = still_open;
        if picked.is_some() {
            self.open = false;
        }
        if self.open && self.restore_search_focus_after == Some(ctx.cumulative_pass_nr()) {
            // Forward Tab gives focus to the next button while laying out the
            // window, so restore it after those controls have been registered.
            ctx.memory_mut(|memory| memory.request_focus(search_id));
        }
        self.focused_control = if self.open {
            ctx.memory(|memory| memory.focused()).filter(|id| {
                ctx.read_response(*id)
                    .is_some_and(|response| response.layer_id == self.window_layer())
            })
        } else {
            self.focus_search = false;
            self.restore_search_focus_after = None;
            None
        };
        picked
    }
}

/// Scale image dimensions to a target height, preserving aspect ratio.
fn fit_size(w: u32, h: u32, max_side: f32) -> Vec2 {
    if w == 0 || h == 0 {
        return Vec2::new(max_side, max_side);
    }
    let scale_x = max_side / w as f32;
    let scale_y = max_side / h as f32;
    let scale = scale_x.min(scale_y);
    Vec2::new(w as f32 * scale, h as f32 * scale)
}

fn paint_picker_image(
    ui: &egui::Ui,
    images: &UiImageCache,
    url: &str,
    raw: &Arc<[u8]>,
    rect: egui::Rect,
    animate: bool,
) {
    if !animate && super::message_list::is_animation_format(raw) {
        if let Some(texture) = images.static_texture(ui.ctx(), url) {
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            ui.painter().rect_filled(rect, 3.0, t::tooltip_bg());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(code: &str, provider: &str, url: &str) -> EmoteCatalogEntry {
        EmoteCatalogEntry {
            id: code.to_owned(),
            url_hd: None,
            code: code.to_owned(),
            provider: provider.to_owned(),
            url: url.to_owned(),
            scope: "channel".to_owned(),
            channel: None,
            user_id: None,
            zero_width: false,
        }
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn frame(
        ctx: &Context,
        picker: &mut EmotePicker,
        catalog: &[EmoteCatalogEntry],
        events: Vec<egui::Event>,
        other_focus: bool,
    ) -> (Option<String>, egui::FullOutput) {
        let (sender, _receiver) = tokio::sync::mpsc::channel(4);
        let sender = UiCommandSender::new(sender);
        let images = UiImageCache::default();
        let mut picked = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                picker.handle_shortcuts(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    let id = egui::Id::new("other_composer");
                    if other_focus {
                        ui.memory_mut(|memory| memory.request_focus(id));
                    }
                    ui.add(egui::TextEdit::singleline(&mut String::new()).id(id));
                });
                picked = picker.show(ctx, catalog, &images, &sender, false);
            },
        );
        (picked, output)
    }

    #[test]
    fn separate_pickers_keep_queries_navigation_and_identical_cells_independent() {
        let ctx = Context::default();
        ctx.style_mut(|style| style.animation_time = 0.0);
        let mut main = EmotePicker::default();
        let mut thread = EmotePicker::default().with_namespace(("thread", "account", "root"));
        main.emoji_entries.clear();
        thread.emoji_entries.clear();
        main.toggle();
        thread.toggle();
        let catalog = [entry("Kappa", "twitch", "https://example.test/kappa.png")];
        let (sender, _receiver) = tokio::sync::mpsc::channel(4);
        let sender = UiCommandSender::new(sender);
        let images = UiImageCache::default();
        let run = |main: &mut EmotePicker, thread: &mut EmotePicker, events| {
            let mut picks = (None, None);
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1100.0, 750.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    main.handle_shortcuts(ctx);
                    thread.handle_shortcuts(ctx);
                    if let Some(code) = main.show(ctx, &catalog, &images, &sender, false) {
                        picks.0 = Some(code);
                    }
                    if let Some(code) = thread.show(ctx, &catalog, &images, &sender, false) {
                        picks.1 = Some(code);
                    }
                },
            );
            picks
        };
        for _ in 0..3 {
            run(&mut main, &mut thread, Vec::new());
        }
        assert_eq!(main.search_id(), egui::Id::new("emote_picker_search"));
        assert_eq!(main.window_id(), egui::Id::new("Emotes"));
        assert_ne!(main.window_id(), thread.window_id());
        assert_ne!(main.search_id(), thread.search_id());

        ctx.memory_mut(|memory| memory.request_focus(thread.search_id()));
        run(
            &mut main,
            &mut thread,
            vec![egui::Event::Text("Ka".into())],
        );
        assert_eq!(thread.filter, "Ka");
        assert!(main.filter.is_empty());
        assert!(thread.owns_keyboard(&ctx));
        assert!(!main.owns_keyboard(&ctx));
        run(
            &mut main,
            &mut thread,
            vec![key(egui::Key::Tab, egui::Modifiers::CTRL)],
        );
        assert!(thread.active_view == PickerView::Favorites);
        assert!(main.active_view == PickerView::All);
        run(
            &mut main,
            &mut thread,
            vec![key(
                egui::Key::Tab,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            )],
        );
        run(&mut main, &mut thread, Vec::new());
        assert!(thread.active_view == PickerView::All);
        assert!(main.filter.is_empty());
        assert_eq!(thread.filter, "Ka");

        let main_cell = main
            .scoped_id("emote_picker_cell")
            .with((catalog[0].url.as_str(), 0usize));
        let thread_cell = thread
            .scoped_id("emote_picker_cell")
            .with((catalog[0].url.as_str(), 0usize));
        assert_ne!(main_cell, thread_cell);
        assert_eq!(
            ctx.read_response(main_cell).unwrap().layer_id,
            main.window_layer()
        );
        assert_eq!(
            ctx.read_response(thread_cell).unwrap().layer_id,
            thread.window_layer()
        );
        ctx.memory_mut(|memory| memory.request_focus(main_cell));
        run(&mut main, &mut thread, Vec::new());
        assert!(main.owns_keyboard(&ctx));
        assert!(!thread.owns_keyboard(&ctx));
        let picked = run(
            &mut main,
            &mut thread,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert_eq!(picked, (Some("Kappa".into()), None));
        assert!(!main.open);
        assert!(thread.open);
        assert_eq!(thread.filter, "Ka");
    }

    fn picker_repaint_delay(raw: Vec<u8>, animate: bool) -> std::time::Duration {
        let ctx = Context::default();
        let mut images = UiImageCache::default();
        images.begin_frame(&ctx);
        let url = "https://example.test/emote/1x";
        assert!(images.complete(&ctx, url.into(), 2, 1, raw));
        let raw = images.get(url).unwrap().2.clone();
        let uri = super::super::bytes_uri(url, &raw);
        ctx.include_bytes(uri.clone(), egui::load::Bytes::Shared(raw.clone()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // Paused painting uses a separate first-frame decode and texture cache.
            // Warm the same path that paint_picker_image will use below.
            let ready = if !animate && super::super::message_list::is_animation_format(&raw) {
                images.static_texture(&ctx, url).is_some()
            } else {
                match ctx.try_load_texture(
                    &format!("{uri}#0"),
                    egui::TextureOptions::LINEAR,
                    egui::load::SizeHint::default(),
                ) {
                    Ok(egui::load::TexturePoll::Ready { .. }) => true,
                    Ok(egui::load::TexturePoll::Pending { .. }) => false,
                    Err(error) => panic!("fixture decode failed: {error}"),
                }
            };
            // Pixels become ready before the decoder requests its completion repaint.
            // Let the job finish before measuring the image's own repaint deadline.
            if ready && images.work_stats(&ctx).pending_decodes == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture decode timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut picker = EmotePicker {
            open: true,
            ..Default::default()
        };
        picker.emoji_entries.clear();
        let catalog = [entry("Fixture", "bttv", url)];
        let (sender, _receiver) = tokio::sync::mpsc::channel(4);
        let sender = UiCommandSender::new(sender);
        let mut delay = std::time::Duration::ZERO;
        for frame in 0..6 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    // Let the window's initial fade finish before measuring
                    // image deadlines at the start of a 100ms frame.
                    time: Some(if frame == 0 { 0.0 } else { 1.001 }),
                    ..Default::default()
                },
                |ctx| {
                    picker.show(ctx, &catalog, &images, &sender, animate);
                },
            );
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        delay
    }

    #[test]
    fn picker_still_webp_images_do_not_force_continuous_repaints() {
        let mut raw = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 1)
            .write_to(&mut raw, image::ImageFormat::WebP)
            .unwrap();
        assert_eq!(
            picker_repaint_delay(raw.into_inner(), true),
            std::time::Duration::MAX
        );
    }

    #[test]
    fn picker_extensionless_animations_use_frame_deadlines_and_honor_pause() {
        for raw in [
            crate::image_cache::animated_gif_fixture(),
            crate::image_cache::animated_webp_fixture(),
        ] {
            let active = picker_repaint_delay(raw.clone(), true);
            assert!(
                active > std::time::Duration::from_millis(40)
                    && active <= std::time::Duration::from_millis(100),
                "100ms frames should schedule their own deadline: {active:?}"
            );
            assert_eq!(
                picker_repaint_delay(raw, false),
                std::time::Duration::MAX,
                "paused images must stop animating even when the URL has no extension"
            );
        }
    }

    #[test]
    fn catalog_invalidation_refreshes_equal_length_provider_and_url_changes() {
        let mut picker = EmotePicker::default();
        picker.emoji_entries.clear();
        let first = entry("First", "twitch", "https://example.test/first.png");
        let second = entry("Second", "7tv", "https://example.test/second.png");
        picker.ensure_cache(&[first]);
        let _ = picker.display_indices_for(PickerView::All, None);
        picker.invalidate_catalog();
        picker.ensure_cache(&[second]);
        let indices = picker.display_indices_for(PickerView::Provider(1), None);
        assert_eq!(indices.as_ref(), &[0]);
        let cached = picker.cache.as_ref().unwrap();
        assert!(cached.tabs[0].indices.is_empty());
        assert_eq!(cached.combined[0].code, "Second");
        assert!(cached
            .index_by_url
            .contains_key("https://example.test/second.png"));
        assert!(!cached
            .index_by_url
            .contains_key("https://example.test/first.png"));
    }

    #[test]
    fn all_view_starts_with_emotes_and_keeps_explicit_emoji_preferences() {
        let mut picker = EmotePicker::default();
        picker.emoji_entries = vec![entry("🙂", "emoji", "https://example.test/smile.png")];
        let catalog = [entry(
            "LongEmoteName",
            "twitch",
            "https://example.test/emote.png",
        )];
        picker.ensure_cache(&catalog);
        assert_eq!(
            picker.display_indices_for(PickerView::All, None).as_ref(),
            &[0, 1]
        );
        assert_eq!(
            picker
                .display_indices_for(PickerView::All, Some("emoji"))
                .as_ref(),
            &[1, 0]
        );
        picker.toggle_favorite_url("https://example.test/smile.png");
        assert_eq!(
            picker.display_indices_for(PickerView::All, None).as_ref(),
            &[1, 0]
        );
    }

    #[test]
    fn open_picker_does_not_accept_enter_from_another_composer() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.emoji_entries.clear();
        picker.toggle();
        let catalog = [entry("Kappa", "twitch", "https://example.test/kappa.png")];
        frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        frame(&ctx, &mut picker, &catalog, Vec::new(), true);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new("other_composer"))));
        let (picked, _) = frame(
            &ctx,
            &mut picker,
            &catalog,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            false,
        );
        assert!(picked.is_none());
        assert!(picker.open);
    }

    #[test]
    fn search_owns_reverse_tab_and_enter_and_opens_with_focus() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.emoji_entries.clear();
        picker.toggle();
        let catalog = [entry("Kappa", "twitch", "https://example.test/kappa.png")];
        frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        assert!(ctx.memory(|memory| memory.has_focus(picker.search_id())));
        frame(
            &ctx,
            &mut picker,
            &catalog,
            vec![key(
                egui::Key::Tab,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            )],
            false,
        );
        assert!(picker.active_view == PickerView::Provider(TABS.len() - 1));
        frame(
            &ctx,
            &mut picker,
            &catalog,
            vec![key(egui::Key::Tab, egui::Modifiers::CTRL)],
            false,
        );
        assert!(picker.active_view == PickerView::All);
        assert!(ctx.memory(|memory| memory.has_focus(picker.search_id())));
        let (picked, _) = frame(
            &ctx,
            &mut picker,
            &catalog,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            false,
        );
        assert_eq!(picked.as_deref(), Some("Kappa"));
        assert!(!picker.open);
    }

    #[test]
    fn plain_tab_can_move_keyboard_focus_out_of_search() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.toggle();
        frame(&ctx, &mut picker, &[], Vec::new(), false);
        frame(
            &ctx,
            &mut picker,
            &[],
            vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
            false,
        );
        assert!(!ctx.memory(|memory| memory.has_focus(picker.search_id())));
        assert!(picker.active_view == PickerView::All);
        assert!(picker.open);
    }

    #[test]
    fn picker_controls_keep_navigation_shortcuts_inside_the_window() {
        use egui::{Key, Modifiers};

        for (key_code, modifiers, expected) in [
            (Key::Tab, Modifiers::CTRL, PickerView::Favorites),
            (Key::PageDown, Modifiers::CTRL, PickerView::Favorites),
            (
                Key::Tab,
                Modifiers::CTRL | Modifiers::SHIFT,
                PickerView::Provider(TABS.len() - 1),
            ),
            (
                Key::PageUp,
                Modifiers::CTRL,
                PickerView::Provider(TABS.len() - 1),
            ),
            (
                Key::Num5,
                Modifiers::CTRL | Modifiers::ALT,
                PickerView::Provider(1),
            ),
        ] {
            let ctx = Context::default();
            let mut picker = EmotePicker::default();
            picker.toggle();
            frame(&ctx, &mut picker, &[], Vec::new(), false);
            frame(
                &ctx,
                &mut picker,
                &[],
                vec![key(Key::Tab, Modifiers::NONE)],
                false,
            );
            frame(&ctx, &mut picker, &[], Vec::new(), false);
            assert!(picker.focused_control.is_some());
            assert_ne!(picker.focused_control, Some(picker.search_id()));

            let (picked, _) = frame(
                &ctx,
                &mut picker,
                &[],
                vec![key(key_code, modifiers)],
                false,
            );
            assert!(picked.is_none());
            assert!(picker.open);
            assert!(picker.active_view == expected, "{modifiers:?}+{key_code:?}");
            assert!(!ctx.input(|input| input.key_pressed(key_code)));
        }
    }

    #[test]
    fn escape_closes_picker_after_tabbing_to_its_controls() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.toggle();
        frame(&ctx, &mut picker, &[], Vec::new(), false);
        frame(
            &ctx,
            &mut picker,
            &[],
            vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
            false,
        );
        frame(&ctx, &mut picker, &[], Vec::new(), false);
        assert!(picker.focused_control.is_some());
        assert_ne!(picker.focused_control, Some(picker.search_id()));
        let (picked, _) = frame(
            &ctx,
            &mut picker,
            &[],
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
            false,
        );
        assert!(picked.is_none());
        assert!(!picker.open);
        assert!(picker.focused_control.is_none());
    }

    #[test]
    fn search_shortcut_refocuses_picker_and_selects_existing_text() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.toggle();
        frame(&ctx, &mut picker, &[], Vec::new(), false);
        frame(
            &ctx,
            &mut picker,
            &[],
            vec![egui::Event::Text("Kåppa🙂".to_owned())],
            false,
        );
        frame(
            &ctx,
            &mut picker,
            &[],
            vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
            false,
        );
        frame(&ctx, &mut picker, &[], Vec::new(), false);
        assert_ne!(picker.focused_control, Some(picker.search_id()));
        frame(
            &ctx,
            &mut picker,
            &[],
            vec![key(egui::Key::F, egui::Modifiers::CTRL)],
            false,
        );
        assert!(ctx.memory(|memory| memory.has_focus(picker.search_id())));
        assert!(!ctx.input(|input| input.key_pressed(egui::Key::F)));
        let range = egui::TextEdit::load_state(&ctx, picker.search_id())
            .unwrap()
            .cursor
            .char_range()
            .unwrap();
        let start = range.primary.index.min(range.secondary.index);
        let end = range.primary.index.max(range.secondary.index);
        assert_eq!(start, 0);
        assert_eq!(end, "Kåppa🙂".chars().count());
        frame(
            &ctx,
            &mut picker,
            &[],
            vec![egui::Event::Text("Kappa".to_owned())],
            false,
        );
        assert_eq!(picker.filter, "Kappa");
    }

    #[test]
    fn focused_emote_cells_show_focus_and_accept_their_own_entry() {
        let ctx = Context::default();
        ctx.style_mut(|style| style.animation_time = 0.0);
        let mut picker = EmotePicker::default();
        picker.emoji_entries.clear();
        picker.toggle();
        let catalog = [
            entry("Kappa", "twitch", "https://example.test/kappa.png"),
            entry("PogChamp", "twitch", "https://example.test/pogchamp.png"),
        ];
        frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        let id = egui::Id::new("emote_picker_cell").with((catalog[1].url.as_str(), 1usize));
        ctx.memory_mut(|memory| memory.request_focus(id));
        let (_, output) = frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        let cell = ctx.read_response(id).unwrap();
        assert!(cell.has_focus());
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Rect(rect) if rect.rect == cell.rect.expand(2.0)
                && rect.stroke.width == 2.0 && rect.stroke.color == t::accent())));
        let (picked, output) = frame(
            &ctx,
            &mut picker,
            &catalog,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            false,
        );
        assert_eq!(picked.as_deref(), Some("PogChamp"));
        assert!(!picker.open);
        assert!(output.platform_output.events.iter().any(|event| matches!(event,
            egui::output::OutputEvent::Clicked(info)
                if info.typ == egui::WidgetType::Button
                    && info.label.as_deref() == Some("PogChamp"))));
    }

    #[test]
    fn picker_shortcuts_leave_external_composer_keys_available() {
        for (key_code, modifiers) in [
            (egui::Key::Tab, egui::Modifiers::CTRL),
            (egui::Key::F, egui::Modifiers::CTRL),
            (egui::Key::Escape, egui::Modifiers::NONE),
        ] {
            let ctx = Context::default();
            let mut picker = EmotePicker::default();
            picker.toggle();
            frame(&ctx, &mut picker, &[], Vec::new(), false);
            frame(&ctx, &mut picker, &[], Vec::new(), true);
            let (picked, _) = frame(
                &ctx,
                &mut picker,
                &[],
                vec![key(key_code, modifiers)],
                false,
            );
            assert!(picked.is_none());
            assert!(picker.open);
            assert!(picker.active_view == PickerView::All);
            assert!(ctx.input(|input| input.key_pressed(key_code)));
        }
    }

    #[test]
    fn escape_closes_the_picker_after_search_loses_egui_focus() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.toggle();
        frame(&ctx, &mut picker, &[], Vec::new(), false);
        let (picked, _) = frame(
            &ctx,
            &mut picker,
            &[],
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
            false,
        );
        assert!(picked.is_none());
        assert!(!picker.open);
    }

    fn pointer(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    #[test]
    fn favorite_menu_stays_open_when_pointer_leaves_its_cell() {
        let ctx = Context::default();
        let mut picker = EmotePicker::default();
        picker.emoji_entries.clear();
        picker.toggle();
        let catalog = [entry("Kappa", "twitch", "https://example.test/kappa.png")];
        frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        let cell = ctx
            .read_response(
                egui::Id::new("emote_picker_cell").with((catalog[0].url.as_str(), 0usize)),
            )
            .unwrap()
            .rect;
        frame(
            &ctx,
            &mut picker,
            &catalog,
            pointer(cell.center(), egui::PointerButton::Secondary, true),
            false,
        );
        frame(
            &ctx,
            &mut picker,
            &catalog,
            pointer(cell.center(), egui::PointerButton::Secondary, false),
            false,
        );
        assert!(ctx.is_context_menu_open());
        // A new egui popup is measured on its first pass before it is painted.
        let (_, output) = frame(&ctx, &mut picker, &catalog, Vec::new(), false);
        let menu_pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Add to favorites" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .expect("favorite action is visible");
        assert!(!cell.contains(menu_pos));
        frame(
            &ctx,
            &mut picker,
            &catalog,
            vec![egui::Event::PointerMoved(menu_pos)],
            false,
        );
        assert!(ctx.is_context_menu_open());
        frame(
            &ctx,
            &mut picker,
            &catalog,
            pointer(menu_pos, egui::PointerButton::Primary, true),
            false,
        );
        let (picked, _) = frame(
            &ctx,
            &mut picker,
            &catalog,
            pointer(menu_pos, egui::PointerButton::Primary, false),
            false,
        );
        assert!(picked.is_none());
        assert_eq!(picker.preferences().favorites, vec![catalog[0].url.clone()]);
    }
}
