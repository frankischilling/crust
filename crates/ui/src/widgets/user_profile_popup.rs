use std::collections::HashMap;
use std::sync::Arc;

use egui::{Color32, CornerRadius, RichText, Vec2};

use crust_core::model::{Badge, ChannelId, UserProfile};

use crate::theme as t;

// ─── Public action returned from show() ──────────────────────────────────────

/// Action emitted by the popup when a moderation button is pressed.
#[derive(Debug, Clone)]
pub enum PopupAction {
    Timeout { channel: ChannelId, login: String, seconds: u32 },
    Ban     { channel: ChannelId, login: String },
}

/// Floating popup showing a Twitch user's profile, similar to Chatterino's
/// user-card.  Open it by calling [`UserProfilePopup::set_loading`]; it stays
/// open until the user closes it.
#[derive(Default)]
pub struct UserProfilePopup {
    pub open: bool,
    profile: Option<UserProfile>,
    /// Show a spinner while the network request is in flight.
    loading: bool,
    /// Login name of whoever is being fetched (shown while loading).
    loading_login: String,
    /// Badges from the most-recently clicked message sender.
    badges: Vec<Badge>,
    /// Channel the sender was seen in (used for mod actions).
    channel: Option<ChannelId>,
    /// Whether the logged-in user is a moderator in that channel.
    is_mod: bool,
    /// Buffer for the custom timeout-duration entry.
    timeout_custom: String,
}

impl UserProfilePopup {
    /// Begin a loading state for `login` (called as soon as the click happens).
    pub fn set_loading(
        &mut self,
        login: &str,
        badges: Vec<Badge>,
        channel: Option<ChannelId>,
        is_mod: bool,
    ) {
        self.loading_login = login.to_owned();
        self.loading = true;
        self.open = true;
        self.profile = None;
        self.badges = badges;
        self.channel = channel;
        self.is_mod = is_mod;
        self.timeout_custom.clear();
    }

    /// Called when `AppEvent::UserProfileLoaded` arrives.
    pub fn set_profile(&mut self, profile: UserProfile) {
        self.loading = false;
        self.open = true;
        self.profile = Some(profile);
    }

