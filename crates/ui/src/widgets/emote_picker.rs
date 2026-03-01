use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{Color32, Context, RichText, ScrollArea, Vec2};
use image::DynamicImage;
use tokio::sync::mpsc;
use tracing::info;

use crust_core::events::AppCommand;
use crust_core::model::EmoteCatalogEntry;

const EMOTE_SIZE: f32 = 28.0;
const CELL_SIZE: f32 = EMOTE_SIZE + 8.0;
const ROW_H: f32 = CELL_SIZE + 4.0;
/// Fallback image fetches per frame (safety net if prefetch hasn't finished).
const FETCH_BATCH: usize = 12;

/// Provider tabs.
const TABS: &[(&str, &str)] = &[
    ("7tv", "7TV"),
    ("bttv", "BTTV"),
    ("ffz", "FFZ"),
];

/// Cached per-tab data.
struct CachedTab {
    indices: Vec<usize>, // indices into catalog
}

/// Cached filtered/grouped view.
struct CachedView {
    filter: String,
    catalog_len: usize,
    tabs: Vec<CachedTab>, // one per TABS entry
}

/// Floating emote picker window with provider tabs.
pub struct EmotePicker {
    pub open: bool,
    filter: String,
    /// Currently selected tab index into TABS. `None` = no tab selected.
    active_tab: Option<usize>,
    /// URLs we've already requested fetching for (fallback lazy fetch).
    requested: HashSet<String>,
    /// Cached filtered/grouped view.
    cache: Option<CachedView>,
    /// Last size we logged to avoid spamming console every frame.
    last_logged_size: Option<Vec2>,
    /// Cached static textures for animated emotes (first frame).
    static_frames: HashMap<String, egui::TextureHandle>,
}

impl Default for EmotePicker {
    fn default() -> Self {
        Self {
            open: false,
            filter: String::new(),
            active_tab: None,
            requested: HashSet::new(),
            cache: None,
            last_logged_size: None,
            static_frames: HashMap::new(),
        }
    }
}

