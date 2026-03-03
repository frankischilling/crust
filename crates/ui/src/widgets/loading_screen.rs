use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Context, FontId, Pos2, Vec2};

use crate::theme as t;

// tunables 

/// How long to wait before declaring "done" even if not all channels have
/// emotes and history (catches slow/missing responses).
const DONE_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum fraction of prefetched images that must arrive before declaring
/// done (allows for a small number of failed fetches without hanging forever).
const IMAGES_DONE_PCT: f32 = 0.95;

/// How long to keep the overlay visible after "ready" before fading out.
const FADE_DURATION: Duration = Duration::from_millis(700);

// public event feed 

/// Subset of app events that the loading screen cares about.
pub enum LoadEvent {
    Connecting,
    Connected,
    Authenticated { username: String },
    ChannelJoined { channel: String },
    CatalogLoaded { count: usize },
    HistoryLoaded { channel: String, count: usize },
    EmoteImageReady,
    ChannelEmotesLoaded { channel: String, count: usize },
    /// A new batch of images has been queued for background prefetch.
    ImagePrefetchQueued { count: usize },
}

// state machine 

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    Authenticating,
    Loading,
    Ready,
    Done,
}

// widget

pub struct LoadingScreen {
    started_at:             Instant,
    phase:                  Phase,
    ready_at:               Option<Instant>,

    // Progress counters
    authenticated_user:     Option<String>,
    channels_joined:        Vec<String>,
    channels_with_history:  HashSet<String>,
    channels_with_emotes:   HashSet<String>,
    catalog_count:          usize,
    catalog_loaded:         bool,
    images_loaded:          usize,
    images_expected:        usize,
    channel_emote_totals:   std::collections::HashMap<String, usize>,

    // Live log
    log:                    VecDeque<LogLine>,
}

struct LogLine {
    text:  String,
    color: Color32,
}

impl Default for LoadingScreen {
    fn default() -> Self {
        Self {
            started_at:            Instant::now(),
            phase:                 Phase::Connecting,
            ready_at:              None,
            authenticated_user:    None,
            channels_joined:       Vec::new(),
            channels_with_history: HashSet::new(),
            channels_with_emotes:  HashSet::new(),
            catalog_count:         0,
            catalog_loaded:        false,
            images_loaded:         0,
            images_expected:       0,
            channel_emote_totals:  std::collections::HashMap::new(),
            log:                   VecDeque::new(),
        }
    }
}

impl LoadingScreen {
    /// Feed an event; returns whether the loading screen is still active.
    pub fn on_event(&mut self, evt: LoadEvent) {
        match evt {
            LoadEvent::Connecting => {
                self.phase = Phase::Connecting;
                self.push_log("Connecting to Twitch…", t::TEXT_SECONDARY);
            }
            LoadEvent::Connected => {
                self.phase = Phase::Authenticating;
                self.push_log("Connected — authenticating…", t::GREEN);
            }
            LoadEvent::Authenticated { username } => {
                self.push_log(format!("Authenticated as {username}"), t::GREEN);
                self.authenticated_user = Some(username);
                self.phase = Phase::Loading;
            }
            LoadEvent::ChannelJoined { channel } => {
                self.push_log(format!("Joined #{channel}"), t::TEXT_PRIMARY);
                if !self.channels_joined.contains(&channel) {
                    self.channels_joined.push(channel);
                }
            }
            LoadEvent::CatalogLoaded { count } => {
                self.push_log(format!("Global emotes ready — {count} emotes"), t::ACCENT);
                self.catalog_count = count;
                self.catalog_loaded = true;
                self.check_done();
            }
            LoadEvent::HistoryLoaded { channel, count } => {
                self.push_log(
                    format!("#{channel}: {count} history messages"),
                    t::TEXT_SECONDARY,
                );
                self.channels_with_history.insert(channel);
                self.check_done();
            }
            LoadEvent::EmoteImageReady => {
                self.images_loaded += 1;
                // Re-check done on every image arrival.
                self.check_done();
            }
            LoadEvent::ImagePrefetchQueued { count } => {
                self.images_expected += count;
                // Don't log — too noisy.
            }
            LoadEvent::ChannelEmotesLoaded { channel, count } => {
                if count > 0 {
                    self.push_log(
                        format!("#{channel}: {count} channel emotes"),
                        t::TEXT_SECONDARY,
                    );
                } else {
                    self.push_log(
                        format!("#{channel}: no channel emotes"),
                        t::TEXT_MUTED,
                    );
                }
                self.channel_emote_totals.insert(channel.clone(), count);
                self.channels_with_emotes.insert(channel);
                self.check_done();
            }
        }
    }

