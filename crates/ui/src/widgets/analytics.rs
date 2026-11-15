use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use egui::{Frame, RichText, ScrollArea, Ui};
use serde::{Deserialize, Serialize};

use crust_core::model::{ChannelId, ChannelState, MsgKind, Span};

use crate::theme as t;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const ACTIVITY_BUCKETS: usize = 30;

// Time window

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TimeWindow {
    Last5Min,
    Last15Min,
    Last30Min,
    #[default]
    All,
}

impl TimeWindow {
    fn label(self) -> &'static str {
        match self {
            TimeWindow::Last5Min => "5 min",
            TimeWindow::Last15Min => "15 min",
            TimeWindow::Last30Min => "30 min",
            TimeWindow::All => "All",
        }
    }

    fn minutes(self) -> Option<i64> {
        match self {
            TimeWindow::Last5Min => Some(5),
            TimeWindow::Last15Min => Some(15),
            TimeWindow::Last30Min => Some(30),
            TimeWindow::All => None,
        }
    }
}

// Tab

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Overview,
    Chatters,
    Emotes,
    Activity,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Chatters => "Chatters",
            Tab::Emotes => "Emotes",
            Tab::Activity => "Activity",
        }
    }
}

// Stats

#[derive(Serialize, Deserialize)]
struct ChannelStats {
    channel: ChannelId,
    computed_at: DateTime<Utc>,
    window_label: String,
    total_msgs: u32,
    unique_chatters: usize,
    msgs_per_min: Option<f32>,
    avg_msg_len: f32,
    emote_msg_pct: f32,
    top_chatters: Vec<(String, u32)>,
    top_emotes: Vec<(String, u32)>,
    total_bits: u64,
    top_bit_donors: Vec<(String, u64)>,
    sub_count: u32,
    raid_count: u32,
    first_msg_time: Option<DateTime<Utc>>,
    peak_period_label: String,
    activity_buckets: Vec<u32>,
}