impl EmotePicker {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.filter.clear();
            self.cache = None;
            self.active_tab = None; // no tab selected by default
            self.last_logged_size = None;
        }
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

        let filter_lower = self.filter.to_lowercase();
        let has_filter = !filter_lower.is_empty();

        // Bucket by provider in one pass
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); TABS.len()];

        for (i, entry) in catalog.iter().enumerate() {
            if has_filter && !entry.code.to_lowercase().contains(&filter_lower) {
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

        self.cache = Some(CachedView {
            filter: self.filter.clone(),
            catalog_len: catalog.len(),
            tabs,
        });
    }

    /// Show the emote picker window. Returns the emote code to insert if one was clicked.
    pub fn show(
        &mut self,
        ctx: &Context,
        catalog: &[EmoteCatalogEntry],
        emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
        cmd_tx: &mpsc::Sender<AppCommand>,
    ) -> Option<String> {
        if !self.open {
            return None;
        }

        let mut picked: Option<String> = None;
        let mut still_open = self.open;

        let window_resp = egui::Window::new("Emotes")
            .open(&mut still_open)
            .default_size([280.0, 340.0])
            .min_size([200.0, 180.0])
            .resizable(true)
            .collapsible(true)
            .scroll(false)
            .show(ctx, |ui| {
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
                let view = self.cache.as_ref().unwrap();

                // ── Tab bar ──────────────────────────────────────────
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (ti, &(_, label)) in TABS.iter().enumerate() {
                        let count = view.tabs[ti].indices.len();
                        let is_active = self.active_tab == Some(ti);
                        let text = format!("{label} ({count})");

                        let resp = ui.selectable_label(
                            is_active,
                            RichText::new(text).small(),
                        );
                        if resp.clicked() {
                            self.active_tab = if is_active { None } else { Some(ti) };
                        }
                    }
                });
                ui.separator();

                // ── Content ──────────────────────────────────────────
                let tab_data = match self.active_tab {
                    Some(ti) => &view.tabs[ti],
                    None => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.label(
                                RichText::new("Select a tab above")
                                    .color(Color32::from_rgb(120, 120, 130))
                                    .italics(),
                            );
                        });
                        return;
                    }
                };

                if tab_data.indices.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            RichText::new("No emotes")
                                .color(Color32::from_rgb(120, 120, 130))
                                .italics(),
                        );
                    });
                    return;
                }

                let available_w = ui.available_width();
                let available_h = ui.available_height();
                let cols = ((available_w / CELL_SIZE) as usize).max(1);
                let num_rows = (tab_data.indices.len() + cols - 1) / cols;
                let mut fetches_this_frame = 0usize;
                let pointer_pos = ui.input(|i| i.pointer.hover_pos());

                ScrollArea::vertical()
                    .max_height(available_h)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        // Lock content width to prevent layout feedback loop
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
                        let last_row = ((vis_bottom / ROW_H).ceil().max(0.0) as usize)
                            .min(num_rows);

                        let mut hovered_entry: Option<(usize, egui::Rect)> = None;

                        for row in first_row..last_row {
                            let start = row * cols;
                            let end = (start + cols).min(tab_data.indices.len());

                            for slot in start..end {
                                let cat_idx = tab_data.indices[slot];
                                let entry = &catalog[cat_idx];
                                let col = slot - start;

                                let cell_x =
                                    grid_rect.left() + col as f32 * CELL_SIZE;
                                let cell_y = base_y + row as f32 * ROW_H;
                                let cell_rect = egui::Rect::from_min_size(
                                    egui::pos2(cell_x, cell_y),
                                    egui::vec2(EMOTE_SIZE, EMOTE_SIZE),
                                );

                                // Fallback lazy fetch (most images should already
                                // be prefetched, but just in case)
                                let has_bytes =
                                    emote_bytes.contains_key(entry.url.as_str());
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

                                // Render
                                if has_bytes {
                                    let &(w, h, ref raw) =
                                        emote_bytes.get(entry.url.as_str()).unwrap();
                                    let animated = is_likely_animated_url(&entry.url);
                                    if animated && !is_hovered {
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
                                                Color32::from_rgb(40, 40, 48),
                                            );
                                        }
                                    } else {
                                        let size = fit_size(w, h, EMOTE_SIZE);
                                        let image_rect = egui::Rect::from_center_size(
                                            cell_rect.center(),
                                            size,
                                        );
                                        let url_key = format!("bytes://{}", entry.url);
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
                                        Color32::from_rgb(45, 45, 55),
                                    );
                                }

                                // Hover check via pointer position
                                if is_hovered {
                                    hovered_entry = Some((cat_idx, cell_rect));
                                }
                            }
                        }

                        // Single hovered cell — click + tooltip
                        if let Some((cat_idx, rect)) = hovered_entry {
                            let entry = &catalog[cat_idx];
                            let click_resp = ui.interact(
                                rect,
                                egui::Id::new("ep_hover"),
                                egui::Sense::click(),
                            );
                            if click_resp.clicked() {
                                picked = Some(entry.code.clone());
                            }
                            ui.painter().rect_stroke(
                                rect.expand(2.0),
                                4.0,
                                egui::Stroke::new(
                                    1.5,
                                    Color32::from_rgb(140, 120, 220),
                                ),
                                egui::epaint::StrokeKind::Outside,
                            );
                            click_resp.on_hover_ui(|ui| {
                                if let Some(&(w, h, ref raw)) =
                                    emote_bytes.get(entry.url.as_str())
                                {
                                    let size = fit_size(w, h, 48.0);
                                    ui.add(
                                        egui::Image::from_bytes(
                                            format!("bytes://{}", entry.url),
                                            egui::load::Bytes::Shared(raw.clone()),
                                        )
                                        .fit_to_exact_size(size),
                                    );
                                }
                                ui.label(RichText::new(&entry.code).strong());
                            });
                        }
                    });
            });

        if let Some(resp) = &window_resp {
            let size = resp.response.rect.size();
            let changed = self
                .last_logged_size
                .map(|prev| (prev.x - size.x).abs() > 0.5 || (prev.y - size.y).abs() > 0.5)
                .unwrap_or(true);
            if changed {
                info!("Emote window size: {:.1} x {:.1}", size.x, size.y);
                eprintln!("Emote window size: {:.1} x {:.1}", size.x, size.y);
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