    /// Call every frame; advances the timeout-based done detection.
    pub fn tick(&mut self) {
        if self.phase == Phase::Loading
            && self.started_at.elapsed() >= DONE_TIMEOUT
        {
            self.mark_ready();
        }
    }

    /// Returns `true` while the overlay should be rendered (including fade-out).
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Done
    }

    /// Render the full-window loading overlay.
    /// Call this **instead of** the normal UI when `is_active()` is true.
    pub fn show(&mut self, ctx: &Context) {
        self.tick();

        // Compute fade-out alpha
        let alpha = match (self.phase, self.ready_at) {
            (Phase::Done, _) => return,
            (Phase::Ready, Some(t)) => {
                let prog = t.elapsed().as_secs_f32() / FADE_DURATION.as_secs_f32();
                if prog >= 1.0 {
                    self.phase = Phase::Done;
                    ctx.request_repaint();
                    return;
                }
                ctx.request_repaint_after(Duration::from_millis(16));
                1.0 - prog
            }
            _ => 1.0,
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t::BG_BASE))
            .show(ctx, |ui| {
                let rect   = ui.max_rect();
                let center = rect.center();
                let painter = ui.painter();

                // Fade whole overlay by painting a semi-transparent cover when fading
                if alpha < 1.0 {
                    painter.rect_filled(
                        rect,
                        egui::CornerRadius::ZERO,
                        Color32::from_rgba_unmultiplied(
                            t::BG_BASE.r(), t::BG_BASE.g(), t::BG_BASE.b(),
                            (alpha * 255.0) as u8,
                        ),
                    );
                }

                let a = |c: Color32| {
                    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0) as u8)
                };

                // ── Logo ────────────────────────────────────────────────────
                let logo_pos = Pos2::new(center.x, center.y - 90.0);
                painter.text(
                    logo_pos,
                    Align2::CENTER_CENTER,
                    "crust",
                    FontId::proportional(42.0),
                    a(t::ACCENT),
                );

                // Sub-label
                painter.text(
                    Pos2::new(center.x, center.y - 56.0),
                    Align2::CENTER_CENTER,
                    "Twitch chat client",
                    FontId::proportional(12.0),
                    a(t::TEXT_MUTED),
                );

                // Spinner
                if self.phase != Phase::Ready {
                    let t_val = ui.input(|i| i.time) as f32;
                    let radius = 18.0_f32;
                    let spinner_center = Pos2::new(center.x, center.y - 10.0);
                    let segments = 12usize;
                    for i in 0..segments {
                        let angle = (i as f32 / segments as f32) * std::f32::consts::TAU;
                        let spin  = (t_val * 1.8).rem_euclid(1.0);
                        let dot_age = ((i as f32 / segments as f32) - spin).rem_euclid(1.0);
                        let dot_alpha = ((1.0 - dot_age) * alpha * 220.0) as u8;
                        let p = Pos2::new(
                            spinner_center.x + radius * angle.cos(),
                            spinner_center.y + radius * angle.sin(),
                        );
                        painter.circle_filled(
                            p,
                            2.5,
                            Color32::from_rgba_unmultiplied(
                                t::ACCENT.r(), t::ACCENT.g(), t::ACCENT.b(), dot_alpha,
                            ),
                        );
                    }
                    ctx.request_repaint_after(Duration::from_millis(16));
                } else {
                    // ✓ checkmark when ready
                    painter.text(
                        Pos2::new(center.x, center.y - 10.0),
                        Align2::CENTER_CENTER,
                        "✓",
                        FontId::proportional(28.0),
                        a(t::GREEN),
                    );
                }

                // ── Progress pills ──────────────────────────────────────────
                let pill_y = center.y + 24.0;
                let mut pills: Vec<(String, Color32)> = Vec::new();

                // Connection state pill
                let conn_pill = match self.phase {
                    Phase::Connecting    => ("● Connecting",    t::YELLOW),
                    Phase::Authenticating => ("● Authenticating", t::YELLOW),
                    _                    => ("● Connected",      t::GREEN),
                };
                pills.push((conn_pill.0.to_owned(), conn_pill.1));

                // Joined channels
                if !self.channels_joined.is_empty() {
                    let names = self.channels_joined.iter()
                        .map(|c| format!("#{c}"))
                        .collect::<Vec<_>>()
                        .join("  ");
                    pills.push((names, a(t::TEXT_SECONDARY)));
                }

                // Global emotes
                if self.catalog_loaded {
                    pills.push((
                        format!("{} global emotes", self.catalog_count),
                        a(t::ACCENT),
                    ));
                } else if self.phase == Phase::Loading {
                    pills.push(("Loading global emotes…".to_owned(), a(t::TEXT_MUTED)));
                }

                // Image prefetch progress — show once we know total expected
                if self.images_expected > 0 {
                    let pct = ((self.images_loaded as f32 / self.images_expected as f32) * 100.0)
                        .min(100.0) as usize;
                    let color = if pct >= 95 { a(t::GREEN) } else { a(t::TEXT_SECONDARY) };
                    pills.push((
                        format!("{} / {} images  ({}%)",
                            self.images_loaded, self.images_expected, pct),
                        color,
                    ));
                } else if self.images_loaded > 0 {
                    pills.push((format!("{} images", self.images_loaded), a(t::TEXT_SECONDARY)));
                }

                // Render pills in a centered row, wrapping if needed
                let pill_font  = FontId::proportional(11.0);
                let pill_h     = 18.0_f32;
                let pill_pad   = 8.0_f32;
                let pill_gap   = 6.0_f32;
                let max_row_w  = rect.width() * 0.75;

                let mut row_y = pill_y;
                let total_pills_w: f32 = pills.iter().map(|(text, _)| {
                    let gal = painter.layout_no_wrap(text.clone(), pill_font.clone(), Color32::WHITE);
                    gal.size().x + pill_pad * 2.0 + pill_gap
                }).sum::<f32>() - pill_gap;

                let origin_x  = center.x - (total_pills_w / 2.0).min(max_row_w / 2.0);
                let mut row_x = origin_x;

                for (text, color) in &pills {
                    let gal = painter.layout_no_wrap(
                        text.clone(), pill_font.clone(), a(*color),
                    );
                    let pill_w = gal.size().x + pill_pad * 2.0;
                    if row_x + pill_w > center.x + max_row_w / 2.0 + 10.0 {
                        row_x = origin_x;
                        row_y += pill_h + 4.0;
                    }
                    let pill_rect = egui::Rect::from_min_size(
                        Pos2::new(row_x, row_y),
                        Vec2::new(pill_w, pill_h),
                    );
                    painter.rect_filled(
                        pill_rect,
                        egui::CornerRadius::same(9),
                        Color32::from_rgba_unmultiplied(
                            t::BG_RAISED.r(), t::BG_RAISED.g(), t::BG_RAISED.b(),
                            (alpha * 180.0) as u8,
                        ),
                    );
                    painter.galley(
                        Pos2::new(row_x + pill_pad, row_y + (pill_h - gal.size().y) / 2.0),
                        gal,
                        a(*color),
                    );
                    row_x += pill_w + pill_gap;
                }

                // Log lines 
                let log_start_y = row_y + pill_h + 18.0;
                let log_font    = FontId::proportional(11.0);
                let line_h      = 15.0_f32;
                for (i, line) in self.log.iter().rev().take(8).enumerate() {
                    let line_alpha = {
                        let base = alpha;
                        let fade = 1.0 - (i as f32 / 8.0);
                        (base * fade * 0.9).max(0.0)
                    };
                    let col = Color32::from_rgba_unmultiplied(
                        line.color.r(), line.color.g(), line.color.b(),
                        (line_alpha * 255.0) as u8,
                    );
                    painter.text(
                        Pos2::new(center.x, log_start_y + i as f32 * line_h),
                        Align2::CENTER_TOP,
                        &line.text,
                        log_font.clone(),
                        col,
                    );
                }
            });
    }

    // private 

    fn push_log(&mut self, text: impl Into<String>, color: Color32) {
        self.log.push_back(LogLine { text: text.into(), color });
        while self.log.len() > 40 {
            self.log.pop_front();
        }
    }

    fn check_done(&mut self) {
        if self.phase != Phase::Loading { return; }
        let auth_ok    = self.authenticated_user.is_some();
        let catalog_ok = self.catalog_loaded;
        let history_ok = !self.channels_joined.is_empty()
            && self.channels_joined.iter()
               .all(|ch| self.channels_with_history.contains(ch));
        let emotes_ok  = !self.channels_joined.is_empty()
            && self.channels_joined.iter()
               .all(|ch| self.channels_with_emotes.contains(ch));
        // Wait for the bulk of prefetched images to arrive so the first paint
        // after the loading screen shows real emotes/badges, not blank boxes.
        let images_ok  = self.images_expected == 0
            || (self.images_loaded as f32 / self.images_expected as f32) >= IMAGES_DONE_PCT;
        if auth_ok && catalog_ok && history_ok && emotes_ok && images_ok {
            self.push_log(
                format!("Ready! ({}/{} images)", self.images_loaded, self.images_expected),
                t::GREEN,
            );
            self.mark_ready();
        }
    }

    fn mark_ready(&mut self) {
        if self.phase == Phase::Loading || self.phase == Phase::Connecting || self.phase == Phase::Authenticating {
            self.phase    = Phase::Ready;
            self.ready_at = Some(Instant::now());
        }
    }
}
