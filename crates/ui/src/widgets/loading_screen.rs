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

/// When startup settings include Twitch auto-join channels, wait up to this
/// long for join events before allowing completion without them.
const STARTUP_JOIN_DISCOVERY_GRACE: Duration = Duration::from_secs(12);

/// After this long from join, treat missing history/emote events as settled
/// so startup doesn't hang forever on channels that never produce ROOMSTATE.
const CHANNEL_DATA_SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

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
    StartupChannelsConfigured {
        channels: Vec<String>,
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
    channel_joined_at: std::collections::HashMap<String, Instant>,
    configured_startup_twitch_channels: HashSet<String>,
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
            channel_joined_at: std::collections::HashMap::new(),
            configured_startup_twitch_channels: HashSet::new(),
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
                self.push_log("Connecting to Twitch...", t::text_secondary());
            }
            LoadEvent::Connected => {
                // Don't regress from Loading if an optimistic Authenticated
                // event already advanced us.
                if matches!(self.phase, Phase::Connecting | Phase::Authenticating) {
                    self.phase = Phase::Authenticating;
                }
                self.push_log("Connected - authenticating...", t::green());
            }
            LoadEvent::Authenticated { username } => {
                self.push_log(format!("Authenticated as {username}"), t::green());
                self.authenticated_user = Some(username);
                self.auth_optional = false;
                self.enter_loading();
            }
            LoadEvent::ChannelJoined { channel } => {
                let channel = normalize_channel_key(&channel);
                self.maybe_enter_anonymous_loading();
                self.push_log(format!("Joined #{channel}"), t::text_primary());
                if !self.channels_joined.contains(&channel) {
                    self.channels_joined.push(channel.clone());
                }
                self.channel_joined_at
                    .entry(channel.clone())
                    .or_insert_with(Instant::now);
                self.check_done();
            }
            LoadEvent::StartupChannelsConfigured { channels } => {
                self.configured_startup_twitch_channels = channels
                    .into_iter()
                    .filter_map(|raw| normalize_blocking_twitch_startup_channel(&raw))
                    .collect();
                self.check_done();
            }
            LoadEvent::CatalogLoaded { count } => {
                self.maybe_enter_anonymous_loading();
                self.push_log(format!("Global emotes ready - {count} emotes"), t::accent());
                self.catalog_count = count;
                self.catalog_loaded = true;
                self.check_done();
            }
            LoadEvent::HistoryLoaded { channel, count } => {
                let channel = normalize_channel_key(&channel);
                self.maybe_enter_anonymous_loading();
                self.push_log(
                    format!("#{channel}: {count} history messages"),
                    t::text_secondary(),
                );
                self.channels_with_history.insert(channel);
                self.check_done();
            }
            LoadEvent::EmoteImageReady => {
                self.maybe_enter_anonymous_loading();
                // Not all EmoteImageReady events come from prefetch queues
                // (avatars/link previews/plugins can emit them too). Clamp to
                // the queued total so counters don't run past expected.
                if self.images_loaded < self.images_expected {
                    self.images_loaded += 1;
                }
                // Re-check done on every image arrival.
                self.check_done();
            }
            LoadEvent::ImagePrefetchQueued { count } => {
                self.maybe_enter_anonymous_loading();
                self.images_expected += count;
                // Don't log - too noisy.
            }
            LoadEvent::ChannelEmotesLoaded { channel, count } => {
                let channel = normalize_channel_key(&channel);
                self.maybe_enter_anonymous_loading();
                if count > 0 {
                    self.push_log(
                        format!("#{channel}: {count} channel emotes"),
                        t::text_secondary(),
                    );
                } else {
                    self.push_log(format!("#{channel}: no channel emotes"), t::text_muted());
                }
                self.channel_emote_totals.insert(channel.clone(), count);
                self.channels_with_emotes.insert(channel);
                self.check_done();
            }
        }
    }

    /// Call every frame; advances the timeout-based done detection.
    pub fn tick(&mut self) {
        if matches!(
            self.phase,
            Phase::Connecting | Phase::Authenticating | Phase::Loading
        ) && self.started_at.elapsed() >= DONE_TIMEOUT
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
            .frame(egui::Frame::new().fill(t::bg_base()))
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
                        t::accent().r(),
                        t::accent().g(),
                        t::accent().b(),
                        (alpha * 20.0) as u8,
                    ),
                );
                painter.circle_filled(
                    Pos2::new(rect.right() - 80.0, rect.bottom() - 60.0),
                    140.0,
                    Color32::from_rgba_unmultiplied(
                        t::border_accent().r(),
                        t::border_accent().g(),
                        t::border_accent().b(),
                        (alpha * 18.0) as u8,
                    ),
                );

                if rect.width() <= SUPER_NARROW_WIDTH {
                    self.show_super_narrow(ctx, rect, alpha);
                    return;
                }

                let card_w = (rect.width() - 36.0).clamp(200.0, 760.0);
                let is_compact = card_w < 400.0;
                let spinner_box = if is_compact { 48.0 } else { 64.0 };
                let spinner_r = if is_compact { 15.0_f32 } else { 20.0_f32 };
                let spinner_dot = if is_compact { 2.3_f32 } else { 2.7_f32 };
                // Approximate fixed overhead (title, spinner, pills, margins,
                // spacing) so we can give remaining height to the log.
                let overhead = if is_compact { 290.0 } else { 330.0 };
                let log_rows = ((rect.height() - overhead) / LOADING_LOG_ROW_HEIGHT)
                    .clamp(3.0, LOADING_LOG_ROWS as f32) as usize;
                egui::Area::new(egui::Id::new("loading_center_card"))
                    .order(egui::Order::Foreground)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .interactable(false)
                    .show(ctx, |ui| {
                        ui.set_width(card_w);
                        egui::Frame::new()
                            .fill(a(t::bg_surface()))
                            .stroke(egui::Stroke::new(1.0, a(t::border_subtle())))
                            .corner_radius(t::RADIUS)
                            .inner_margin(egui::Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(card_w - 32.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        egui::RichText::new("crust")
                                            .font(FontId::proportional(if is_compact { 30.0 } else { 40.0 }))
                                            .color(a(t::accent())),
                                    );
                                    if !is_compact {
                                        ui.label(
                                            egui::RichText::new(
                                                "Twitch chat client (Kick/IRC optional beta)",
                                            )
                                            .font(FontId::proportional(12.0))
                                            .color(a(t::text_muted())),
                                        );
                                    }
                                });

                                ui.add_space(if is_compact { 6.0 } else { 10.0 });

                                ui.vertical_centered(|ui| {
                                    let (spin_rect, _) = ui.allocate_exact_size(
                                        Vec2::new(spinner_box, spinner_box),
                                        egui::Sense::hover(),
                                    );
                                    let center = spin_rect.center();
                                    let spin_painter = ui.painter();
                                    if self.phase != Phase::Ready {
                                        let t_val = ui.input(|i| i.time) as f32;
                                        let radius = spinner_r;
                                        let segments = if is_compact { 10usize } else { 12usize };
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
                                                spinner_dot,
                                                Color32::from_rgba_unmultiplied(
                                                    t::accent().r(),
                                                    t::accent().g(),
                                                    t::accent().b(),
                                                    dot_alpha,
                                                ),
                                            );
                                        }
                                        ctx.request_repaint_after(Duration::from_millis(16));
                                    } else {
                                        spin_painter.text(
                                            center,
                                            Align2::CENTER_CENTER,
                                            "",
                                            FontId::proportional(28.0),
                                            a(t::green()),
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

                                ui.add_space(if is_compact { 6.0 } else { 10.0 });
                                let details_w = (card_w - 32.0).clamp(160.0, 560.0);
                                ui.vertical_centered(|ui| {
                                    ui.set_min_width(details_w);
                                    ui.set_max_width(details_w);
                                    let mut pills: Vec<(String, Color32)> = Vec::new();
                                    let conn = self.connection_status();
                                    pills.push((format!("● {}", conn.0), conn.1));

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
                                        pills.push((text, t::text_secondary()));
                                    }

                                    if self.catalog_loaded {
                                        pills.push((
                                            format!("{} global emotes", self.catalog_count),
                                            t::accent(),
                                        ));
                                    } else if self.phase == Phase::Loading {
                                        pills.push((
                                            "Loading global emotes...".to_owned(),
                                            t::text_muted(),
                                        ));
                                    }

                                    if self.images_expected > 0 {
                                        let pct = ((self.images_loaded as f32
                                            / self.images_expected as f32)
                                            * 100.0)
                                            .min(100.0)
                                            as usize;
                                        let c = if pct >= 95 {
                                            t::green()
                                        } else {
                                            t::text_secondary()
                                        };
                                        pills.push((
                                            format!(
                                                "{} / {} images prefetched ({pct}%)",
                                                self.images_loaded, self.images_expected
                                            ),
                                            c,
                                        ));
                                    } else if self.images_loaded > 0 {
                                        pills.push((
                                            format!("{} images", self.images_loaded),
                                            t::text_secondary(),
                                        ));
                                    }

                                    let pill_spacing = 6.0_f32;
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing =
                                            Vec2::new(pill_spacing, pill_spacing);
                                        for (text, color) in pills {
                                            egui::Frame::new()
                                                .fill(Color32::from_rgba_unmultiplied(
                                                    t::bg_raised().r(),
                                                    t::bg_raised().g(),
                                                    t::bg_raised().b(),
                                                    (alpha * 185.0) as u8,
                                                ))
                                                .corner_radius(egui::CornerRadius::same(9))
                                                .inner_margin(egui::Margin::symmetric(8, 3))
                                                .show(ui, |ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(text)
                                                                .font(
                                                                    FontId::proportional(11.0),
                                                                )
                                                                .color(a(color)),
                                                        )
                                                        .wrap_mode(
                                                            egui::TextWrapMode::Truncate,
                                                        ),
                                                    );
                                                });
                                        }
                                    });

                                    let startup_prog = self.startup_progress();
                                    ui.add_space(8.0);
                                    let bar_w = ui.available_width().min(details_w);
                                    ui.add(
                                        egui::widgets::ProgressBar::new(startup_prog)
                                            .fill(a(t::accent()))
                                            .desired_width(bar_w)
                                            .text(format!(
                                                "Startup progress: {}%",
                                                (startup_prog * 100.0).round() as usize
                                            )),
                                    );

                                    if self.images_expected > 0 {
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "Background image prefetch: {} / {}",
                                                self.images_loaded, self.images_expected
                                            ))
                                            .font(t::tiny())
                                            .color(a(t::text_muted())),
                                        );
                                    }

                                    ui.add_space(10.0);
                                    ui.vertical_centered(|ui| {
                                        ui.set_min_width(details_w);
                                        ui.set_max_width(details_w);
                                        egui::Frame::new()
                                            .fill(a(t::bg_base()))
                                            .stroke(egui::Stroke::new(
                                                1.0,
                                                a(t::border_subtle()),
                                            ))
                                            .corner_radius(t::RADIUS_SM)
                                            .inner_margin(egui::Margin::symmetric(8, 7))
                                            .show(ui, |ui| {
                                                let inner_w = (details_w - 18.0).max(0.0);
                                                ui.set_min_width(inner_w);
                                                ui.set_max_width(inner_w);
                                                ui.set_min_height(
                                                    log_rows as f32
                                                        * LOADING_LOG_ROW_HEIGHT,
                                                );

                                                // Use explicit vertical layout so log lines stack
                                                // properly (parent vertical_centered would
                                                // otherwise misalign them).
                                                ui.with_layout(
                                                    egui::Layout::top_down(egui::Align::Min),
                                                    |ui| {
                                                        let mut lines: Vec<&LogLine> = self
                                                            .log
                                                            .iter()
                                                            .rev()
                                                            .take(log_rows)
                                                            .collect();
                                                        lines.reverse();
                                                        if lines.is_empty() {
                                                            ui.add_sized(
                                                                Vec2::new(inner_w, 0.0),
                                                                egui::Label::new(
                                                                    egui::RichText::new(
                                                                        "Waiting for startup events...",
                                                                    )
                                                                    .font(t::small())
                                                                    .color(a(t::text_muted())),
                                                                )
                                                                .truncate(),
                                                            );
                                                        } else {
                                                            for line in lines {
                                                                ui.add_sized(
                                                                    Vec2::new(inner_w, 0.0),
                                                                    egui::Label::new(
                                                                        egui::RichText::new(
                                                                            &line.text,
                                                                        )
                                                                        .font(t::small())
                                                                        .color(a(line.color)),
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
                    .fill(a(t::bg_surface()))
                    .stroke(egui::Stroke::new(1.0, a(t::border_subtle())))
                    .corner_radius(t::RADIUS)
                    .inner_margin(egui::Margin::symmetric(10, 10))
                    .show(ui, |ui| {
                        ui.set_width(card_w - 20.0);

                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("crust")
                                    .font(FontId::proportional(30.0))
                                    .color(a(t::accent())),
                            );
                            if card_w > 220.0 {
                                ui.label(
                                    egui::RichText::new("Twitch chat client")
                                        .font(FontId::proportional(11.0))
                                        .color(a(t::text_muted())),
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
                                            t::accent().r(),
                                            t::accent().g(),
                                            t::accent().b(),
                                            dot_alpha,
                                        ),
                                    );
                                }
                                ctx.request_repaint_after(Duration::from_millis(16));
                            } else {
                                spin_painter.text(
                                    center,
                                    Align2::CENTER_CENTER,
                                    "",
                                    FontId::proportional(24.0),
                                    a(t::green()),
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
                                .color(a(t::text_secondary())),
                            );
                        }

                        if self.catalog_loaded {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Global emotes: {}",
                                    self.catalog_count
                                ))
                                .font(t::small())
                                .color(a(t::accent())),
                            );
                        } else if self.phase == Phase::Loading {
                            ui.label(
                                egui::RichText::new("Global emotes: loading...")
                                    .font(t::small())
                                    .color(a(t::text_muted())),
                            );
                        }

                        let startup_prog = self.startup_progress();
                        ui.add_space(6.0);
                        ui.add(
                            egui::widgets::ProgressBar::new(startup_prog)
                                .fill(a(t::accent()))
                                .desired_width(ui.available_width())
                                .text(format!(
                                    "Startup: {}%",
                                    (startup_prog * 100.0).round() as usize
                                )),
                        );

                        if self.images_expected > 0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Background images: {} / {}",
                                    self.images_loaded, self.images_expected
                                ))
                                .font(t::tiny())
                                .color(a(t::text_muted())),
                            );
                        }

                        ui.add_space(8.0);
                        let narrow_log_rows = ((rect.height() - 280.0) / LOADING_LOG_ROW_HEIGHT)
                            .clamp(2.0, SUPER_NARROW_LOG_ROWS as f32)
                            as usize;
                        egui::Frame::new()
                            .fill(a(t::bg_base()))
                            .stroke(egui::Stroke::new(1.0, a(t::border_subtle())))
                            .corner_radius(t::RADIUS_SM)
                            .inner_margin(egui::Margin::symmetric(7, 6))
                            .show(ui, |ui| {
                                let inner_w = ui.available_width();
                                ui.set_min_height(narrow_log_rows as f32 * LOADING_LOG_ROW_HEIGHT);
                                let mut lines: Vec<&LogLine> =
                                    self.log.iter().rev().take(narrow_log_rows).collect();
                                lines.reverse();

                                if lines.is_empty() {
                                    ui.add_sized(
                                        Vec2::new(inner_w, 0.0),
                                        egui::Label::new(
                                            egui::RichText::new("Waiting for startup events...")
                                                .font(t::small())
                                                .color(a(t::text_muted())),
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
            Phase::Connecting => ("Connecting to Twitch...", t::yellow()),
            Phase::Authenticating => ("Authenticating...", t::yellow()),
            Phase::Loading => ("Loading startup data...", t::text_secondary()),
            Phase::Ready => ("Ready", t::green()),
            Phase::Done => ("Done", t::green()),
        }
    }

    fn connection_status(&self) -> (&'static str, Color32) {
        match self.phase {
            Phase::Connecting => ("Connecting", t::yellow()),
            Phase::Authenticating => ("Authenticating", t::yellow()),
            _ => ("Connected", t::green()),
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
            self.push_log("Connected anonymously", t::text_muted());
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

        let startup_channels: Vec<&String> = self
            .channels_joined
            .iter()
            .filter(|ch| is_blocking_twitch_startup_channel(ch))
            .collect();

        let configured_startup_count = self.configured_startup_twitch_channels.len();
        if configured_startup_count > 0 {
            let joined_configured_count = startup_channels
                .iter()
                .filter(|ch| {
                    self.configured_startup_twitch_channels
                        .contains(ch.as_str())
                })
                .count();
            if joined_configured_count < configured_startup_count {
                if let Some(entered) = self.loading_entered_at {
                    if entered.elapsed() < STARTUP_JOIN_DISCOVERY_GRACE {
                        return;
                    }
                }
            }
        }

        let auth_ok = self.authenticated_user.is_some() || self.auth_optional;
        let catalog_ok = self.catalog_loaded;
        // Non-Twitch channels don't currently emit full history/emote-load
        // signals. Don't block startup on those platform-specific side paths.
        let tracked_startup_channels: Vec<&String> =
            if self.configured_startup_twitch_channels.is_empty() {
                startup_channels
            } else {
                startup_channels
                    .into_iter()
                    .filter(|ch| {
                        self.configured_startup_twitch_channels
                            .contains(ch.as_str())
                    })
                    .collect()
            };
        let history_ok = tracked_startup_channels
            .iter()
            .all(|ch| self.channel_history_settled(ch));
        let emotes_ok = tracked_startup_channels
            .iter()
            .all(|ch| self.channel_emotes_settled(ch));
        // Images are prefetched in the background - don't block the loading
        // screen on them.  They'll fill in shortly after the overlay fades.
        if auth_ok && catalog_ok && history_ok && emotes_ok {
            self.push_log(
                format!(
                    "Ready! ({}/{} images)",
                    self.images_loaded, self.images_expected
                ),
                t::green(),
            );
            self.mark_ready();
        }
    }

    fn startup_progress(&self) -> f32 {
        if matches!(self.phase, Phase::Ready | Phase::Done) {
            return 1.0;
        }

        let auth_ok = self.authenticated_user.is_some() || self.auth_optional;
        let observed_blocking_channels: Vec<&String> = self
            .channels_joined
            .iter()
            .filter(|ch| is_blocking_twitch_startup_channel(ch))
            .collect();

        let configured_count = self.configured_startup_twitch_channels.len();
        let join_target = if configured_count > 0 {
            configured_count
        } else {
            observed_blocking_channels.len()
        };

        let joined_count = if configured_count > 0 {
            observed_blocking_channels
                .iter()
                .filter(|ch| {
                    self.configured_startup_twitch_channels
                        .contains(ch.as_str())
                })
                .count()
        } else {
            observed_blocking_channels.len()
        };

        let tracked_channels: Vec<&String> = if configured_count > 0 {
            observed_blocking_channels
                .into_iter()
                .filter(|ch| {
                    self.configured_startup_twitch_channels
                        .contains(ch.as_str())
                })
                .collect()
        } else {
            observed_blocking_channels
        };

        let history_count = tracked_channels
            .iter()
            .filter(|ch| self.channel_history_settled(ch))
            .count();
        let emotes_count = tracked_channels
            .iter()
            .filter(|ch| self.channel_emotes_settled(ch))
            .count();

        let total_units = 2 + (join_target * 3);
        let mut done_units = 0usize;
        if auth_ok {
            done_units += 1;
        }
        if self.catalog_loaded {
            done_units += 1;
        }
        done_units += joined_count.min(join_target);
        done_units += history_count.min(join_target);
        done_units += emotes_count.min(join_target);

        if total_units == 0 {
            return 1.0;
        }

        let mut progress = (done_units as f32 / total_units as f32).clamp(0.0, 1.0);
        if self.phase == Phase::Loading && progress >= 1.0 {
            progress = 0.99;
        }
        progress
    }

    fn channel_history_settled(&self, channel: &str) -> bool {
        self.channels_with_history.contains(channel) || self.channel_data_timed_out(channel)
    }

    fn channel_emotes_settled(&self, channel: &str) -> bool {
        self.channels_with_emotes.contains(channel) || self.channel_data_timed_out(channel)
    }

    fn channel_data_timed_out(&self, channel: &str) -> bool {
        self.channel_joined_at
            .get(channel)
            .map(|joined_at| joined_at.elapsed() >= CHANNEL_DATA_SETTLE_TIMEOUT)
            .unwrap_or(false)
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

fn normalize_channel_key(raw: &str) -> String {
    raw.trim().trim_start_matches('#').to_ascii_lowercase()
}

fn normalize_blocking_twitch_startup_channel(raw: &str) -> Option<String> {
    if raw.starts_with("kick:") || raw.starts_with("irc:") {
        return None;
    }
    let login = normalize_channel_key(raw);
    let len = login.len();
    if !(3..=25).contains(&len) {
        return None;
    }
    if login
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        Some(login)
    } else {
        None
    }
}

fn is_blocking_twitch_startup_channel(raw: &str) -> bool {
    normalize_blocking_twitch_startup_channel(raw).is_some()
}