fn compute_stats(
    channel: &ChannelState,
    window: TimeWindow,
    wipe_time: Option<DateTime<Utc>>,
) -> ChannelStats {
    let now = Utc::now();
    let window_cutoff = window.minutes().map(|m| now - chrono::Duration::minutes(m));
    let cutoff = match (window_cutoff, wipe_time) {
        (Some(w), Some(wt)) => Some(w.max(wt)),
        (Some(w), None) => Some(w),
        (None, Some(wt)) => Some(wt),
        (None, None) => None,
    };
    let analysis_start = cutoff
        .unwrap_or_else(|| channel.messages.front().map(|m| m.timestamp).unwrap_or(now))
        .min(now);
    let analysis_span_secs = (now - analysis_start).num_seconds().max(1);
    let bucket_span_secs =
        ((analysis_span_secs + ACTIVITY_BUCKETS as i64 - 1) / ACTIVITY_BUCKETS as i64).max(1);

    let mut activity_buckets = vec![0u32; ACTIVITY_BUCKETS];
    let mut total_msgs: u32 = 0;
    let mut text_metric_msgs: u32 = 0;
    let mut total_msg_len: u64 = 0;
    let mut msgs_with_emotes: u32 = 0;
    let mut chatter_counts: HashMap<String, u32> = HashMap::new();
    let mut emotes: HashMap<String, u32> = HashMap::new();
    let mut bit_donors: HashMap<String, u64> = HashMap::new();
    let mut total_bits: u64 = 0;
    let mut sub_count: u32 = 0;
    let mut raid_count: u32 = 0;
    let mut first_msg_time: Option<DateTime<Utc>> = None;

    for msg in &channel.messages {
        if msg.timestamp < analysis_start || msg.flags.is_deleted {
            continue;
        }

        let bits_amount = match &msg.msg_kind {
            MsgKind::Chat => None,
            MsgKind::Bits { amount } => Some(*amount as u64),
            MsgKind::Sub { .. } => {
                sub_count = sub_count.saturating_add(1);
                continue;
            }
            MsgKind::Raid { .. } => {
                raid_count = raid_count.saturating_add(1);
                continue;
            }
            _ => continue,
        };

        total_msgs = total_msgs.saturating_add(1);
        text_metric_msgs = text_metric_msgs.saturating_add(1);
        total_msg_len = total_msg_len.saturating_add(msg.raw_text.chars().count() as u64);

        let login_key = msg.sender.login.to_lowercase();
        *chatter_counts.entry(login_key.clone()).or_insert(0) += 1;
        if let Some(bits) = bits_amount {
            *bit_donors.entry(login_key).or_insert(0) += bits;
            total_bits = total_bits.saturating_add(bits);
        }

        let mut line_has_emote = false;
        for span in &msg.spans {
            if let Span::Emote { code, .. } = span {
                *emotes.entry(code.clone()).or_insert(0) += 1;
                line_has_emote = true;
            }
        }
        // History lines can carry Twitch emote positions without parsed spans.
        if !line_has_emote && !msg.twitch_emotes.is_empty() {
            for te in &msg.twitch_emotes {
                let code = extract_twitch_emote_code(&msg.raw_text, te.start, te.end)
                    .unwrap_or_else(|| format!("emote:{}", te.id));
                *emotes.entry(code).or_insert(0) += 1;
            }
            line_has_emote = true;
        }
        if line_has_emote {
            msgs_with_emotes = msgs_with_emotes.saturating_add(1);
        }

        let bucket_idx = bucket_index_for(msg.timestamp, analysis_start, bucket_span_secs);
        activity_buckets[bucket_idx] = activity_buckets[bucket_idx].saturating_add(1);

        if first_msg_time.map_or(true, |t| msg.timestamp < t) {
            first_msg_time = Some(msg.timestamp);
        }
    }

    let peak_idx = activity_buckets
        .iter()
        .enumerate()
        .max_by_key(|(_, &v)| v)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let peak_period_label = {
        let bucket_start = peak_idx as i64 * bucket_span_secs;
        let bucket_end = ((peak_idx as i64 + 1) * bucket_span_secs).min(analysis_span_secs);
        let older = (analysis_span_secs - bucket_start).max(0);
        let newer = (analysis_span_secs - bucket_end).max(0);
        fmt_relative_secs(older, newer)
    };

    let mut top_chatters: Vec<(String, u32)> = chatter_counts.into_iter().collect();
    top_chatters.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let unique_chatters = top_chatters.len();

    let mut top_emotes: Vec<(String, u32)> = emotes.into_iter().collect();
    top_emotes.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut top_bit_donors: Vec<(String, u64)> = bit_donors.into_iter().collect();
    top_bit_donors.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top_bit_donors.truncate(25);

    let avg_msg_len = if text_metric_msgs > 0 {
        total_msg_len as f32 / text_metric_msgs as f32
    } else {
        0.0
    };
    let emote_msg_pct = if text_metric_msgs > 0 {
        msgs_with_emotes as f32 / text_metric_msgs as f32 * 100.0
    } else {
        0.0
    };

    let msgs_per_min = if total_msgs > 0 {
        Some(total_msgs as f32 * 60.0 / analysis_span_secs as f32)
    } else {
        None
    };
    let window_label = window
        .minutes()
        .map(|m| format!("last {m} min"))
        .unwrap_or_else(|| "all time".to_owned());

    ChannelStats {
        channel: channel.id.clone(),
        computed_at: now,
        window_label,
        total_msgs,
        unique_chatters,
        msgs_per_min,
        avg_msg_len,
        emote_msg_pct,
        top_chatters,
        top_emotes,
        total_bits,
        top_bit_donors,
        sub_count,
        raid_count,
        first_msg_time,
        peak_period_label,
        activity_buckets,
    }
}

fn fmt_relative_secs(older: i64, newer: i64) -> String {
    let f = |s: i64| {
        if s < 60 {
            format!("{s}s ago")
        } else {
            format!("{}m ago", s / 60)
        }
    };
    format!("{} - {}", f(older), f(newer.max(0)))
}

fn bucket_index_for(
    timestamp: DateTime<Utc>,
    start: DateTime<Utc>,
    bucket_span_secs: i64,
) -> usize {
    let elapsed = (timestamp - start).num_seconds().max(0);
    ((elapsed / bucket_span_secs) as usize).min(ACTIVITY_BUCKETS - 1)
}

fn extract_twitch_emote_code(raw_text: &str, start: usize, end_inclusive: usize) -> Option<String> {
    if end_inclusive < start {
        return None;
    }
    let code: String = raw_text
        .chars()
        .skip(start)
        .take(end_inclusive.saturating_sub(start) + 1)
        .collect();
    let code = code.trim();
    if code.is_empty() {
        None
    } else {
        Some(code.to_owned())
    }
}

