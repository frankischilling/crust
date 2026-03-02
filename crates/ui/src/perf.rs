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

use egui::{Color32, Context, RichText, Window};

const WINDOW: usize = 120; // frames to average over

/// Accumulated per-frame stats.
pub struct PerfOverlay {
    pub visible: bool,

    // Frame timing
    last_frame: Instant,
    frame_ms: VecDeque<f32>,    // rolling frame durations (ms)

    // Event throughput
    events_per_frame: VecDeque<u32>,
    total_events: u64,
    total_frames: u64,

    // Repaint efficiency: how often did drain_events() return true?
    event_driven_repaints: u64,

    // Snapshot state updated each frame
    pub emote_count: usize,
    pub emote_ram_kb: usize,  // rough estimate
}

impl Default for PerfOverlay {
    fn default() -> Self {
        Self {
            visible: false,
            last_frame: Instant::now(),
            frame_ms: VecDeque::with_capacity(WINDOW),
            events_per_frame: VecDeque::with_capacity(WINDOW),
            total_events: 0,
            total_frames: 0,
            event_driven_repaints: 0,
            emote_count: 0,
            emote_ram_kb: 0,
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
        push_ring(&mut self.events_per_frame, events, WINDOW);

        self.total_events += events as u64;
        self.total_frames += 1;
        if had_events {
            self.event_driven_repaints += 1;
        }
    }

    /// Render the overlay window.  Call this anywhere inside `update()`.
    pub fn show(&self, ctx: &Context) {
        if !self.visible {
            return;
        }

        let avg_ms = avg_f32(&self.frame_ms);
        let worst_ms = self.frame_ms.iter().cloned().fold(0.0_f32, f32::max);
        let best_ms  = self.frame_ms.iter().cloned().fold(f32::MAX, f32::min);
        let fps      = if avg_ms > 0.0 { 1000.0 / avg_ms } else { 0.0 };

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
                        ui.label(
                            RichText::new(format!("{worst_ms:.2} ms"))
                                .color(if worst_ms > 33.0 { Color32::YELLOW } else { Color32::WHITE }),
                        );
                        ui.end_row();

                        ui.label(RichText::new("Frame (best)").color(Color32::GRAY));
                        ui.label(format!("{best_ms:.2} ms"));
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
                            RichText::new(format!("{repaint_pct:.1}% event-driven"))
                                .color(if repaint_pct < 5.0 {
                                    Color32::GREEN   // mostly idle — good
                                } else {
                                    Color32::WHITE
                                }),
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
    if buf.is_empty() { return 0.0; }
    buf.iter().sum::<f32>() / buf.len() as f32
}

fn avg_u32(buf: &VecDeque<u32>) -> f32 {
    if buf.is_empty() { return 0.0; }
    buf.iter().map(|&x| x as f32).sum::<f32>() / buf.len() as f32
}

fn fps_color(fps: f32) -> Color32 {
    if fps >= 55.0 { Color32::GREEN }
    else if fps >= 30.0 { Color32::YELLOW }
    else { Color32::RED }
}

fn human_bytes(bytes: usize) -> (f32, &'static str) {
    if bytes >= 1_073_741_824 { (bytes as f32 / 1_073_741_824.0, "GB") }
    else if bytes >= 1_048_576 { (bytes as f32 / 1_048_576.0, "MB") }
    else if bytes >= 1_024 { (bytes as f32 / 1_024.0, "KB") }
    else { (bytes as f32, "B") }
}
