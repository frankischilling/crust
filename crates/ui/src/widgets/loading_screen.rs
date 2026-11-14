use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::{Color32, Context};

use crate::theme as t;

const STATUS_TIMEOUT: Duration = Duration::from_secs(60);
const READY_DURATION: Duration = Duration::from_millis(700);
const CHANNEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Startup events update the status strip without blocking chat or settings.
pub enum LoadEvent {
    Connecting,
    Connected,
    Disconnected,
    Authenticated { username: String },
    ChannelJoined { channel: String },
    StartupChannelsConfigured { channels: Vec<String> },
    CatalogLoaded { count: usize },
    HistoryLoaded { channel: String, count: usize },
    EmoteImageReady,
    ChannelEmotesLoaded { channel: String, count: usize },
    ImagePrefetchQueued { count: usize },
}

pub struct LoadingScreen {
    started_at: Instant,
    connected: bool,
    disconnected: bool,
    catalog_loaded: bool,
    configured: bool,
    configured_channels: HashSet<String>,
    joined_channels: HashMap<String, Instant>,
    history_channels: HashSet<String>,
    emote_channels: HashSet<String>,
    catalog_count: usize,
    history_count: usize,
    ready_at: Option<Instant>,
    dismissed: bool,
}

impl Default for LoadingScreen {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            connected: false,
            disconnected: false,
            catalog_loaded: false,
            configured: false,
            configured_channels: HashSet::new(),
            joined_channels: HashMap::new(),
            history_channels: HashSet::new(),
            emote_channels: HashSet::new(),
            catalog_count: 0,
            history_count: 0,
            ready_at: None,
            dismissed: false,
        }
    }
}

impl LoadingScreen {
    pub fn on_event(&mut self, event: LoadEvent) {
        if self.dismissed {
            return;
        }
        match event {
            LoadEvent::Connecting | LoadEvent::Disconnected => {
                self.connected = false;
                self.disconnected = matches!(event, LoadEvent::Disconnected);
                self.ready_at = None;
                self.joined_channels.clear();
                self.history_channels.clear();
                self.emote_channels.clear();
            }
            LoadEvent::Connected => {
                self.connected = true;
                self.disconnected = false;
            }
            LoadEvent::Authenticated { username } => {
                if !username.is_empty() {
                    self.connected = true;
                    self.disconnected = false;
                }
            }
            LoadEvent::StartupChannelsConfigured { channels } => {
                self.configured = true;
                self.configured_channels = channels
                    .iter()
                    .filter_map(|channel| twitch_channel(channel))
                    .collect();
            }
            LoadEvent::ChannelJoined { channel } => {
                if let Some(channel) = twitch_channel(&channel) {
                    self.joined_channels
                        .entry(channel)
                        .or_insert_with(Instant::now);
                }
            }
            LoadEvent::CatalogLoaded { count } => {
                self.catalog_loaded = true;
                self.catalog_count = count;
            }
            LoadEvent::HistoryLoaded { channel, count } => {
                if let Some(channel) = twitch_channel(&channel) {
                    self.history_channels.insert(channel);
                    self.history_count = self.history_count.saturating_add(count);
                }
            }
            LoadEvent::ChannelEmotesLoaded { channel, count: _ } => {
                if let Some(channel) = twitch_channel(&channel) {
                    self.emote_channels.insert(channel);
                }
            }
            LoadEvent::EmoteImageReady | LoadEvent::ImagePrefetchQueued { count: _ } => {}
        }
        self.tick();
    }

    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    fn tick_at(&mut self, now: Instant) {
        if self.dismissed {
            return;
        }
        if now.saturating_duration_since(self.started_at) >= STATUS_TIMEOUT
            || self
                .ready_at
                .is_some_and(|ready| now.saturating_duration_since(ready) >= READY_DURATION)
        {
            self.dismissed = true;
            return;
        }
        if self.ready_at.is_none() && self.connected && self.catalog_loaded && self.configured {
            let complete = self.configured_channels.iter().all(|channel| {
                self.joined_channels.get(channel).is_some_and(|joined| {
                    (self.history_channels.contains(channel)
                        && self.emote_channels.contains(channel))
                        || now.saturating_duration_since(*joined) >= CHANNEL_SETTLE_TIMEOUT
                })
            });
            if complete {
                self.ready_at = Some(now);
            }
        }
    }

    pub fn is_active(&self) -> bool {
        !self.dismissed
    }

    /// Reserve one status row; the caller continues rendering the normal UI.
    pub fn show(&mut self, ctx: &Context) {
        self.tick();
        if !self.is_active() {
            return;
        }
        let (label, color) = self.status();
        egui::TopBottomPanel::top("startup_status")
            .resizable(false)
            .frame(egui::Frame::new().fill(t::bg_surface()).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Hide").on_hover_text("Hide startup status. Loading continues.").clicked() {
                            self.dismissed = true;
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add_sized(
                                [ui.available_width(), 18.0],
                                egui::Label::new(egui::RichText::new(label).font(t::small()).color(color)).truncate(),
                            ).on_hover_text(format!(
                                "{} channels joined, {} emotes available, {} history messages loaded. Chat and settings are available while loading continues.",
                                self.joined_channels.len(), self.catalog_count, self.history_count,
                            ));
                        });
                    });
                });
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn status(&self) -> (&'static str, Color32) {
        if self.ready_at.is_some() {
            ("Chat is ready", t::green())
        } else if self.connected {
            ("Loading chats and emotes...", t::text_secondary())
        } else if self.disconnected {
            ("Chat is disconnected", t::yellow())
        } else {
            ("Connecting to chat...", t::yellow())
        }
    }
}

