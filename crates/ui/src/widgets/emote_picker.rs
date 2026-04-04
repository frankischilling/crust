use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{Color32, Context, RichText, ScrollArea, Vec2};
use image::DynamicImage;
use tokio::sync::mpsc;
use tracing::info;

use crust_core::events::AppCommand;
use crust_core::model::EmoteCatalogEntry;

use super::emoji_list::emoji_catalog_entries;
use crate::theme as t;

use super::chrome;

const EMOTE_SIZE: f32 = 28.0;
const CELL_SIZE: f32 = EMOTE_SIZE + 8.0;
const ROW_H: f32 = CELL_SIZE + 4.0;
/// Fallback image fetches per frame (safety net if prefetch hasn't finished).
const FETCH_BATCH: usize = 12;
/// Max recent emotes kept in memory.
const RECENT_LIMIT: usize = 80;

/// Provider tabs - Twitch first since this is a Twitch-first client.
const TABS: &[(&str, &str)] = &[
    ("twitch", "Twitch"),
    ("7tv", "7TV"),
    ("bttv", "BTTV"),
    ("ffz", "FFZ"),
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
    filter: String,
    active_view: PickerView,
    /// URLs we've already requested fetching for (fallback lazy fetch).
    requested: HashSet<String>,
    /// Cached filtered/grouped view.
    cache: Option<CachedView>,
    /// Cached ranked indices for the current filter/tab/ranking preferences.
    display_cache: Option<CachedDisplay>,
    /// Last size we logged to avoid spamming every frame.
    last_logged_size: Option<Vec2>,
    /// Cached static textures for animated emotes (first frame).
    static_frames: HashMap<String, egui::TextureHandle>,
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
}

impl Default for EmotePicker {
    fn default() -> Self {
        Self {
            open: false,
            filter: String::new(),
            active_view: PickerView::All,
            requested: HashSet::new(),
            cache: None,
            display_cache: None,
            last_logged_size: None,
            static_frames: HashMap::new(),
            emoji_entries: emoji_catalog_entries(),
            favorite_urls: HashSet::new(),
            recent_urls: Vec::new(),
            usage_by_url: HashMap::new(),
            provider_boost: None,
            rank_revision: 0,
        }
    }
}