    /// Render the popup, returning any moderation action the user triggered.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
    ) -> Option<PopupAction> {
        if !self.open {
            return None;
        }

        let title = if self.loading {
            format!("@{}", self.loading_login)
        } else if let Some(ref p) = self.profile {
            format!("@{}", p.login)
        } else {
            "User Profile".to_owned()
        };

        let mut action: Option<PopupAction> = None;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .min_width(300.0)
            .max_width(360.0)
            .open(&mut self.open)
            .show(ctx, |ui| {
                if self.loading {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("Loading profile…")
                                .color(t::TEXT_SECONDARY)
                                .italics(),
                        );
                    });
                    ui.add_space(20.0);
                    return;
                }

                let Some(ref profile) = self.profile else {
                    ui.add_space(8.0);
                    ui.label(RichText::new("Profile unavailable.").color(t::RED));
                    ui.add_space(8.0);
                    return;
                };

                ui.add_space(6.0);

                // ── Header: avatar + name + badges ────────────────────────
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;

                    const AV: f32 = 64.0;
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::splat(AV),
                        egui::Sense::hover(),
                    );

                    // Avatar: real image if loaded, else initial-letter fallback.
                    let avatar_drawn = profile.avatar_url.as_deref().and_then(|logo| {
                        emote_bytes.get(logo)
                    });

                    if let Some((_, _, ref raw)) = avatar_drawn {
                        let logo = profile.avatar_url.as_deref().unwrap();
                        let uri = format!("bytes://{logo}");
                        ui.painter().circle_filled(
                            rect.center(), AV / 2.0, t::BG_RAISED,
                        );
                        ui.put(
                            rect,
                            egui::Image::from_bytes(
                                uri,
                                egui::load::Bytes::Shared(raw.clone()),
                            )
                            .fit_to_exact_size(Vec2::splat(AV))
                            .corner_radius(CornerRadius::same(AV as u8 / 2)),
                        );
                    } else {
                        let initial = profile
                            .display_name
                            .chars()
                            .next()
                            .and_then(|c| c.to_uppercase().next())
                            .unwrap_or('?');
                        let painter = ui.painter();
                        painter.circle_filled(rect.center(), AV / 2.0, t::ACCENT_DIM);
                        painter.circle_stroke(
                            rect.center(),
                            AV / 2.0,
                            egui::Stroke::new(2.0, t::ACCENT),
                        );
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            initial.to_string(),
                            egui::FontId::proportional(26.0),
                            t::TEXT_PRIMARY,
                        );
                    }

                    ui.vertical(|ui| {
                        ui.add_space(4.0);

                        // Display name
                        ui.add(egui::Label::new(
                            RichText::new(&profile.display_name)
                                .strong()
                                .size(16.0)
                                .color(t::TEXT_PRIMARY),
                        ));

                        // @login (only if it differs from the display name)
                        if profile.login.to_lowercase()
                            != profile.display_name.to_lowercase()
                        {
                            ui.add(egui::Label::new(
                                RichText::new(format!("@{}", profile.login))
                                    .color(t::TEXT_SECONDARY)
                                    .small(),
                            ));
                        }

                        // Role pill
                        ui.add_space(2.0);
                        if profile.is_partner {
                            role_pill(ui, "✓ Partner", t::ACCENT);
                        } else if profile.is_affiliate {
                            role_pill(
                                ui,
                                "Affiliate",
                                Color32::from_rgb(140, 100, 220),
                            );
                        }

                        // ── Badges row ────────────────────────────────────
                        if !self.badges.is_empty() {
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                                for badge in &self.badges {
                                    let badge_name = badge_display_name(
                                        &badge.name,
                                        &badge.version,
                                    );
                                    if let Some(url) = &badge.url {
                                        let uri = format!("bytes://{url}");
                                        if let Some((_, _, ref raw)) = emote_bytes.get(url.as_str()) {
                                            const BS: f32 = 18.0;
                                            let (brect, _) = ui.allocate_exact_size(
                                                Vec2::splat(BS),
                                                egui::Sense::hover(),
                                            );
                                            ui.put(
                                                brect,
                                                egui::Image::from_bytes(
                                                    uri,
                                                    egui::load::Bytes::Shared(raw.clone()),
                                                )
                                                .fit_to_exact_size(Vec2::splat(BS)),
                                            );
                                            // tooltip
                                            if ui.rect_contains_pointer(brect) {
                                                egui::show_tooltip_text(
                                                    ui.ctx(),
                                                    ui.layer_id(),
                                                    egui::Id::new(url.as_str()),
                                                    &badge_name,
                                                );
                                            }
                                            continue;
                                        }
                                    }
                                    // Fallback: text pill
                                    badge_text_pill(ui, &badge_name);
                                }
                            });
                        }
                    });
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                // ── Stats grid ───────────────────────────────────────────
                egui::Grid::new("profile_stats")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        if let Some(f) = profile.followers {
                            ui.label(
                                RichText::new("Followers")
                                    .color(t::TEXT_SECONDARY)
                                    .small(),
                            );
                            ui.label(
                                RichText::new(fmt_count(f))
                                    .strong()
                                    .color(t::TEXT_PRIMARY),
                            );
                            ui.end_row();
                        }
                        if let Some(ref ts) = profile.created_at {
                            ui.label(
                                RichText::new("Account age")
                                    .color(t::TEXT_SECONDARY)
                                    .small(),
                            );
                            ui.label(
                                RichText::new(fmt_account_age(ts))
                                    .color(t::TEXT_PRIMARY),
                            );
                            ui.end_row();

                            ui.label(
                                RichText::new("Joined")
                                    .color(t::TEXT_SECONDARY)
                                    .small(),
                            );
                            ui.label(
                                RichText::new(fmt_join_date(ts))
                                    .color(t::TEXT_PRIMARY),
                            );
                            ui.end_row();
                        }
                    });

                // ── Bio ──────────────────────────────────────────────────
                if !profile.description.is_empty() {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&profile.description)
                                .color(t::TEXT_SECONDARY)
                                .small(),
                        )
                        .wrap(),
                    );
                }

                // ── Moderation panel ─────────────────────────────────────
                if self.is_mod {
                    if let Some(ref channel) = self.channel.clone() {
                        let login = profile.login.clone();

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("🛡 Moderation")
                                    .small()
                                    .strong()
                                    .color(Color32::from_rgb(220, 160, 50)),
                            );
                        });

                        ui.add_space(4.0);

                        // Timeout presets ─────────────────────────────────
                        ui.label(
                            RichText::new("Timeout").color(t::TEXT_SECONDARY).small(),
                        );
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                            const PRESETS: &[(&str, u32)] = &[
                                ("1m",  60),
                                ("10m", 600),
                                ("30m", 1_800),
                                ("1h",  3_600),
                                ("24h", 86_400),
                                ("7d",  604_800),
                            ];
                            for (label, secs) in PRESETS {
                                if timeout_btn(ui, label) {
                                    action = Some(PopupAction::Timeout {
                                        channel: channel.clone(),
                                        login: login.clone(),
                                        seconds: *secs,
                                    });
                                }
                            }
                        });

                        // Custom timeout ───────────────────────────────────
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            let resp = ui.add_sized(
                                [80.0, 20.0],
                                egui::TextEdit::singleline(&mut self.timeout_custom)
                                    .hint_text("custom (s)")
                                    .font(t::small()),
                            );
                            let pressed = resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let custom_btn = ui.add_sized(
                                [40.0, 20.0],
                                egui::Button::new(
                                    RichText::new("Go").small(),
                                ),
                            );
                            if pressed || custom_btn.clicked() {
                                if let Ok(secs) = self.timeout_custom.trim().parse::<u32>() {
                                    if secs > 0 {
                                        action = Some(PopupAction::Timeout {
                                            channel: channel.clone(),
                                            login: login.clone(),
                                            seconds: secs,
                                        });
                                        self.timeout_custom.clear();
                                    }
                                }
                            }
                        });

                        ui.add_space(6.0);

                        // Ban ──────────────────────────────────────────────
                        ui.label(
                            RichText::new("Permanent ban").color(t::TEXT_SECONDARY).small(),
                        );
                        ui.add_space(2.0);
                        let ban_btn = ui.add(
                            egui::Button::new(
                                RichText::new("🚫 Ban").small().color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgba_unmultiplied(180, 40, 40, 220))
                            .min_size(egui::vec2(70.0, 22.0)),
                        );
                        if ban_btn.clicked() {
                            action = Some(PopupAction::Ban {
                                channel: channel.clone(),
                                login: login.clone(),
                            });
                        }
                    }
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);

                // ── Actions ──────────────────────────────────────────────
                ui.horizontal(|ui| {
                    let twitch_url =
                        format!("https://twitch.tv/{}", profile.login);
                    ui.hyperlink_to(
                        RichText::new("View on Twitch ↗")
                            .small()
                            .color(t::ACCENT),
                        &twitch_url,
                    );
                });

                ui.add_space(4.0);
            });

        // Close the popup after a mod action so the user doesn't have to.
        if action.is_some() {
            self.open = false;
        }
        action
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Render a small inline role pill using egui Frame.
fn role_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(5, 2))
        .show(ui, |ui| {
            ui.add(egui::Label::new(
                RichText::new(text).color(color).small().strong(),
            ));
        });
}