fn twitch_channel(raw: &str) -> Option<String> {
    if raw.starts_with("kick:") || raw.starts_with("irc:") {
        return None;
    }
    let channel = raw.trim().trim_start_matches('#').to_ascii_lowercase();
    ((3..=25).contains(&channel.len())
        && channel
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'))
    .then_some(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_catalog_does_not_report_a_network_connection() {
        let mut status = LoadingScreen::default();
        status.on_event(LoadEvent::StartupChannelsConfigured {
            channels: Vec::new(),
        });
        status.on_event(LoadEvent::CatalogLoaded { count: 20 });
        assert_eq!(status.status().0, "Connecting to chat...");
        assert!(status.ready_at.is_none());
        status.on_event(LoadEvent::Connected);
        assert!(status.ready_at.is_some());
        let ready = status.ready_at.unwrap();
        status.tick_at(ready + READY_DURATION);
        assert!(!status.is_active());
    }

    #[test]
    fn startup_waits_for_configured_channels_without_reopening_after_dismissal() {
        let mut status = LoadingScreen::default();
        status.on_event(LoadEvent::Connected);
        status.on_event(LoadEvent::CatalogLoaded { count: 20 });
        status.on_event(LoadEvent::StartupChannelsConfigured {
            channels: vec!["room".into(), "kick:other".into()],
        });
        assert!(status.ready_at.is_none());
        status.on_event(LoadEvent::ChannelJoined {
            channel: "room".into(),
        });
        status.on_event(LoadEvent::HistoryLoaded {
            channel: "room".into(),
            count: 0,
        });
        assert!(status.ready_at.is_none());
        status.on_event(LoadEvent::ChannelEmotesLoaded {
            channel: "room".into(),
            count: 0,
        });
        assert!(status.ready_at.is_some());
        status.dismissed = true;
        status.on_event(LoadEvent::Connecting);
        assert!(!status.is_active());
    }

    #[test]
    fn reconnecting_clears_readiness_before_remaining_channel_data_arrives() {
        let mut status = LoadingScreen::default();
        status.on_event(LoadEvent::StartupChannelsConfigured {
            channels: vec!["room".into()],
        });
        status.on_event(LoadEvent::Connected);
        status.on_event(LoadEvent::CatalogLoaded { count: 20 });
        status.on_event(LoadEvent::ChannelJoined {
            channel: "room".into(),
        });
        status.on_event(LoadEvent::Connecting);
        status.on_event(LoadEvent::HistoryLoaded {
            channel: "room".into(),
            count: 10,
        });
        status.on_event(LoadEvent::ChannelEmotesLoaded {
            channel: "room".into(),
            count: 20,
        });
        status.tick_at(Instant::now() + CHANNEL_SETTLE_TIMEOUT);
        assert!(status.ready_at.is_none());
        assert_eq!(status.status().0, "Connecting to chat...");
        status.on_event(LoadEvent::Connected);
        assert!(
            status.ready_at.is_none(),
            "a new connection must rejoin its channels"
        );
        status.on_event(LoadEvent::ChannelJoined {
            channel: "room".into(),
        });
        assert!(status.ready_at.is_some());
    }

    #[test]
    fn disconnect_during_ready_display_cancels_the_ready_status() {
        let mut status = LoadingScreen::default();
        status.on_event(LoadEvent::StartupChannelsConfigured {
            channels: Vec::new(),
        });
        status.on_event(LoadEvent::CatalogLoaded { count: 20 });
        status.on_event(LoadEvent::Connected);
        let ready = status.ready_at.unwrap();
        status.on_event(LoadEvent::Disconnected);
        status.tick_at(ready + READY_DURATION);
        assert!(status.is_active());
        assert!(status.ready_at.is_none());
        assert_eq!(status.status().0, "Chat is disconnected");
    }

    #[test]
    fn unavailable_startup_services_expire_without_claiming_readiness() {
        let mut status = LoadingScreen::default();
        status.tick_at(status.started_at + STATUS_TIMEOUT);
        assert!(!status.is_active());
        assert!(status.ready_at.is_none());
    }

    #[test]
    fn startup_status_leaves_room_for_usable_controls_in_a_narrow_window() {
        let context = Context::default();
        let mut status = LoadingScreen::default();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(220.0, 200.0),
                )),
                ..Default::default()
            },
            |ctx| {
                status.show(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    assert!(ui.available_height() > 140.0);
                    assert!(ui.button("Settings").rect.right() <= 220.0);
                });
            },
        );
        assert!(!output.shapes.is_empty());
    }
}