// Widget

/// One auto-claim event recorded for the analytics log.
#[derive(Debug, Clone)]
struct BonusClaimEntry {
    at: DateTime<Utc>,
    points: u64,
    balance: u64,
}

/// Capped ring of recent claims per channel (most-recent first).
const BONUS_CLAIM_LOG_LIMIT: usize = 30;

pub struct AnalyticsPanel {
    time_window: TimeWindow,
    active_tab: Tab,
    cached: Option<ChannelStats>,
    last_recompute: Option<Instant>,
    cached_window: Option<TimeWindow>,
    wipe_times: HashMap<ChannelId, DateTime<Utc>>,
    toast: Option<(String, Instant)>,
    bonus_claims: HashMap<ChannelId, VecDeque<BonusClaimEntry>>,
}

impl Default for AnalyticsPanel {
    fn default() -> Self {
        Self {
            time_window: TimeWindow::default(),
            active_tab: Tab::default(),
            cached: None,
            last_recompute: None,
            cached_window: None,
            wipe_times: HashMap::new(),
            toast: None,
            bonus_claims: HashMap::new(),
        }
    }
}

impl AnalyticsPanel {
    /// Append a single auto-claim event to the per-channel log.
    pub fn record_bonus_claim(&mut self, channel: &ChannelId, points: u64, balance: u64) {
        let entry = BonusClaimEntry {
            at: Utc::now(),
            points,
            balance,
        };
        let log = self
            .bonus_claims
            .entry(channel.clone())
            .or_insert_with(VecDeque::new);
        log.push_front(entry);
        while log.len() > BONUS_CLAIM_LOG_LIMIT {
            log.pop_back();
        }
    }

    pub fn tick(&mut self, channel: &ChannelState) {
        let channel_changed = self
            .cached
            .as_ref()
            .is_none_or(|stats| stats.channel != channel.id);
        let window_changed = self.cached_window != Some(self.time_window);
        let stale = self
            .last_recompute
            .map(|t| t.elapsed() >= REFRESH_INTERVAL)
            .unwrap_or(true);
        if channel_changed || window_changed || stale {
            self.cached = Some(compute_stats(
                channel,
                self.time_window,
                self.wipe_times.get(&channel.id).copied(),
            ));
            self.last_recompute = Some(Instant::now());
            self.cached_window = Some(self.time_window);
        }
    }

    fn reset_stats(&mut self, channel: &ChannelId) {
        self.wipe_times.insert(channel.clone(), Utc::now());
        self.cached = None;
        self.last_recompute = None;
    }

