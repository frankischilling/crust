//! Real-time performance overlay for debug builds.
//!
//! Tracks:
//!   - Frame time & FPS (rolling 120-frame window)
//!   - Events processed per frame
//!   - Repaint efficiency (event-driven repaints vs. total frames)
//!   - Emote cache size (entries + estimated RAM)
//!   - Image-fetch commands in flight

use std::collections::VecDeque;
use std::time::Instant;

use crate::widgets::message_list::MessageListPerfStats;
use egui::{Color32, Context, RichText, Window};

const WINDOW: usize = 120; // frames to average over
const TIMELINE_WINDOW: usize = 600;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatPerfStats {
    pub retained_rows: usize,
    pub active_rows: usize,
    pub rendered_rows: usize,
    pub boundary_hidden_rows: usize,
    pub prefix_rebuilds: u32,
    pub height_cache_misses: usize,
}

impl ChatPerfStats {
    pub fn accumulate(&mut self, stats: &MessageListPerfStats) {
        self.retained_rows += stats.retained_rows;
        self.active_rows += stats.active_rows;
        self.rendered_rows += stats.rendered_rows;
        self.boundary_hidden_rows += stats.boundary_hidden_rows;
        self.prefix_rebuilds += u32::from(stats.prefix_rebuilt);
        self.height_cache_misses += stats.height_cache_misses;
    }
}

/// Accumulated per-frame stats.
pub struct PerfOverlay {
    pub visible: bool,

    // Frame timing
    last_frame: Instant,
    frame_ms: VecDeque<f32>, // rolling frame durations (ms)
    fps_timeline: VecDeque<f32>,

    // Event throughput
    events_per_frame: VecDeque<u32>,
    total_events: u64,
    total_frames: u64,

    // Repaint efficiency: how often did drain_events() return true?
    event_driven_repaints: u64,

    // Snapshot state updated each frame
    pub emote_count: usize,
    pub emote_ram_kb: usize, // rough estimate
    chat_stats: ChatPerfStats,
}

impl Default for PerfOverlay {
    fn default() -> Self {
        Self {
            visible: false,
            last_frame: Instant::now(),
            frame_ms: VecDeque::with_capacity(WINDOW),
            fps_timeline: VecDeque::with_capacity(TIMELINE_WINDOW),
            events_per_frame: VecDeque::with_capacity(WINDOW),
            total_events: 0,
            total_frames: 0,
            event_driven_repaints: 0,
            emote_count: 0,
            emote_ram_kb: 0,
            chat_stats: ChatPerfStats::default(),
        }
    }
}

impl PerfOverlay {
    /// Call at the **top** of `eframe::App::update()`.
    /// `events` = number of AppEvents drained this frame.
    /// `had_events` = whether drain_events() returned true.
    pub fn record_frame(&mut self, events: u32, had_events: bool) {
        let now = Instant::now();
        let dt_ms = now.duration_since(self.last_frame).as_secs_f32() * 1000.0;
        self.last_frame = now;

        push_ring(&mut self.frame_ms, dt_ms, WINDOW);
        let fps_sample = if dt_ms > 0.0 { 1000.0 / dt_ms } else { 0.0 };
        push_ring(&mut self.fps_timeline, fps_sample, TIMELINE_WINDOW);
        push_ring(&mut self.events_per_frame, events, WINDOW);

        self.total_events += events as u64;
        self.total_frames += 1;
        if had_events {
            self.event_driven_repaints += 1;
        }
    }

    pub fn set_chat_stats(&mut self, stats: ChatPerfStats) {
        self.chat_stats = stats;
    }

