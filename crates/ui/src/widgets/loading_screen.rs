use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Context, FontId, Pos2, Vec2};

use crate::theme as t;

// tunables

/// How long to wait before declaring "done" even if not all channels have
/// emotes and history (catches slow/missing responses).
const DONE_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum fraction of prefetched images that must arrive before declaring
/// done - kept for reference but no longer used to gate the loading screen.
/// Images now prefetch in the background after the overlay is dismissed.
#[allow(dead_code)]
const IMAGES_DONE_PCT: f32 = 0.95;

/// After entering the `Loading` phase, wait at least this long before allowing
/// `check_done` to succeed.  This gives channel-join events time to arrive so
/// we don't dismiss the loading screen before any channel data has loaded.
const MIN_LOADING_GRACE: Duration = Duration::from_secs(2);

/// How long to keep the overlay visible after "ready" before fading out.
const FADE_DURATION: Duration = Duration::from_millis(700);
const LOADING_LOG_ROWS: usize = 10;
const LOADING_LOG_ROW_HEIGHT: f32 = 16.0;
const SUPER_NARROW_WIDTH: f32 = 320.0;
const SUPER_NARROW_LOG_ROWS: usize = 6;

// public event feed

/// Subset of app events that the loading screen cares about.
pub enum LoadEvent {
    Connecting,
    Connected,
    Authenticated {
        username: String,
    },
    ChannelJoined {
        channel: String,
    },
    CatalogLoaded {
        count: usize,
    },
    HistoryLoaded {
        channel: String,
        count: usize,
    },
    EmoteImageReady,
    ChannelEmotesLoaded {
        channel: String,
        count: usize,
    },
    /// A new batch of images has been queued for background prefetch.
    ImagePrefetchQueued {
        count: usize,
    },
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
    started_at: Instant,
    phase: Phase,
    ready_at: Option<Instant>,
    /// Wall-clock instant when we entered the `Loading` phase (for grace period).
    loading_entered_at: Option<Instant>,
    /// True when startup proceeds in anonymous mode (no GLOBALUSERSTATE).
    auth_optional: bool,

    // Progress counters
    authenticated_user: Option<String>,
    channels_joined: Vec<String>,
    channels_with_history: HashSet<String>,
    channels_with_emotes: HashSet<String>,
    catalog_count: usize,
    catalog_loaded: bool,
    images_loaded: usize,
    images_expected: usize,
    channel_emote_totals: std::collections::HashMap<String, usize>,

    // Live log
    log: VecDeque<LogLine>,
}

struct LogLine {
    text: String,
    color: Color32,
}

impl Default for LoadingScreen {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            phase: Phase::Connecting,
            ready_at: None,
            loading_entered_at: None,
            auth_optional: false,
            authenticated_user: None,
            channels_joined: Vec::new(),
            channels_with_history: HashSet::new(),
            channels_with_emotes: HashSet::new(),
            catalog_count: 0,
            catalog_loaded: false,
            images_loaded: 0,
            images_expected: 0,
            channel_emote_totals: std::collections::HashMap::new(),
            log: VecDeque::new(),
        }
    }
}

