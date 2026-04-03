use std::collections::HashMap;

use egui::{Color32, Context, Frame, RichText, TopBottomPanel};

use crust_core::AppState;

use crate::theme as t;

/// Stream status snapshot for one channel, populated via FetchUserProfile.
#[derive(Clone)]
pub struct StreamStatusInfo {
    pub is_live: bool,
    pub title: Option<String>,
    pub game: Option<String>,
    pub viewers: Option<u64>,
}

pub fn show_channel_info_bars(
    ctx: &Context,
    state: &AppState,
    stream_statuses: &HashMap<String, StreamStatusInfo>,
) {
    // Shows live/offline status, viewer count and stream title for the
    // currently active channel. Hidden when no channel is active.
    if let Some(active_ch) = state.active_channel.as_ref() {
        if !active_ch.is_twitch() {
            TopBottomPanel::top("stream_info_bar")
                .exact_height(28.0)
                .frame(
                    Frame::new()
                        .fill(t::bg_surface())
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .stroke(egui::Stroke::NONE),
                )
                .show(ctx, |ui| {
                    let prefix = if active_ch.is_kick() || active_ch.is_irc_server_tab() {
                        ""
                    } else {
                        "#"
                    };
                    let platform = if active_ch.is_kick() { "Kick" } else { "IRC" };
                    let topic = state
                        .channels
                        .get(active_ch)
                        .and_then(|ch| ch.topic.as_deref())
                        .unwrap_or("");
                    let bar_w = ui.available_width();
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{prefix}{}", active_ch.display_name()))
                                    .strong()
                                    .font(t::small())
                                    .color(t::text_primary()),
                            )
                            .truncate(),
                        );
                        if bar_w > 120.0 {
                            ui.label(
                                RichText::new(platform)
                                    .font(t::small())
                                    .color(t::text_muted()),
                            );
                        }
                        if !topic.is_empty() && bar_w > 200.0 {
                            ui.label(RichText::new("-").font(t::small()).color(t::text_muted()));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(topic)
                                        .font(t::small())
                                        .color(t::text_secondary()),
                                )
                                .truncate(),
                            );
                        }
                    });
                });
        } else {
            let login = active_ch.display_name().to_ascii_lowercase();
            let status = stream_statuses.get(&login);
            // Subtle red tint on the bar background when the channel is live.
            let bar_is_live = status.map(|s| s.is_live).unwrap_or(false);
            let bar_fill = if bar_is_live {
                t::live_tint_bg()
            } else {
                t::bg_surface()
            };
            TopBottomPanel::top("stream_info_bar")
                .exact_height(28.0)
                .frame(
                    Frame::new()
                        .fill(bar_fill)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .stroke(egui::Stroke::NONE),
                )
                .show(ctx, |ui| {
                    let bar_w = ui.available_width();
                    let compact = bar_w < 640.0;
                    let ultra_compact = bar_w < 360.0;
                    let show_viewers = bar_w >= 420.0;
                    let show_game = bar_w >= 700.0;
                    let show_title = bar_w >= 260.0;

                    // Thin accent stripe on the very left edge when live.
                    if bar_is_live {
                        let br = ui.max_rect();
                        let strip =
                            egui::Rect::from_min_size(br.left_top(), egui::vec2(3.0, br.height()));
                        ui.painter().rect_filled(strip, 0.0, t::red());
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = if ultra_compact { 4.0 } else { 8.0 };
                        match status {
                            None => {
                                // Not fetched yet - show the channel name only.
                                let ch_prefix =
                                    if active_ch.is_kick() || active_ch.is_irc_server_tab() {
                                        ""
                                    } else {
                                        "#"
                                    };
                                ui.label(
                                    RichText::new(format!(
                                        "{ch_prefix}{}",
                                        active_ch.display_name()
                                    ))
                                    .strong()
                                    .font(t::small())
                                    .color(t::text_primary()),
                                );
                                if !ultra_compact {
                                    ui.label(
                                        RichText::new("Fetching stream status...")
                                            .font(t::small())
                                            .color(t::text_muted()),
                                    );
                                }
                            }
                            Some(s) => {
                                let status_text = if s.is_live { "LIVE" } else { "OFFLINE" };
                                let status_col = if s.is_live { t::red() } else { t::text_muted() };
                                let status_bg = if s.is_live {
                                    t::danger_soft_bg()
                                } else {
                                    t::alpha(t::text_muted(), 20)
                                };

                                egui::Frame::new()
                                    .fill(status_bg)
                                    .stroke(egui::Stroke::new(1.0, status_col.gamma_multiply(0.5)))
                                    .corner_radius(t::RADIUS_SM)
                                    .inner_margin(egui::Margin::symmetric(6, 1))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 4.0;
                                            ui.label(
                                                RichText::new("●")
                                                    .font(t::small())
                                                    .color(status_col),
                                            );
                                            if !ultra_compact {
                                                ui.label(
                                                    RichText::new(status_text)
                                                        .font(t::small())
                                                        .color(status_col)
                                                        .strong(),
                                                );
                                            }
                                        });
                                    });

                                let ch_prefix2 =
                                    if active_ch.is_kick() || active_ch.is_irc_server_tab() {
                                        ""
                                    } else {
                                        "#"
                                    };
                                ui.label(
                                    RichText::new(format!(
                                        "{ch_prefix2}{}",
                                        active_ch.display_name()
                                    ))
                                    .strong()
                                    .font(t::small())
                                    .color(t::text_primary()),
                                );

                                // Viewer count (live only)
                                if s.is_live {
                                    if show_viewers {
                                        if let Some(viewers) = s.viewers {
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} viewers",
                                                    fmt_viewers(viewers)
                                                ))
                                                .font(t::small())
                                                .color(t::text_secondary()),
                                            );
                                        }
                                    }

                                    // Game
                                    if show_game {
                                        if let Some(ref game) = s.game {
                                            if !game.is_empty() {
                                                ui.label(
                                                    RichText::new(game.as_str())
                                                        .font(t::small())
                                                        .color(t::text_secondary()),
                                                );
                                            }
                                        }
                                    }

                                    // Stream title uses any remaining horizontal space.
                                    if show_title {
                                        if let Some(ref title) = s.title {
                                            if !title.is_empty() {
                                                let rem = ui.available_width();
                                                let min_title_w = if ultra_compact {
                                                    24.0
                                                } else if compact {
                                                    56.0
                                                } else {
                                                    140.0
                                                };
                                                if rem > min_title_w {
                                                    ui.add_sized(
                                                        [rem, 16.0],
                                                        egui::Label::new(
                                                            RichText::new(title.as_str())
                                                                .font(t::small())
                                                                .color(t::text_muted()),
                                                        )
                                                        .truncate(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(ref title) = s.title {
                                    if !title.is_empty() && show_title {
                                        let rem = ui.available_width();
                                        let min_title_w = if ultra_compact {
                                            24.0
                                        } else if compact {
                                            56.0
                                        } else {
                                            80.0
                                        };
                                        if rem > min_title_w {
                                            ui.add_sized(
                                                [rem, 16.0],
                                                egui::Label::new(
                                                    RichText::new(title.as_str())
                                                        .font(t::small())
                                                        .color(t::text_muted()),
                                                )
                                                .truncate(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    });
                });

            // Room state pills (sub-only, slow, emote-only, etc.)
            // Shown as a thin strip below the stream info bar when any mode
            // is active - Twitch channels only.
            let room = state.channels.get(active_ch).map(|ch| &ch.room_state);
            let live_viewers = status.and_then(|s| if s.is_live { s.viewers } else { None });
            let has_active_modes = room
                .map(|rs| {
                    rs.emote_only
                        || rs.subscribers_only
                        || rs.r9k
                        || rs.followers_only.map(|v| v >= 0).unwrap_or(false)
                        || rs.slow_mode.map(|v| v > 0).unwrap_or(false)
                })
                .unwrap_or(false)
                || live_viewers.is_some();
            if has_active_modes {
                TopBottomPanel::top("room_state_bar")
                    .exact_height(20.0)
                    .frame(
                        Frame::new()
                            .fill(t::bg_base())
                            .inner_margin(egui::Margin::symmetric(8, 2))
                            .stroke(egui::Stroke::NONE),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if let Some(rs) = room {
                                if rs.emote_only {
                                    room_state_pill(ui, "Emote Only", t::accent());
                                }
                                if rs.subscribers_only {
                                    room_state_pill(ui, "Sub Only", t::gold());
                                }
                                if let Some(slow) = rs.slow_mode {
                                    if slow > 0 {
                                        room_state_pill(ui, &format!("Slow {slow}s"), t::yellow());
                                    }
                                }
                                if let Some(fol) = rs.followers_only {
                                    if fol >= 0 {
                                        let label = format_followers_only_label(fol);
                                        room_state_pill(ui, &label, t::text_secondary());
                                    }
                                }
                                if rs.r9k {
                                    room_state_pill(ui, "R9K", t::text_muted());
                                }
                            }
                            if let Some(viewers) = live_viewers {
                                room_state_pill(
                                    ui,
                                    &format!("Viewers {}", fmt_viewers(viewers)),
                                    t::raid_cyan(),
                                );
                            }
                        });
                    });
            }
        }

        // Pinned message strip (Twitch/Kick pinned/elevated messages).
        // Show the latest pinned message near the top of chat.
        let latest_pinned = state.channels.get(active_ch).and_then(|ch| {
            ch.messages
                .iter()
                .rev()
                .find(|m| m.flags.is_pinned && !m.flags.is_deleted)
                .map(|m| (m.sender.display_name.clone(), m.raw_text.clone()))
        });
        if let Some((sender, text)) = latest_pinned {
            TopBottomPanel::top("pinned_message_bar")
                .exact_height(24.0)
                .frame(
                    Frame::new()
                        .fill(t::warning_soft_bg())
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .stroke(egui::Stroke::new(1.0, t::gold().gamma_multiply(0.45))),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(
                            RichText::new("📌 Pinned")
                                .font(t::small())
                                .strong()
                                .color(t::gold()),
                        );
                        ui.label(RichText::new("·").font(t::small()).color(t::text_muted()));
                        ui.label(
                            RichText::new(format!("{sender}:"))
                                .font(t::small())
                                .strong()
                                .color(t::text_primary()),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(text)
                                    .font(t::small())
                                    .color(t::text_secondary()),
                            )
                            .truncate(),
                        );
                    });
                });
        }
    }
}

/// Render a tiny colored pill label (used for room-state modes in the stream bar).
fn room_state_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(t::alpha(color, 20))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.4)))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(5, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).font(t::tiny()).color(color).strong());
        });
}

fn fmt_viewers(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_followers_only_label(minutes: i32) -> String {
    if minutes <= 0 {
        return "Followers-only".to_owned();
    }

    let total = minutes as i64;
    let days = total / 1_440;
    let hours = (total % 1_440) / 60;
    let mins = total % 60;

    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 && parts.len() < 2 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 && parts.len() < 2 {
        parts.push(format!("{mins}m"));
    }
    if parts.is_empty() {
        parts.push("0m".to_owned());
    }

    format!("Followers-only {}", parts.join(" "))
}