    /// Render the overlay window.  Call this anywhere inside `update()`.
    pub fn show(&self, ctx: &Context) {
        if !self.visible {
            return;
        }

        let avg_ms = avg_f32(&self.frame_ms);
        let worst_ms = self.frame_ms.iter().cloned().fold(0.0_f32, f32::max);
        let best_ms = self.frame_ms.iter().cloned().fold(f32::MAX, f32::min);
        let fps = if avg_ms > 0.0 { 1000.0 / avg_ms } else { 0.0 };

        let avg_events = avg_u32(&self.events_per_frame);
        let repaint_pct = if self.total_frames > 0 {
            100.0 * self.event_driven_repaints as f32 / self.total_frames as f32
        } else {
            0.0
        };

        Window::new("⚡ Performance")
            .collapsible(true)
            .resizable(false)
            .default_pos([8.0, 40.0])
            .show(ctx, |ui| {
                egui::Grid::new("perf_grid")
                    .num_columns(2)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        // FPS
                        ui.label(RichText::new("FPS").color(Color32::GRAY));
                        ui.label(
                            RichText::new(format!("{fps:.1}"))
                                .color(fps_color(fps))
                                .strong(),
                        );
                        ui.end_row();

                        // Frame time
                        ui.label(RichText::new("Frame (avg)").color(Color32::GRAY));
                        ui.label(format!("{avg_ms:.2} ms"));
                        ui.end_row();

                        ui.label(RichText::new("Frame (worst)").color(Color32::GRAY));
                        ui.label(RichText::new(format!("{worst_ms:.2} ms")).color(
                            if worst_ms > 33.0 {
                                Color32::YELLOW
                            } else {
                                Color32::WHITE
                            },
                        ));
                        ui.end_row();

                        ui.label(RichText::new("Frame (best)").color(Color32::GRAY));
                        ui.label(format!("{best_ms:.2} ms"));
                        ui.end_row();

                        ui.label(RichText::new("FPS trend").color(Color32::GRAY));
                        draw_sparkline(ui, &self.fps_timeline, 180.0, 36.0);
                        ui.end_row();

                        ui.separator();
                        ui.separator();
                        ui.end_row();

                        // Events
                        ui.label(RichText::new("Events/frame").color(Color32::GRAY));
                        ui.label(format!("{avg_events:.2}"));
                        ui.end_row();

                        ui.label(RichText::new("Events total").color(Color32::GRAY));
                        ui.label(format!("{}", self.total_events));
                        ui.end_row();

                        ui.label(RichText::new("Frames total").color(Color32::GRAY));
                        ui.label(format!("{}", self.total_frames));
                        ui.end_row();

                        // Repaint efficiency
                        ui.label(RichText::new("Repaint efficiency").color(Color32::GRAY));
                        ui.label(
                            RichText::new(format!("{repaint_pct:.1}% event-driven")).color(
                                if repaint_pct < 5.0 {
                                    Color32::GREEN // mostly idle - good
                                } else {
                                    Color32::WHITE
                                },
                            ),
                        );
                        ui.end_row();

                        ui.separator();
                        ui.separator();
                        ui.end_row();

                        // Emote cache
                        ui.label(RichText::new("Emotes cached").color(Color32::GRAY));
                        ui.label(format!("{}", self.emote_count));
                        ui.end_row();

                        ui.label(RichText::new("Emote RAM ~").color(Color32::GRAY));
                        let (val, unit) = human_bytes(self.emote_ram_kb * 1024);
                        ui.label(format!("{val:.1} {unit}"));
                        ui.end_row();

                        ui.separator();
                        ui.separator();
                        ui.end_row();

                        ui.label(RichText::new("Rows retained").color(Color32::GRAY));
                        ui.label(format!("{}", self.chat_stats.retained_rows));
                        ui.end_row();

                        ui.label(RichText::new("Rows active").color(Color32::GRAY));
                        ui.label(format!("{}", self.chat_stats.active_rows));
                        ui.end_row();

                        ui.label(RichText::new("Rows rendered").color(Color32::GRAY));
                        ui.label(format!("{}", self.chat_stats.rendered_rows));
                        ui.end_row();

                        ui.label(RichText::new("Rows hidden").color(Color32::GRAY));
                        ui.label(format!("{}", self.chat_stats.boundary_hidden_rows));
                        ui.end_row();

                        ui.label(RichText::new("Prefix rebuilds").color(Color32::GRAY));
                        ui.label(format!("{}", self.chat_stats.prefix_rebuilds));
                        ui.end_row();

                        ui.label(RichText::new("Height misses").color(Color32::GRAY));
                        ui.label(format!("{}", self.chat_stats.height_cache_misses));
                        ui.end_row();
                    });
            });
    }
}

// Helpers

fn push_ring<T>(buf: &mut VecDeque<T>, val: T, cap: usize) {
    if buf.len() == cap {
        buf.pop_front();
    }
    buf.push_back(val);
}

fn avg_f32(buf: &VecDeque<f32>) -> f32 {
    if buf.is_empty() {
        return 0.0;
    }
    buf.iter().sum::<f32>() / buf.len() as f32
}