    pub fn show(&mut self, ui: &mut Ui, channel: &ChannelState) {
        self.tick(channel);

        // Keep actions beside the title; the description gets its own row
        // so it cannot push the resize handle off screen.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Analytics")
                    .font(t::body())
                    .strong()
                    .color(t::text_primary()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let save_btn = ui
                    .add(
                        egui::Button::new(RichText::new("💾").font(t::small()))
                            .fill(t::bg_raised())
                            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
                    )
                    .on_hover_text("Save snapshot to file");
                if save_btn.clicked() {
                    let msg = self.save_snapshot();
                    self.toast = Some((msg, Instant::now()));
                }
                let wipe_btn = ui
                    .add(
                        egui::Button::new(RichText::new("🗑").font(t::small()))
                            .fill(t::bg_raised())
                            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle())),
                    )
                    .on_hover_text("Reset stats from now");
                if wipe_btn.clicked() {
                    self.reset_stats(&channel.id);
                    self.toast = Some(("Stats reset".to_owned(), Instant::now()));
                }
            });
        });
        ui.add(
            egui::Label::new(
                RichText::new("Recent message, chatter, and activity stats.")
                    .font(t::small())
                    .color(t::text_helper()),
            )
            .wrap(),
        );

        // Toast
        if let Some((msg, at)) = &self.toast {
            let age = at.elapsed().as_secs_f32();
            if age < 3.0 {
                let alpha = ((3.0 - age) / 1.0).min(1.0);
                let col = egui::Color32::from_rgba_unmultiplied(
                    t::green().r(),
                    t::green().g(),
                    t::green().b(),
                    (alpha * 220.0) as u8,
                );
                ui.label(RichText::new(msg.as_str()).font(t::small()).color(col));
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            } else {
                self.toast = None;
            }
        }

        ui.add_space(4.0);

        // Time window selector
        if ui.available_width() < 230.0 {
            egui::ComboBox::from_id_salt("analytics_window")
                .selected_text(self.time_window.label())
                .width(ui.available_width().max(0.0))
                .show_ui(ui, |ui| {
                    for window in [
                        TimeWindow::Last5Min,
                        TimeWindow::Last15Min,
                        TimeWindow::Last30Min,
                        TimeWindow::All,
                    ] {
                        ui.selectable_value(&mut self.time_window, window, window.label());
                    }
                });
        } else {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for w in [
                    TimeWindow::Last5Min,
                    TimeWindow::Last15Min,
                    TimeWindow::Last30Min,
                    TimeWindow::All,
                ] {
                    let selected = self.time_window == w;
                    let btn = egui::Button::new(RichText::new(w.label()).font(t::small()))
                        .fill(if selected {
                            t::accent_dim()
                        } else {
                            t::bg_raised()
                        })
                        .stroke(egui::Stroke::new(
                            1.0_f32,
                            if selected {
                                t::accent()
                            } else {
                                t::border_subtle()
                            },
                        ));
                    if ui.add(btn).clicked() {
                        self.time_window = w;
                    }
                }
            });
        }

        self.tick(channel);

        if let Some(wt) = self.wipe_times.get(&channel.id) {
            let local = wt.with_timezone(&chrono::Local);
            ui.label(
                RichText::new(format!("Since {}", local.format("%H:%M:%S")))
                    .font(t::small())
                    .color(t::text_muted()),
            );
        }

        ui.add_space(4.0);

        // Tab bar
        if ui.available_width() < 300.0 {
            egui::ComboBox::from_id_salt("analytics_tab")
                .selected_text(self.active_tab.label())
                .width(ui.available_width().max(0.0))
                .show_ui(ui, |ui| {
                    for tab in [Tab::Overview, Tab::Chatters, Tab::Emotes, Tab::Activity] {
                        ui.selectable_value(&mut self.active_tab, tab, tab.label());
                    }
                });
        } else {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for tab in [Tab::Overview, Tab::Chatters, Tab::Emotes, Tab::Activity] {
                    let selected = self.active_tab == tab;
                    let btn = egui::Button::new(RichText::new(tab.label()).font(t::small()))
                        .fill(if selected {
                            t::bg_raised()
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::new(
                            1.0_f32,
                            if selected {
                                t::border_accent()
                            } else {
                                t::border_subtle()
                            },
                        ));
                    if ui.add(btn).clicked() {
                        self.active_tab = tab;
                    }
                }
            });
        }
        ui.add_space(4.0);
        ui.add(egui::Separator::default().spacing(0.0));
        ui.add_space(4.0);

        let stats = self.cached.as_ref().unwrap();
        match self.active_tab {
            Tab::Overview => {
                show_overview(ui, stats);
                if let Some(log) = self.bonus_claims.get(&channel.id) {
                    if !log.is_empty() {
                        ui.add_space(6.0);
                        section_header(ui, "BONUS POINT CLAIMS");
                        ui.add_space(3.0);
                        let card = Frame::new()
                            .fill(t::bg_raised())
                            .corner_radius(t::RADIUS_SM)
                            .inner_margin(egui::Margin::symmetric(8, 6));
                        card.show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            for entry in log.iter().take(8) {
                                let local = entry.at.with_timezone(&chrono::Local);
                                let label = format!("{}", local.format("%H:%M:%S"));
                                let value = format!("+{} (now {})", entry.points, entry.balance);
                                stat_row(ui, &label, &value);
                            }
                            if log.len() > 8 {
                                ui.label(
                                    RichText::new(format!("+{} more this session", log.len() - 8))
                                        .font(t::tiny())
                                        .color(t::text_muted()),
                                );
                            }
                        });
                    }
                }
            }
            Tab::Chatters => show_chatters(ui, stats),
            Tab::Emotes => show_emotes(ui, stats),
            Tab::Activity => show_activity(ui, stats),
        }
    }

    fn save_snapshot(&self) -> String {
        let Some(stats) = &self.cached else {
            return "No data to save".to_owned();
        };
        let Some(proj) = directories::ProjectDirs::from("", "", "crust") else {
            return "Could not find data dir".to_owned();
        };
        let dir = proj.data_local_dir().join("analytics");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return format!("Save failed: {e}");
        }
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let channel: String = stats
            .channel
            .as_str()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let filename = format!("{channel}_{ts}.json");
        let path = dir.join(&filename);
        match serde_json::to_string_pretty(stats) {
            Ok(json) => match std::fs::write(&path, json) {
                Ok(()) => format!("Saved {filename}"),
                Err(e) => format!("Save failed: {e}"),
            },
            Err(e) => format!("Serialize failed: {e}"),
        }
    }
}