impl LoadingScreen {
    /// Feed an event; returns whether the loading screen is still active.
    pub fn on_event(&mut self, evt: LoadEvent) {
        // Startup-only overlay: once we've reached ready/done, ignore any
        // subsequent connection/auth events (e.g. logout reconnect).
        if matches!(self.phase, Phase::Ready | Phase::Done) {
            return;
        }

        match evt {
            LoadEvent::Connecting => {
                // Only regress the phase if we haven't advanced past Connecting.
                if matches!(self.phase, Phase::Connecting) {
                    self.phase = Phase::Connecting;
                }
                self.push_log("Connecting to Twitch…", t::TEXT_SECONDARY);
            }
            LoadEvent::Connected => {
                // Don't regress from Loading if an optimistic Authenticated
                // event already advanced us.
                if matches!(self.phase, Phase::Connecting | Phase::Authenticating) {
                    self.phase = Phase::Authenticating;
                }
                self.push_log("Connected - authenticating…", t::GREEN);
            }
            LoadEvent::Authenticated { username } => {
                self.push_log(format!("Authenticated as {username}"), t::GREEN);
                self.authenticated_user = Some(username);
                self.auth_optional = false;
                self.enter_loading();
            }
            LoadEvent::ChannelJoined { channel } => {
                self.maybe_enter_anonymous_loading();
                self.push_log(format!("Joined #{channel}"), t::TEXT_PRIMARY);
                if !self.channels_joined.contains(&channel) {
                    self.channels_joined.push(channel);
                }
                self.check_done();
            }
            LoadEvent::CatalogLoaded { count } => {
                self.maybe_enter_anonymous_loading();
                self.push_log(format!("Global emotes ready - {count} emotes"), t::ACCENT);
                self.catalog_count = count;
                self.catalog_loaded = true;
                self.check_done();
            }
            LoadEvent::HistoryLoaded { channel, count } => {
                self.maybe_enter_anonymous_loading();
                self.push_log(
                    format!("#{channel}: {count} history messages"),
                    t::TEXT_SECONDARY,
                );
                self.channels_with_history.insert(channel);
                self.check_done();
            }
            LoadEvent::EmoteImageReady => {
                self.maybe_enter_anonymous_loading();
                self.images_loaded += 1;
                // Re-check done on every image arrival.
                self.check_done();
            }
            LoadEvent::ImagePrefetchQueued { count } => {
                self.maybe_enter_anonymous_loading();
                self.images_expected += count;
                // Don't log - too noisy.
            }
            LoadEvent::ChannelEmotesLoaded { channel, count } => {
                self.maybe_enter_anonymous_loading();
                if count > 0 {
                    self.push_log(
                        format!("#{channel}: {count} channel emotes"),
                        t::TEXT_SECONDARY,
                    );
                } else {
                    self.push_log(format!("#{channel}: no channel emotes"), t::TEXT_MUTED);
                }
                self.channel_emote_totals.insert(channel.clone(), count);
                self.channels_with_emotes.insert(channel);
                self.check_done();
            }
        }
    }