fn avg_u32(buf: &VecDeque<u32>) -> f32 {
    if buf.is_empty() {
        return 0.0;
    }
    buf.iter().map(|&x| x as f32).sum::<f32>() / buf.len() as f32
}

fn fps_color(fps: f32) -> Color32 {
    if fps >= 55.0 {
        Color32::GREEN
    } else if fps >= 30.0 {
        Color32::YELLOW
    } else {
        Color32::RED
    }
}

fn sparkline_points(samples: &VecDeque<f32>, width: f32, height: f32) -> Vec<egui::Pos2> {
    if samples.is_empty() {
        return Vec::new();
    }

    let max = samples.iter().copied().fold(0.0_f32, f32::max).max(1.0);
    let denom = (samples.len().saturating_sub(1)).max(1) as f32;
    samples
        .iter()
        .enumerate()
        .map(|(idx, sample)| {
            let x = (idx as f32 / denom) * width;
            let y = height - ((*sample / max).clamp(0.0, 1.0) * height);
            egui::pos2(x, y)
        })
        .collect()
}

fn draw_sparkline(ui: &mut egui::Ui, samples: &VecDeque<f32>, width: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let bg = if ui.visuals().dark_mode {
        Color32::from_rgb(28, 31, 38)
    } else {
        Color32::from_rgb(240, 243, 247)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(4), bg);
    let points = sparkline_points(samples, rect.width(), rect.height())
        .into_iter()
        .map(|p| rect.left_top() + p.to_vec2())
        .collect::<Vec<_>>();
    if points.len() >= 2 {
        ui.painter().add(egui::Shape::line(
            points,
            egui::Stroke::new(1.5, Color32::from_rgb(90, 200, 140)),
        ));
    }
}

fn human_bytes(bytes: usize) -> (f32, &'static str) {
    if bytes >= 1_073_741_824 {
        (bytes as f32 / 1_073_741_824.0, "GB")
    } else if bytes >= 1_048_576 {
        (bytes as f32 / 1_048_576.0, "MB")
    } else if bytes >= 1_024 {
        (bytes as f32 / 1_024.0, "KB")
    } else {
        (bytes as f32, "B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::message_list::MessageListPerfStats;

    #[test]
    fn push_ring_drops_oldest_sample_when_capacity_is_reached() {
        let mut buf = VecDeque::new();
        push_ring(&mut buf, 10_u32, 3);
        push_ring(&mut buf, 20_u32, 3);
        push_ring(&mut buf, 30_u32, 3);
        push_ring(&mut buf, 40_u32, 3);

        assert_eq!(buf.into_iter().collect::<Vec<_>>(), vec![20, 30, 40]);
    }

    #[test]
    fn sparkline_points_keep_sample_order() {
        let pts = sparkline_points(&VecDeque::from([10.0, 20.0, 15.0]), 120.0, 40.0);

        assert_eq!(pts.len(), 3);
        assert!(pts[0].x < pts[1].x);
        assert!(pts[1].x < pts[2].x);
    }

    #[test]
    fn chat_stats_accumulate_and_replace_previous_snapshot() {
        let mut stats = ChatPerfStats::default();
        stats.accumulate(&MessageListPerfStats {
            retained_rows: 1_500,
            active_rows: 400,
            rendered_rows: 36,
            boundary_hidden_rows: 1_100,
            prefix_rebuilt: true,
            height_cache_misses: 8,
        });
        stats.accumulate(&MessageListPerfStats {
            retained_rows: 700,
            active_rows: 300,
            rendered_rows: 28,
            boundary_hidden_rows: 400,
            prefix_rebuilt: false,
            height_cache_misses: 3,
        });

        assert_eq!(stats.retained_rows, 2_200);
        assert_eq!(stats.active_rows, 700);
        assert_eq!(stats.rendered_rows, 64);
        assert_eq!(stats.boundary_hidden_rows, 1_500);
        assert_eq!(stats.prefix_rebuilds, 1);
        assert_eq!(stats.height_cache_misses, 11);

        let mut overlay = PerfOverlay::default();
        overlay.set_chat_stats(stats.clone());
        assert_eq!(overlay.chat_stats, stats);

        overlay.set_chat_stats(ChatPerfStats::default());
        assert_eq!(overlay.chat_stats, ChatPerfStats::default());
    }
}