// Tab renderers

fn show_overview(ui: &mut Ui, stats: &ChannelStats) {
    let card = Frame::new()
        .fill(t::bg_raised())
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(8, 6));
    card.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        stat_row(ui, "Window", &stats.window_label);
        stat_row(ui, "Messages", &stats.total_msgs.to_string());
        stat_row(ui, "Unique chatters", &stats.unique_chatters.to_string());
        if let Some(mpm) = stats.msgs_per_min {
            stat_row(ui, "Msgs / min", &format!("{mpm:.1}"));
        }
        stat_row(
            ui,
            "Avg msg length",
            &format!("{:.0} chars", stats.avg_msg_len),
        );
        stat_row(
            ui,
            "Msgs with emotes",
            &format!("{:.0}%", stats.emote_msg_pct),
        );
    });
    if stats.total_bits > 0 || stats.sub_count > 0 || stats.raid_count > 0 {
        ui.add_space(6.0);
        section_header(ui, "EVENTS");
        ui.add_space(3.0);
        card.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if stats.total_bits > 0 {
                stat_row(ui, "Total bits", &stats.total_bits.to_string());
            }
            if stats.sub_count > 0 {
                stat_row(ui, "Subs / resubs", &stats.sub_count.to_string());
            }
            if stats.raid_count > 0 {
                stat_row(ui, "Raids", &stats.raid_count.to_string());
            }
        });
    }
    if stats.total_msgs > 0 {
        ui.add_space(6.0);
        section_header(ui, "TIMING");
        ui.add_space(3.0);
        card.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            stat_row(ui, "Peak period", &stats.peak_period_label);
            if let Some(t) = stats.first_msg_time {
                stat_row(
                    ui,
                    "Oldest msg",
                    &t.with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string(),
                );
            }
        });
    }
}

fn show_chatters(ui: &mut Ui, stats: &ChannelStats) {
    if stats.top_chatters.is_empty() {
        ui.label(
            RichText::new("No chat data yet")
                .font(t::small())
                .color(t::text_muted()),
        );
        return;
    }
    let max = stats.top_chatters[0].1.max(1);
    ScrollArea::vertical()
        .id_salt("ac_chatters")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            for (login, count) in &stats.top_chatters {
                bar_row_u32(ui, login, *count, max, t::accent());
            }
            if !stats.top_bit_donors.is_empty() {
                ui.add_space(8.0);
                section_header(ui, "TOP BIT DONORS");
                ui.add_space(3.0);
                let bmax = stats.top_bit_donors[0].1.max(1);
                for (login, bits) in &stats.top_bit_donors {
                    bar_row_u64(ui, login, *bits, bmax, t::yellow());
                }
            }
        });
}

fn show_emotes(ui: &mut Ui, stats: &ChannelStats) {
    if stats.top_emotes.is_empty() {
        ui.label(
            RichText::new("No emote data yet")
                .font(t::small())
                .color(t::text_muted()),
        );
        return;
    }
    let max = stats.top_emotes[0].1.max(1);
    ScrollArea::vertical()
        .id_salt("ac_emotes")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            for (code, count) in &stats.top_emotes {
                bar_row_u32(ui, code, *count, max, t::green());
            }
        });
}