    /// Call every frame; advances the timeout-based done detection.
    pub fn tick(&mut self) {
        if matches!(self.phase, Phase::Connecting | Phase::Authenticating | Phase::Loading)
            && self.started_at.elapsed() >= DONE_TIMEOUT
        {
            self.mark_ready();
        }
        // Re-run the normal done check every frame so that the grace period
        // expiry is caught even when no new events arrive (e.g. the user has
        // no auto-joined channels and CatalogLoaded fired during the grace window).
        self.check_done();
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
                let rect = ui.max_rect();
                let painter = ui.painter();
                let a = |c: Color32| {
                    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0) as u8)
                };

                // Subtle background accents so the panel doesn't feel flat.
                painter.circle_filled(
                    Pos2::new(rect.left() + 90.0, rect.top() + 70.0),
                    130.0,
                    Color32::from_rgba_unmultiplied(
                        t::ACCENT.r(),
                        t::ACCENT.g(),
                        t::ACCENT.b(),
                        (alpha * 20.0) as u8,
                    ),
                );
                painter.circle_filled(
                    Pos2::new(rect.right() - 80.0, rect.bottom() - 60.0),
                    140.0,
                    Color32::from_rgba_unmultiplied(
                        t::BORDER_ACCENT.r(),
                        t::BORDER_ACCENT.g(),
                        t::BORDER_ACCENT.b(),
                        (alpha * 18.0) as u8,
                    ),
                );

                if rect.width() <= SUPER_NARROW_WIDTH {
                    self.show_super_narrow(ctx, rect, alpha);
                    return;
                }

                let card_w = (rect.width() - 36.0).clamp(200.0, 760.0);
                egui::Area::new(egui::Id::new("loading_center_card"))
                    .order(egui::Order::Foreground)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .interactable(false)
                    .show(ctx, |ui| {
                        ui.set_width(card_w);
                        egui::Frame::new()
                            .fill(a(t::BG_SURFACE))
                            .stroke(egui::Stroke::new(1.0, a(t::BORDER_SUBTLE)))
                            .corner_radius(t::RADIUS)
                            .inner_margin(egui::Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(card_w - 32.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        egui::RichText::new("crust")
                                            .font(FontId::proportional(40.0))
                                            .color(a(t::ACCENT)),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            "Twitch chat client (Kick/IRC optional beta)",
                                        )
                                        .font(FontId::proportional(12.0))
                                        .color(a(t::TEXT_MUTED)),
                                    );
                                });

                                ui.add_space(10.0);

                                ui.vertical_centered(|ui| {
                                    let (spin_rect, _) = ui.allocate_exact_size(
                                        Vec2::new(64.0, 64.0),
                                        egui::Sense::hover(),
                                    );
                                    let center = spin_rect.center();
                                    let spin_painter = ui.painter();
                                    if self.phase != Phase::Ready {
                                        let t_val = ui.input(|i| i.time) as f32;
                                        let radius = 20.0_f32;
                                        let segments = 12usize;
                                        for i in 0..segments {
                                            let angle = (i as f32 / segments as f32)
                                                * std::f32::consts::TAU;
                                            let spin = (t_val * 1.8).rem_euclid(1.0);
                                            let dot_age = ((i as f32 / segments as f32) - spin)
                                                .rem_euclid(1.0);
                                            let dot_alpha =
                                                ((1.0 - dot_age) * alpha * 220.0) as u8;
                                            let p = Pos2::new(
                                                center.x + radius * angle.cos(),
                                                center.y + radius * angle.sin(),
                                            );
                                            spin_painter.circle_filled(
                                                p,
                                                2.7,
                                                Color32::from_rgba_unmultiplied(
                                                    t::ACCENT.r(),
                                                    t::ACCENT.g(),
                                                    t::ACCENT.b(),
                                                    dot_alpha,
                                                ),
                                            );
                                        }
                                        ctx.request_repaint_after(Duration::from_millis(16));
                                    } else {
                                        spin_painter.text(
                                            center,
                                            Align2::CENTER_CENTER,
                                            "✓",
                                            FontId::proportional(28.0),
                                            a(t::GREEN),
                                        );
                                    }
                                });

                                let stage = self.stage_label();
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        egui::RichText::new(stage.0)
                                            .font(t::small())
                                            .color(a(stage.1)),
                                    );
                                });

                                ui.add_space(10.0);
                                let details_w = (card_w - 32.0).clamp(160.0, 560.0);
                                ui.vertical_centered(|ui| {
                                    ui.set_min_width(details_w);
                                    ui.set_max_width(details_w);
                                            let mut pills: Vec<(String, Color32)> = Vec::new();
                                            let conn = self.connection_status();
                                            let conn_pill = (format!("● {}", conn.0), conn.1);
                                            pills.push(conn_pill);

                                            if !self.channels_joined.is_empty() {
                                                let text = if self.channels_joined.len() <= 4 {
                                                    self.channels_joined
                                                        .iter()
                                                        .map(|c| format!("#{c}"))
                                                        .collect::<Vec<_>>()
                                                        .join("  ")
                                                } else {
                                                    format!(
                                                        "{} channels joined",
                                                        self.channels_joined.len()
                                                    )
                                                };
                                                pills.push((text, t::TEXT_SECONDARY));
                                            }

                                            if self.catalog_loaded {
                                                pills.push((
                                                    format!("{} global emotes", self.catalog_count),
                                                    t::ACCENT,
                                                ));
                                            } else if self.phase == Phase::Loading {
                                                pills.push((
                                                    "Loading global emotes…".to_owned(),
                                                    t::TEXT_MUTED,
                                                ));
                                            }

                                            if self.images_expected > 0 {
                                                let pct = ((self.images_loaded as f32
                                                    / self.images_expected as f32)
                                                    * 100.0)
                                                    .min(100.0)
                                                    as usize;
                                                let c = if pct >= 95 {
                                                    t::GREEN
                                                } else {
                                                    t::TEXT_SECONDARY
                                                };
                                                pills.push((
                                                    format!(
                                                        "{} / {} images ({pct}%)",
                                                        self.images_loaded, self.images_expected
                                                    ),
                                                    c,
                                                ));
                                            } else if self.images_loaded > 0 {
                                                pills.push((
                                                    format!("{} images", self.images_loaded),
                                                    t::TEXT_SECONDARY,
                                                ));
                                            }

                                            // Measure total pill width so we can center them.
                                            let pill_font = FontId::proportional(11.0);
                                            let pill_h_pad = 16.0_f32; // Margin::symmetric(8,3) → 8+8
                                            let pill_extra = 6.0_f32;  // frame border overhead
                                            let pill_spacing = 6.0_f32;
                                            let mut total_pills_w = 0.0_f32;
                                            for (text, _) in &pills {
                                                let tw = ui.fonts(|f| {
                                                    f.layout_no_wrap(
                                                        text.clone(),
                                                        pill_font.clone(),
                                                        Color32::WHITE,
                                                    )
                                                    .rect
                                                    .width()
                                                });
                                                total_pills_w += tw + pill_h_pad + pill_extra;
                                            }
                                            if pills.len() > 1 {
                                                total_pills_w +=
                                                    (pills.len() - 1) as f32 * pill_spacing;
                                            }
                                            let pill_avail = ui.available_width();
                                            let pill_pad =
                                                ((pill_avail - total_pills_w) / 2.0).max(0.0);

                                            ui.horizontal_wrapped(|ui| {
                                                ui.spacing_mut().item_spacing =
                                                    Vec2::new(pill_spacing, pill_spacing);
                                                if pill_pad > 1.0 {
                                                    ui.add_space(pill_pad);
                                                }
                                                for (text, color) in pills {
                                                    egui::Frame::new()
                                                        .fill(Color32::from_rgba_unmultiplied(
                                                            t::BG_RAISED.r(),
                                                            t::BG_RAISED.g(),
                                                            t::BG_RAISED.b(),
                                                            (alpha * 185.0) as u8,
                                                        ))
                                                        .corner_radius(egui::CornerRadius::same(9))
                                                        .inner_margin(egui::Margin::symmetric(8, 3))
                                                        .show(ui, |ui| {
                                                            ui.add(
                                                                egui::Label::new(
                                                                    egui::RichText::new(text)
                                                                        .font(
                                                                            FontId::proportional(
                                                                                11.0,
                                                                            ),
                                                                        )
                                                                        .color(a(color)),
                                                                )
                                                                .wrap_mode(
                                                                    egui::TextWrapMode::Extend,
                                                                ),
                                                            );
                                                        });
                                                }
                                            });

                                            if self.images_expected > 0 {
                                                let prog = (self.images_loaded as f32
                                                    / self.images_expected as f32)
                                                    .clamp(0.0, 1.0);
                                                ui.add_space(8.0);
                                                let bar_w = ui.available_width().min(details_w);
                                                ui.add(
                                                    egui::widgets::ProgressBar::new(prog)
                                                        .fill(a(t::ACCENT))
                                                        .desired_width(bar_w)
                                                        .text(format!(
                                                            "Image prefetch: {} / {}",
                                                            self.images_loaded,
                                                            self.images_expected
                                                        )),
                                                );
                                            }

                                            ui.add_space(10.0);
                                            ui.vertical_centered(|ui| {
                                                ui.set_min_width(details_w);
                                                ui.set_max_width(details_w);
                                                egui::Frame::new()
                                                    .fill(a(t::BG_BASE))
                                                    .stroke(egui::Stroke::new(
                                                        1.0,
                                                        a(t::BORDER_SUBTLE),
                                                    ))
                                                    .corner_radius(t::RADIUS_SM)
                                                    .inner_margin(egui::Margin::symmetric(8, 7))
                                                    .show(ui, |ui| {
                                                        let inner_w = (details_w - 18.0).max(0.0);
                                                        ui.set_min_width(inner_w);
                                                        ui.set_max_width(inner_w);
                                                        ui.set_min_height(
                                                            LOADING_LOG_ROWS as f32
                                                                * LOADING_LOG_ROW_HEIGHT,
                                                        );

                                                            // Use explicit vertical layout so log lines stack properly
                                                            // (parent vertical_centered would otherwise misalign them)
                                                            ui.with_layout(
                                                                egui::Layout::top_down(
                                                                    egui::Align::Min,
                                                                ),
                                                                |ui| {
                                                                    let mut lines: Vec<&LogLine> =
                                                                        self
                                                                            .log
                                                                            .iter()
                                                                            .rev()
                                                                            .take(
                                                                                LOADING_LOG_ROWS,
                                                                            )
                                                                            .collect();
                                                                    lines.reverse();
                                                                    if lines.is_empty() {
                                                                        ui.add_sized(
                                                                            Vec2::new(inner_w, 0.0),
                                                                            egui::Label::new(
                                                                                egui::RichText::new(
                                                                                    "Waiting for startup events…",
                                                                                )
                                                                                .font(t::small())
                                                                                .color(a(
                                                                                    t::TEXT_MUTED,
                                                                                )),
                                                                            )
                                                                            .truncate(),
                                                                        );
                                                                    } else {
                                                                        for line in lines {
                                                                            ui.add_sized(
                                                                                Vec2::new(
                                                                                    inner_w, 0.0,
                                                                                ),
                                                                                egui::Label::new(
                                                                                    egui::RichText::new(
                                                                                        &line.text,
                                                                                    )
                                                                                    .font(t::small())
                                                                                    .color(a(
                                                                                        line.color,
                                                                                    )),
                                                                                )
                                                                                .truncate(),
                                                                            );
                                                                        }
                                                                    }
                                                                },
                                                            );
                                                        });
                                            });
                                });
                            });
                    });
            });
    }

    fn show_super_narrow(&mut self, ctx: &Context, rect: egui::Rect, alpha: f32) {
        let a = |c: Color32| {
            Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0) as u8)
        };

        let card_w = (rect.width() - 12.0).clamp(120.0, SUPER_NARROW_WIDTH);
        egui::Area::new(egui::Id::new("loading_narrow_card"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_width(card_w);
                egui::Frame::new()
                    .fill(a(t::BG_SURFACE))
                    .stroke(egui::Stroke::new(1.0, a(t::BORDER_SUBTLE)))
                    .corner_radius(t::RADIUS)
                    .inner_margin(egui::Margin::symmetric(10, 10))
                    .show(ui, |ui| {
                        ui.set_width(card_w - 20.0);

                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("crust")
                                    .font(FontId::proportional(30.0))
                                    .color(a(t::ACCENT)),
                            );
                            if card_w > 220.0 {
                                ui.label(
                                    egui::RichText::new("Twitch chat client")
                                        .font(FontId::proportional(11.0))
                                        .color(a(t::TEXT_MUTED)),
                                );
                            }
                        });

                        ui.add_space(6.0);
                        ui.vertical_centered(|ui| {
                            let (spin_rect, _) =
                                ui.allocate_exact_size(Vec2::new(52.0, 52.0), egui::Sense::hover());
                            let center = spin_rect.center();
                            let spin_painter = ui.painter();
                            if self.phase != Phase::Ready {
                                let t_val = ui.input(|i| i.time) as f32;
                                let radius = 16.0_f32;
                                let segments = 10usize;
                                for i in 0..segments {
                                    let angle =
                                        (i as f32 / segments as f32) * std::f32::consts::TAU;
                                    let spin = (t_val * 1.8).rem_euclid(1.0);
                                    let dot_age =
                                        ((i as f32 / segments as f32) - spin).rem_euclid(1.0);
                                    let dot_alpha = ((1.0 - dot_age) * alpha * 220.0) as u8;
                                    let p = Pos2::new(
                                        center.x + radius * angle.cos(),
                                        center.y + radius * angle.sin(),
                                    );
                                    spin_painter.circle_filled(
                                        p,
                                        2.5,
                                        Color32::from_rgba_unmultiplied(
                                            t::ACCENT.r(),
                                            t::ACCENT.g(),
                                            t::ACCENT.b(),
                                            dot_alpha,
                                        ),
                                    );
                                }
                                ctx.request_repaint_after(Duration::from_millis(16));
                            } else {
                                spin_painter.text(
                                    center,
                                    Align2::CENTER_CENTER,
                                    "✓",
                                    FontId::proportional(24.0),
                                    a(t::GREEN),
                                );
                            }
                        });

                        let stage = self.stage_label();
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(stage.0)
                                    .font(t::small())
                                    .color(a(stage.1)),
                            );
                        });

                        ui.add_space(8.0);
                        let conn = self.connection_status();
                        ui.label(
                            egui::RichText::new(format!("Connection: {}", conn.0))
                                .font(t::small())
                                .color(a(conn.1)),
                        );

                        if !self.channels_joined.is_empty() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Channels joined: {}",
                                    self.channels_joined.len()
                                ))
                                .font(t::small())
                                .color(a(t::TEXT_SECONDARY)),
                            );
                        }

                        if self.catalog_loaded {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Global emotes: {}",
                                    self.catalog_count
                                ))
                                .font(t::small())
                                .color(a(t::ACCENT)),
                            );
                        } else if self.phase == Phase::Loading {
                            ui.label(
                                egui::RichText::new("Global emotes: loading…")
                                    .font(t::small())
                                    .color(a(t::TEXT_MUTED)),
                            );
                        }

                        if self.images_expected > 0 {
                            let prog =
                                (self.images_loaded as f32 / self.images_expected as f32)
                                    .clamp(0.0, 1.0);
                            ui.add_space(6.0);
                            ui.add(
                                egui::widgets::ProgressBar::new(prog)
                                    .fill(a(t::ACCENT))
                                    .desired_width(ui.available_width())
                                    .text(format!(
                                        "Images: {} / {}",
                                        self.images_loaded, self.images_expected
                                    )),
                            );
                        }

                        ui.add_space(8.0);
                        egui::Frame::new()
                            .fill(a(t::BG_BASE))
                            .stroke(egui::Stroke::new(1.0, a(t::BORDER_SUBTLE)))
                            .corner_radius(t::RADIUS_SM)
                            .inner_margin(egui::Margin::symmetric(7, 6))
                            .show(ui, |ui| {
                                let inner_w = ui.available_width();
                                ui.set_min_height(
                                    SUPER_NARROW_LOG_ROWS as f32 * LOADING_LOG_ROW_HEIGHT,
                                );
                                let mut lines: Vec<&LogLine> = self
                                    .log
                                    .iter()
                                    .rev()
                                    .take(SUPER_NARROW_LOG_ROWS)
                                    .collect();
                                lines.reverse();

                                if lines.is_empty() {
                                    ui.add_sized(
                                        Vec2::new(inner_w, 0.0),
                                        egui::Label::new(
                                            egui::RichText::new("Waiting for startup events…")
                                                .font(t::small())
                                                .color(a(t::TEXT_MUTED)),
                                        )
                                        .truncate(),
                                    );
                                } else {
                                    for line in lines {
                                        ui.add_sized(
                                            Vec2::new(inner_w, 0.0),
                                            egui::Label::new(
                                                egui::RichText::new(&line.text)
                                                    .font(t::small())
                                                    .color(a(line.color)),
                                            )
                                            .truncate(),
                                        );
                                    }
                                }
                            });
                    });
            });
    }

    fn stage_label(&self) -> (&'static str, Color32) {
        match self.phase {
            Phase::Connecting => ("Connecting to Twitch…", t::YELLOW),
            Phase::Authenticating => ("Authenticating…", t::YELLOW),
            Phase::Loading => ("Loading startup data…", t::TEXT_SECONDARY),
            Phase::Ready => ("Ready", t::GREEN),
            Phase::Done => ("Done", t::GREEN),
        }
    }

    fn connection_status(&self) -> (&'static str, Color32) {
        match self.phase {
            Phase::Connecting => ("Connecting", t::YELLOW),
            Phase::Authenticating => ("Authenticating", t::YELLOW),
            _ => ("Connected", t::GREEN),
        }
    }

    // private

    /// Transition to the Loading phase, recording when we entered it.
    fn enter_loading(&mut self) {
        if matches!(self.phase, Phase::Connecting | Phase::Authenticating) {
            self.phase = Phase::Loading;
            self.loading_entered_at = Some(Instant::now());
        }
    }

    fn maybe_enter_anonymous_loading(&mut self) {
        if matches!(self.phase, Phase::Connecting | Phase::Authenticating)
            && self.authenticated_user.is_none()
        {
            self.auth_optional = true;
            self.push_log("Connected anonymously", t::TEXT_MUTED);
            self.enter_loading();
        }
    }

    fn push_log(&mut self, text: impl Into<String>, color: Color32) {
        self.log.push_back(LogLine {
            text: text.into(),
            color,
        });
        while self.log.len() > 40 {
            self.log.pop_front();
        }
    }

    fn check_done(&mut self) {
        if self.phase != Phase::Loading {
            return;
        }
        // Don't declare done too quickly - give channel-join events time to
        // arrive so we don't dismiss before any channel data is loaded.
        if let Some(entered) = self.loading_entered_at {
            if entered.elapsed() < MIN_LOADING_GRACE {
                return;
            }
        }
        let auth_ok = self.authenticated_user.is_some() || self.auth_optional;
        let catalog_ok = self.catalog_loaded;
        // Non-Twitch channels don't currently emit full history/emote-load
        // signals. Don't block startup on those platform-specific side paths.
        let startup_channels: Vec<&String> = self
            .channels_joined
            .iter()
            .filter(|ch| is_blocking_twitch_startup_channel(ch))
            .collect();
        let history_ok = startup_channels
            .iter()
            .all(|ch| self.channels_with_history.contains(*ch));
        let emotes_ok = startup_channels
            .iter()
            .all(|ch| self.channels_with_emotes.contains(*ch));
        // Images are prefetched in the background - don't block the loading
        // screen on them.  They'll fill in shortly after the overlay fades.
        if auth_ok && catalog_ok && history_ok && emotes_ok {
            self.push_log(
                format!(
                    "Ready! ({}/{} images)",
                    self.images_loaded, self.images_expected
                ),
                t::GREEN,
            );
            self.mark_ready();
        }
    }

    fn mark_ready(&mut self) {
        if self.phase == Phase::Loading
            || self.phase == Phase::Connecting
            || self.phase == Phase::Authenticating
        {
            self.phase = Phase::Ready;
            self.ready_at = Some(Instant::now());
        }
    }
}

fn is_blocking_twitch_startup_channel(raw: &str) -> bool {
    if raw.starts_with("kick:") || raw.starts_with("irc:") {
        return false;
    }
    let login = raw.trim_start_matches('#');
    let len = login.len();
    if !(3..=25).contains(&len) {
        return false;
    }
    login
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