impl EmotePicker {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.filter.clear();
            self.cache = None;
            self.display_cache = None;
            self.active_view = PickerView::All;
            self.last_logged_size = None;
        }
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
            .filter(|v| matches!(v.as_str(), "twitch" | "7tv" | "bttv" | "ffz" | "emoji"));

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
        emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
        cmd_tx: &mpsc::Sender<AppCommand>,
        animate_emotes: bool,
    ) -> Option<String> {
        if !self.open {
            return None;
        }

        let (close_requested, next_tab, prev_tab, direct_tab) = ctx.input_mut(|i| {
            let close_requested = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            let next_tab = i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::PageDown);
            let prev_tab = i.consume_key(
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                egui::Key::Tab,
            ) || i.consume_key(egui::Modifiers::CTRL, egui::Key::PageUp);
            let direct_tab = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
            ]
            .iter()
            .position(|key| i.consume_key(egui::Modifiers::ALT | egui::Modifiers::CTRL, *key));
            (close_requested, next_tab, prev_tab, direct_tab)
        });

        if close_requested {
            self.open = false;
            return None;
        }

        if let Some(idx) = direct_tab {
            self.active_view = match idx {
                0 => PickerView::All,
                1 => PickerView::Favorites,
                2 => PickerView::Recent,
                3 => PickerView::Provider(0),
                4 => PickerView::Provider(1),
                _ => PickerView::All,
            };
        } else if next_tab {
            self.active_view = match self.active_view {
                PickerView::All => PickerView::Favorites,
                PickerView::Favorites => PickerView::Recent,
                PickerView::Recent => PickerView::Provider(0),
                PickerView::Provider(i) if i + 1 < TABS.len() => PickerView::Provider(i + 1),
                _ => PickerView::All,
            };
        } else if prev_tab {
            self.active_view = match self.active_view {
                PickerView::All => PickerView::Provider(TABS.len() - 1),
                PickerView::Favorites => PickerView::All,
                PickerView::Recent => PickerView::Favorites,
                PickerView::Provider(0) => PickerView::Recent,
                PickerView::Provider(i) => PickerView::Provider(i - 1),
            };
        }

        let mut picked: Option<String> = None;
        let mut picked_entry: Option<EmoteCatalogEntry> = None;
        let mut still_open = self.open;
        let mut next_active_view = self.active_view;
        let mut next_provider_boost = self.provider_boost.clone();
        let mut pending_favorite_toggle: Option<String> = None;

        let window_resp = egui::Window::new("Emotes")
            .open(&mut still_open)
            .default_size([280.0, 340.0])
            .min_size([200.0, 180.0])
            .resizable(true)
            .collapsible(true)
            .scroll(false)
            .show(ctx, |ui| {
                chrome::dialog_header(
                    ui,
                    "Emotes",
                    Some("Browse Twitch, 7TV, BTTV, FFZ, and emoji."),
                );
                ui.add_space(6.0);

                // Search bar
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Search emotes…")
                            .desired_width(ui.available_width()),
                    );
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

                // Provider boost controls for ranking preference.
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Boost").small().color(t::text_muted()));
                    let none_active = next_provider_boost.is_none();
                    if ui
                        .selectable_label(none_active, RichText::new("None").small())
                        .clicked()
                    {
                        next_provider_boost = None;
                    }
                    for &(provider_key, label) in TABS {
                        let selected = next_provider_boost.as_deref() == Some(provider_key);
                        if ui
                            .selectable_label(selected, RichText::new(label).small())
                            .clicked()
                        {
                            next_provider_boost = Some(provider_key.to_owned());
                        }
                    }
                });
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
                let available_h = ui.available_height();
                let cols = ((available_w / CELL_SIZE) as usize).max(1);
                let num_rows = (display_indices.len() + cols - 1) / cols;
                let mut fetches_this_frame = 0usize;
                let pointer_pos = ui.input(|i| i.pointer.hover_pos());
                let mut has_animated_visible = false;
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

                        let mut hovered_entry: Option<(usize, egui::Rect)> = None;

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

                                // Fallback lazy fetch
                                let has_bytes = emote_bytes.contains_key(entry.url.as_str());
                                if !has_bytes
                                    && fetches_this_frame < FETCH_BATCH
                                    && !self.requested.contains(&entry.url)
                                {
                                    self.requested.insert(entry.url.clone());
                                    let _ = cmd_tx.try_send(AppCommand::FetchImage {
                                        url: entry.url.clone(),
                                    });
                                    fetches_this_frame += 1;
                                }

                                let is_hovered = pointer_pos
                                    .map(|pos| cell_rect.contains(pos))
                                    .unwrap_or(false);

                                if has_bytes {
                                    let &(w, h, ref raw) =
                                        emote_bytes.get(entry.url.as_str()).unwrap();
                                    let animated = is_likely_animated_url(&entry.url);
                                    // Keep 7TV animation hover-only to avoid repaint storms in dense lists.
                                    let animate_in_grid = animated
                                        && animate_emotes
                                        && (entry.provider != "7tv" || is_hovered);
                                    if animated && !animate_in_grid {
                                        if !self.static_frames.contains_key(&entry.url) {
                                            if let Some(img) = decode_static_frame(raw) {
                                                let tex = ui.ctx().load_texture(
                                                    format!("static://{}", entry.url),
                                                    img,
                                                    egui::TextureOptions::LINEAR,
                                                );
                                                self.static_frames.insert(entry.url.clone(), tex);
                                            }
                                        }

                                        if let Some(tex) = self.static_frames.get(&entry.url) {
                                            let size = fit_size(w, h, EMOTE_SIZE);
                                            let image_rect = egui::Rect::from_center_size(
                                                cell_rect.center(),
                                                size,
                                            );
                                            ui.painter().image(
                                                tex.id(),
                                                image_rect,
                                                egui::Rect::from_min_max(
                                                    egui::pos2(0.0, 0.0),
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                Color32::WHITE,
                                            );
                                        } else {
                                            ui.painter().rect_filled(
                                                cell_rect,
                                                3.0,
                                                t::tooltip_bg(),
                                            );
                                        }
                                    } else {
                                        if animate_in_grid {
                                            has_animated_visible = true;
                                        }
                                        let size = fit_size(w, h, EMOTE_SIZE);
                                        let image_rect =
                                            egui::Rect::from_center_size(cell_rect.center(), size);
                                        let url_key = super::bytes_uri(&entry.url, raw);
                                        ui.put(
                                            image_rect,
                                            egui::Image::from_bytes(
                                                url_key,
                                                egui::load::Bytes::Shared(raw.clone()),
                                            )
                                            .fit_to_exact_size(size),
                                        );
                                    }
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

                                if is_hovered {
                                    hovered_entry = Some((cat_idx, cell_rect));
                                }
                            }
                        }

                        if let Some((cat_idx, rect)) = hovered_entry {
                            let entry = &view.combined[cat_idx];
                            let entry_url = entry.url.clone();
                            let click_resp =
                                ui.interact(rect, egui::Id::new("ep_hover"), egui::Sense::click());
                            if click_resp.clicked() {
                                picked = Some(entry.code.clone());
                                picked_entry = Some(entry.clone());
                            }

                            click_resp.context_menu(|ui| {
                                let is_favorite = self.favorite_urls.contains(entry_url.as_str());
                                let label = if is_favorite {
                                    "Remove from favorites"
                                } else {
                                    "Add to favorites"
                                };
                                if ui.button(label).clicked() {
                                    pending_favorite_toggle = Some(entry_url.clone());
                                    ui.close_menu();
                                }
                            });

                            ui.painter().rect_stroke(
                                rect.expand(2.0),
                                4.0,
                                egui::Stroke::new(1.5, t::accent()),
                                egui::epaint::StrokeKind::Outside,
                            );

                            click_resp.on_hover_ui(|ui| {
                                if let Some(&(w, h, ref raw)) = emote_bytes.get(entry.url.as_str())
                                {
                                    let size = fit_size(w, h, 48.0);
                                    ui.add(
                                        egui::Image::from_bytes(
                                            super::bytes_uri(&entry.url, raw),
                                            egui::load::Bytes::Shared(raw.clone()),
                                        )
                                        .fit_to_exact_size(size),
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

                if has_animated_visible {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(33));
                }
            });

        if picked.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            let active_view = self.active_view;
            let provider_boost = self.provider_boost.clone();
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

fn is_likely_animated_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains(".gif") || lower.contains(".webp")
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