fn show_activity(ui: &mut Ui, stats: &ChannelStats) {
    if stats.total_msgs == 0 {
        ui.label(
            RichText::new("No activity in this window")
                .font(t::small())
                .color(t::text_muted()),
        );
        return;
    }
    let max_bucket = *stats.activity_buckets.iter().max().unwrap_or(&1).max(&1);
    let graph_h = 90.0f32;
    let avail_w = ui.available_width().max(0.0);
    let gap = (avail_w / 120.0).min(2.0);
    let bar_w =
        ((avail_w - gap * (ACTIVITY_BUCKETS as f32 - 1.0)) / ACTIVITY_BUCKETS as f32).max(0.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(avail_w, graph_h + 14.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let baseline = rect.top() + graph_h;
    for (i, &count) in stats.activity_buckets.iter().enumerate() {
        let bar_h =
            (count as f32 / max_bucket as f32 * graph_h).max(if count > 0 { 2.0 } else { 0.0 });
        let x = rect.left() + i as f32 * (bar_w + gap);
        let alpha = if count > 0 { 180u8 } else { 20u8 };
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, baseline - bar_h), egui::vec2(bar_w, bar_h)),
            egui::CornerRadius::same(1),
            egui::Color32::from_rgba_unmultiplied(
                t::accent().r(),
                t::accent().g(),
                t::accent().b(),
                alpha,
            ),
        );
    }
    painter.text(
        egui::pos2(rect.left(), baseline + 2.0),
        egui::Align2::LEFT_TOP,
        "older",
        t::small(),
        t::text_muted(),
    );
    painter.text(
        egui::pos2(rect.right(), baseline + 2.0),
        egui::Align2::RIGHT_TOP,
        "now",
        t::small(),
        t::text_muted(),
    );
    ui.add_space(4.0);
    Frame::new()
        .fill(t::bg_raised())
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            stat_row(ui, "Peak period", &stats.peak_period_label);
            stat_row(ui, "Peak msgs", &max_bucket.to_string());
            if let Some(mpm) = stats.msgs_per_min {
                stat_row(ui, "Avg msgs / min", &format!("{mpm:.1}"));
            }
        });
}

// Helpers

fn section_header(ui: &mut Ui, label: &str) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .font(t::heading())
                .color(t::text_muted())
                .strong(),
        );
        ui.add(egui::Separator::default().horizontal().spacing(4.0));
    });
}

fn stat_row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        let width = ui.available_width().max(0.0);
        let gap = ui.spacing().item_spacing.x;
        let label_width = (width - gap).max(0.0) * 0.52;
        let value_width = (width - gap - label_width).max(0.0);
        let height = ui.fonts(|fonts| fonts.row_height(&t::small()));
        ui.add_sized(
            [label_width, height],
            egui::Label::new(RichText::new(label).font(t::small()).color(t::text_secondary()))
                .truncate(),
        )
        .on_hover_text(label);
        ui.add_sized(
            [value_width, height],
            egui::Label::new(RichText::new(value).font(t::small()).color(t::text_primary()))
                .halign(egui::Align::RIGHT)
                .truncate(),
        )
        .on_hover_text(value);
    });
}

fn draw_bar_row(ui: &mut Ui, label: &str, count_str: &str, frac: f32, bar_color: egui::Color32) {
    let row_h = 22.0;
    let avail_w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, row_h), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 2.0),
            egui::vec2((avail_w * frac).max(2.0), row_h - 4.0),
        ),
        egui::CornerRadius::same(2),
        egui::Color32::from_rgba_unmultiplied(bar_color.r(), bar_color.g(), bar_color.b(), 40),
    );
    let cg = ui
        .painter()
        .layout_no_wrap(count_str.to_owned(), t::small(), t::text_secondary());
    let cw = cg.size().x;
    let lg = ui
        .painter()
        .layout_no_wrap(label.to_owned(), t::small(), t::text_primary());
    let clip = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top()),
        egui::pos2(rect.right() - cw - 12.0, rect.bottom()),
    );
    let ty = (row_h - lg.size().y) / 2.0;
    let cy = (row_h - cg.size().y) / 2.0;
    ui.painter().with_clip_rect(clip).galley(
        egui::pos2(rect.left() + 6.0, rect.top() + ty),
        lg,
        t::text_primary(),
    );
    ui.painter().galley(
        egui::pos2(rect.right() - cw - 4.0, rect.top() + cy),
        cg,
        t::text_secondary(),
    );
}

fn bar_row_u32(ui: &mut Ui, label: &str, count: u32, max: u32, color: egui::Color32) {
    draw_bar_row(
        ui,
        label,
        &count.to_string(),
        count as f32 / max as f32,
        color,
    );
}