/// Render a small text badge pill (fallback when image is unavailable).
fn badge_text_pill(ui: &mut egui::Ui, name: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(80, 80, 100, 140))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.add(egui::Label::new(
                RichText::new(name)
                    .color(Color32::from_rgb(210, 210, 220))
                    .size(10.0),
            ));
        });
}

/// Render a small timeout-preset button; returns true if clicked.
fn timeout_btn(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).small())
            .fill(Color32::from_rgba_unmultiplied(200, 140, 30, 60))
            .min_size(egui::vec2(34.0, 20.0)),
    )
    .clicked()
}

/// Convert a badge name + version to a human-readable label.
fn badge_display_name(name: &str, version: &str) -> String {
    match name {
        "subscriber"  => match version.parse::<u32>().unwrap_or(0) {
            0        => "Subscriber".to_owned(),
            m        => format!("{m}-month Sub"),
        },
        "moderator"   => "Moderator".to_owned(),
        "broadcaster" => "Broadcaster".to_owned(),
        "vip"         => "VIP".to_owned(),
        "staff"       => "Staff".to_owned(),
        "admin"       => "Admin".to_owned(),
        "global_mod"  => "Global Mod".to_owned(),
        "partner"     => "Partner".to_owned(),
        "bits"        => {
            let val: u64 = version.parse().unwrap_or(0);
            if val > 0 { format!("{val} Bits") } else { "Bits".to_owned() }
        }
        other => {
            // Capitalise first letter, replace underscores with spaces.
            let s = other.replace('_', " ");
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        }
    }
}

/// Format a large number in a human-readable way: 1.2M, 34.5K, etc.
fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format an ISO 8601 timestamp as "Month YYYY" for display.
fn fmt_join_date(ts: &str) -> String {
    let parts: Vec<&str> = ts.splitn(3, '-').collect();
    if parts.len() < 2 {
        return ts.to_owned();
    }
    let year = parts[0];
    let month = match parts[1] {
        "01" => "January",   "02" => "February", "03" => "March",
        "04" => "April",     "05" => "May",       "06" => "June",
        "07" => "July",      "08" => "August",    "09" => "September",
        "10" => "October",   "11" => "November",  "12" => "December",
        m => m,
    };
    format!("{month} {year}")
}

/// Compute a rough "X years ago" age from an ISO 8601 timestamp.
fn fmt_account_age(ts: &str) -> String {
    let Ok(created_year) = ts.get(..4).unwrap_or("0").parse::<u32>() else {
        return String::new();
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let current_year = 1970 + (now_secs / 31_557_600) as u32;
    let age = current_year.saturating_sub(created_year);
    match age {
        0 => "< 1 year".to_owned(),
        1 => "1 year".to_owned(),
        n => format!("{n} years"),
    }
}