fn bar_row_u64(ui: &mut Ui, label: &str, count: u64, max: u64, color: egui::Color32) {
    draw_bar_row(
        ui,
        label,
        &count.to_string(),
        count as f32 / max as f32,
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::model::{
        ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, TwitchEmotePos, UserId,
    };

    fn make_message(
        id: u64,
        timestamp: DateTime<Utc>,
        login: &str,
        raw_text: &str,
        msg_kind: MsgKind,
        spans: Vec<Span>,
        twitch_emotes: Vec<TwitchEmotePos>,
    ) -> ChatMessage {
        ChatMessage {
            id: MessageId(id),
            server_id: None,
            timestamp,
            channel: ChannelId::new("test"),
            sender: Sender {
                original_display_name: None,
                user_id: UserId(login.to_owned()),
                login: login.to_owned(),
                display_name: login.to_owned(),
                color: None,
                name_paint: None,
                badges: vec![],
            },
            raw_text: raw_text.to_owned(),
            spans: spans.into_iter().collect(),
            twitch_emotes,
            flags: MessageFlags::default(),
            reply: None,
            msg_kind,
            shared: None,
        }
    }

    fn channel_with_messages(id: ChannelId, count: u64, login: &str) -> ChannelState {
        let mut channel = ChannelState::new(id.clone());
        for message_id in 0..count {
            let mut message = make_message(
                message_id,
                Utc::now() - chrono::Duration::seconds(5),
                login,
                "hello",
                MsgKind::Chat,
                vec![],
                vec![],
            );
            message.channel = id.clone();
            channel.push_message(message);
        }
        channel
    }

    #[test]
    fn switching_channels_refreshes_cached_stats_and_export_identity_immediately() {
        let first = channel_with_messages(ChannelId::new("shared"), 2, "alice");
        let second = channel_with_messages(ChannelId::kick("shared"), 1, "bob");
        let mut panel = AnalyticsPanel::default();

        panel.tick(&first);
        assert_eq!(panel.cached.as_ref().unwrap().total_msgs, 2);
        panel.tick(&second);
        let exported = serde_json::to_value(panel.cached.as_ref().unwrap()).unwrap();
        assert_eq!(exported["channel"], second.id.as_str());
        assert_eq!(exported["total_msgs"], 1);
        assert_eq!(exported["top_chatters"][0][0], "bob");

        panel.tick(&first);
        let stats = panel.cached.as_ref().unwrap();
        assert_eq!(stats.channel, first.id);
        assert_eq!(stats.total_msgs, 2);
        assert_eq!(stats.top_chatters[0].0, "alice");
    }

    #[test]
    fn resetting_stats_only_affects_the_selected_channel() {
        let first = channel_with_messages(ChannelId::new("first"), 2, "alice");
        let second = channel_with_messages(ChannelId::new("second"), 1, "bob");
        let mut panel = AnalyticsPanel::default();
        panel.tick(&first);
        panel.reset_stats(&first.id);
        panel.tick(&first);
        assert_eq!(panel.cached.as_ref().unwrap().total_msgs, 0);
        panel.tick(&second);
        assert_eq!(panel.cached.as_ref().unwrap().total_msgs, 1);
        assert!(!panel.wipe_times.contains_key(&second.id));
        panel.tick(&first);
        assert_eq!(panel.cached.as_ref().unwrap().total_msgs, 0);
    }

    #[test]
    fn reset_button_refreshes_the_stats_before_the_same_frame_renders() {
        let ctx = egui::Context::default();
        let channel = channel_with_messages(ChannelId::new("first"), 2, "alice");
        let mut panel = AnalyticsPanel::default();
        let render = |panel: &mut AnalyticsPanel, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(480.0, 720.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| panel.show(ui, &channel));
                },
            )
        };
        render(&mut panel, vec![]);
        let output = render(&mut panel, vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == "🗑" => {
                    Some(egui::Rect::from_min_size(text.pos, text.galley.size()).center())
                }
                _ => None,
            })
            .expect("reset button is rendered");
        for pressed in [true, false] {
            render(
                &mut panel,
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
        assert!(panel.wipe_times.contains_key(&channel.id));
        assert_eq!(panel.cached.as_ref().unwrap().total_msgs, 0);
    }

    #[test]
    fn analytics_content_fits_at_both_resize_limits_and_each_tab() {
        let channel = channel_with_messages(
            ChannelId::new("test"),
            3,
            "extremely_long_chatter_name_that_used_to_push_the_panel",
        );
        for width in [140.0, 176.0, 260.0, 340.0, 480.0] {
            for tab in [Tab::Overview, Tab::Chatters, Tab::Emotes, Tab::Activity] {
                let ctx = egui::Context::default();
                let mut panel = AnalyticsPanel::default();
                panel.active_tab = tab;
                let mut bounds = None;
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(920.0, 640.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::SidePanel::right("analytics_layout_test")
                            .resizable(false)
                            .exact_width(width)
                            .show(ctx, |ui| {
                                panel.show(ui, &channel);
                                bounds = Some((ui.min_rect(), ui.max_rect()));
                            });
                        egui::CentralPanel::default().show(ctx, |_| {});
                    },
                );
                let (used, available) = bounds.unwrap();
                assert!(
                    used.right() <= available.right() + 1.0,
                    "tab {tab:?} at width {width} extends to {}, panel ends at {}",
                    used.right(), available.right(),
                );
            }
        }
    }

    #[test]
    fn all_window_distributes_activity_across_full_range() {
        let now = Utc::now();
        let mut channel = ChannelState::new(ChannelId::new("test"));
        channel.push_message(make_message(
            1,
            now - chrono::Duration::minutes(120),
            "early_user",
            "hello",
            MsgKind::Chat,
            vec![],
            vec![],
        ));
        channel.push_message(make_message(
            2,
            now - chrono::Duration::seconds(5),
            "late_user",
            "yo",
            MsgKind::Chat,
            vec![],
            vec![],
        ));

        let stats = compute_stats(&channel, TimeWindow::All, None);
        let non_zero_buckets = stats.activity_buckets.iter().filter(|&&v| v > 0).count();
        assert!(non_zero_buckets >= 2);
    }

    #[test]
    fn text_metrics_use_text_message_count() {
        let now = Utc::now();
        let mut channel = ChannelState::new(ChannelId::new("test"));
        channel.push_message(make_message(
            1,
            now - chrono::Duration::seconds(10),
            "user_a",
            "hello",
            MsgKind::Chat,
            vec![Span::Emote {
                id: "1".to_owned(),
                code: "Kappa".to_owned(),
                url: "https://example.com/kappa.png".to_owned(),
                url_hd: None,
                provider: "twitch".to_owned(),
            }],
            vec![],
        ));
        channel.push_message(make_message(
            2,
            now - chrono::Duration::seconds(5),
            "user_b",
            "abc",
            MsgKind::Bits { amount: 100 },
            vec![],
            vec![],
        ));

        let stats = compute_stats(&channel, TimeWindow::All, None);
        assert_eq!(stats.total_msgs, 2);
        assert!((stats.avg_msg_len - 4.0).abs() < 0.001);
        assert!((stats.emote_msg_pct - 50.0).abs() < 0.001);
    }

    #[test]
    fn chatter_counts_are_case_insensitive() {
        let now = Utc::now();
        let mut channel = ChannelState::new(ChannelId::new("test"));
        channel.push_message(make_message(
            1,
            now - chrono::Duration::seconds(8),
            "Frank",
            "one",
            MsgKind::Chat,
            vec![],
            vec![],
        ));
        channel.push_message(make_message(
            2,
            now - chrono::Duration::seconds(4),
            "frank",
            "two",
            MsgKind::Chat,
            vec![],
            vec![],
        ));

        let stats = compute_stats(&channel, TimeWindow::All, None);
        assert_eq!(stats.unique_chatters, 1);
        assert_eq!(stats.top_chatters[0].1, 2);
    }

    #[test]
    fn uses_twitch_emote_positions_when_spans_missing() {
        let now = Utc::now();
        let mut channel = ChannelState::new(ChannelId::new("test"));
        channel.push_message(make_message(
            1,
            now - chrono::Duration::seconds(3),
            "user_a",
            "Kappa",
            MsgKind::Chat,
            vec![],
            vec![TwitchEmotePos {
                id: "25".to_owned(),
                start: 0,
                end: 4,
            }],
        ));

        let stats = compute_stats(&channel, TimeWindow::All, None);
        assert_eq!(
            stats.top_emotes.first().map(|(c, _)| c.as_str()),
            Some("Kappa")
        );
        assert_eq!(stats.top_emotes.first().map(|(_, n)| *n), Some(1));
    }
}
