use std::collections::HashMap;
use std::sync::Arc;

use egui::{Color32, CornerRadius, RichText, Vec2};

use crust_core::model::{Badge, ChannelId, UserProfile};

use crate::theme as t;

// ─── Public action returned from show() ──────────────────────────────────────

/// Action emitted by the popup when a moderation button is pressed.
#[derive(Debug, Clone)]
pub enum PopupAction {
    Timeout { channel: ChannelId, login: String, user_id: String, seconds: u32, reason: Option<String> },
    Ban     { channel: ChannelId, login: String, user_id: String, reason: Option<String> },
    Unban   { channel: ChannelId, login: String, user_id: String },
}

// ─── Tab state ────────────────────────────────────────────────────────────────

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum ProfileTab {
    #[default]
    Profile,
    Moderation,
}

// ─── Struct ───────────────────────────────────────────────────────────────────

/// Floating popup showing a Twitch user's profile.  Open it by calling
/// [`UserProfilePopup::set_loading`]; it stays open until the user closes it.
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
    /// Active tab (Profile / Moderation).
    active_tab: ProfileTab,
    /// Shared reason buffer used for timeout / ban.
    mod_reason: String,
    /// Buffer for the custom timeout-duration entry.
    timeout_custom: String,
    /// Whether the ban button has been pressed once (confirmation pending).
    ban_confirm: bool,
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
        self.active_tab = ProfileTab::Profile;
        self.mod_reason.clear();
        self.timeout_custom.clear();
        self.ban_confirm = false;
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
            .min_width((ctx.screen_rect().width() - 40.0).min(360.0))
            .max_width((ctx.screen_rect().width() - 20.0).min(430.0))
            .open(&mut self.open)
            .show(ctx, |ui| {
                ui.style_mut().spacing.item_spacing = egui::vec2(4.0, 3.0);

                // ── Loading spinner ──────────────────────────────────────
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

                // ── Header: avatar + name + badges ───────────────────────
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;

                    const AV: f32 = 76.0;
                    let (av_rect, _) = ui.allocate_exact_size(
                        Vec2::splat(AV),
                        egui::Sense::hover(),
                    );

                    // Avatar: real image if loaded, else initial-letter fallback.
                    if let Some((_, _, ref raw)) = profile
                        .avatar_url
                        .as_deref()
                        .and_then(|logo| emote_bytes.get(logo))
                    {
                        let logo = profile.avatar_url.as_deref().unwrap();
                        let uri = format!("bytes://{logo}");
                        ui.painter().circle_filled(av_rect.center(), AV / 2.0, t::BG_RAISED);
                        ui.put(
                            av_rect,
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
                        let p = ui.painter();
                        p.circle_filled(av_rect.center(), AV / 2.0, t::ACCENT_DIM);
                        p.circle_stroke(av_rect.center(), AV / 2.0, egui::Stroke::new(2.0, t::ACCENT));
                        p.text(
                            av_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            initial.to_string(),
                            egui::FontId::proportional(28.0),
                            t::TEXT_PRIMARY,
                        );
                    }

                    // Live pulse dot overlay on avatar bottom-right corner
                    if profile.is_live {
                        let dot_center = egui::pos2(av_rect.max.x - 6.0, av_rect.max.y - 6.0);
                        ui.painter().circle_filled(dot_center, 7.0, t::BG_BASE);
                        ui.painter().circle_filled(dot_center, 5.0, t::RED);
                    }

                    ui.vertical(|ui| {
                        ui.add_space(2.0);

                        // Display name + LIVE / BANNED pills
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.add(egui::Label::new(
                                RichText::new(&profile.display_name)
                                    .strong()
                                    .size(16.0)
                                    .color(t::TEXT_PRIMARY),
                            ));
                            if profile.is_live {
                                egui::Frame::new()
                                    .fill(Color32::from_rgba_unmultiplied(200, 40, 40, 200))
                                    .corner_radius(t::RADIUS_SM)
                                    .inner_margin(egui::Margin::symmetric(5, 2))
                                    .show(ui, |ui| {
                                        ui.add(egui::Label::new(
                                            RichText::new("● LIVE").color(Color32::WHITE).size(10.0).strong(),
                                        ));
                                    });
                            }
                            if profile.is_banned {
                                egui::Frame::new()
                                    .fill(Color32::from_rgba_unmultiplied(200, 30, 30, 160))
                                    .corner_radius(t::RADIUS_SM)
                                    .inner_margin(egui::Margin::symmetric(5, 2))
                                    .show(ui, |ui| {
                                        ui.add(egui::Label::new(
                                            RichText::new("BANNED").color(Color32::WHITE).size(10.0).strong(),
                                        ));
                                    });
                            }
                        });

                        // @login (only if it differs from display name)
                        if profile.login.to_lowercase() != profile.display_name.to_lowercase() {
                            ui.add(egui::Label::new(
                                RichText::new(format!("@{}", profile.login))
                                    .color(t::TEXT_SECONDARY)
                                    .small(),
                            ));
                        }

                        // Role pills + chat colour
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(3.0, 2.0);
                            if profile.is_partner {
                                role_pill(ui, "✓ Partner", t::ACCENT);
                            } else if profile.is_affiliate {
                                role_pill(ui, "Affiliate", Color32::from_rgb(140, 100, 220));
                            }
                            if let Some(ref hex) = profile.chat_color {
                                if let Some(c) = parse_hex_color(hex) {
                                    egui::Frame::new()
                                        .fill(Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 35))
                                        .corner_radius(t::RADIUS_SM)
                                        .inner_margin(egui::Margin::symmetric(5, 2))
                                        .stroke(egui::Stroke::new(1.0, c))
                                        .show(ui, |ui| {
                                            ui.add(egui::Label::new(
                                                RichText::new("● Chat color").color(c).small(),
                                            ));
                                        });
                                }
                            }
                        });

                        // Badges row
                        if !self.badges.is_empty() {
                            ui.add_space(3.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                                for badge in &self.badges {
                                    let badge_name = badge_display_name(&badge.name, &badge.version);
                                    if let Some(url) = &badge.url {
                                        let uri = format!("bytes://{url}");
                                        if let Some((_, _, ref raw)) = emote_bytes.get(url.as_str()) {
                                            const BS: f32 = 18.0;
                                            let (brect, _) = ui
                                                .allocate_exact_size(Vec2::splat(BS), egui::Sense::hover());
                                            ui.put(
                                                brect,
                                                egui::Image::from_bytes(
                                                    uri,
                                                    egui::load::Bytes::Shared(raw.clone()),
                                                )
                                                .fit_to_exact_size(Vec2::splat(BS)),
                                            );
                                            if ui.rect_contains_pointer(brect) {
                                                egui::show_tooltip_text(
                                                    ui.ctx(), ui.layer_id(),
                                                    egui::Id::new(url.as_str()), &badge_name,
                                                );
                                            }
                                            continue;
                                        }
                                    }
                                    badge_text_pill(ui, &badge_name);
                                }
                            });
                        }
                    });
                });

                // ── Live stream info card ────────────────────────────────
                if profile.is_live {
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(Color32::from_rgba_unmultiplied(200, 40, 40, 18))
                        .corner_radius(t::RADIUS)
                        .stroke(egui::Stroke::new(
                            1.0, Color32::from_rgba_unmultiplied(200, 60, 60, 80),
                        ))
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            if let Some(ref title) = profile.stream_title {
                                ui.add(egui::Label::new(
                                    RichText::new(title).color(t::TEXT_PRIMARY).small().strong(),
                                ).wrap());
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                if let Some(ref game) = profile.stream_game {
                                    ui.add(egui::Label::new(
                                        RichText::new(format!("🎮 {game}")).color(t::ACCENT).small(),
                                    ));
                                }
                                if let Some(v) = profile.stream_viewers {
                                    ui.add(egui::Label::new(
                                        RichText::new(format!("👁 {}", fmt_count(v)))
                                            .color(t::TEXT_SECONDARY).small(),
                                    ));
                                }
                                if let Some(ref started) = profile.last_broadcast_at {
                                    let uptime = fmt_uptime(started);
                                    if !uptime.is_empty() {
                                        ui.add(egui::Label::new(
                                            RichText::new(format!("⏱ {uptime}"))
                                                .color(t::TEXT_SECONDARY).small(),
                                        ));
                                    }
                                }
                            });
                        });
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);

                // ── Tab bar (only rendered when the user has mod access) ──
                if self.is_mod {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (tab_label, tab_val) in [
                            ("Profile",    ProfileTab::Profile),
                            ("Moderation", ProfileTab::Moderation),
                        ] {
                            let active = self.active_tab == tab_val;
                            let fg = if active { t::TEXT_PRIMARY } else { t::TEXT_SECONDARY };
                            let bg = if active {
                                Color32::from_rgba_unmultiplied(145, 95, 255, 35)
                            } else {
                                Color32::TRANSPARENT
                            };
                            let resp = egui::Frame::new()
                                .fill(bg)
                                .corner_radius(CornerRadius::same(4))
                                .inner_margin(egui::Margin::symmetric(10, 4))
                                .show(ui, |ui| {
                                    ui.add(egui::Label::new(
                                        RichText::new(tab_label).color(fg).small().strong(),
                                    ));
                                })
                                .response;
                            if resp.interact(egui::Sense::click()).clicked() {
                                self.active_tab = tab_val;
                                if tab_val != ProfileTab::Moderation {
                                    self.ban_confirm = false;
                                }
                            }
                        }
                    });
                    ui.add_space(4.0);
                }

                // ── Profile tab ──────────────────────────────────────────
                if !self.is_mod || self.active_tab == ProfileTab::Profile {
                    egui::Grid::new("profile_stats")
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            if let Some(f) = profile.followers {
                                ui.label(RichText::new("Followers").color(t::TEXT_SECONDARY).small());
                                ui.label(RichText::new(fmt_count(f)).strong().color(t::TEXT_PRIMARY));
                                ui.end_row();
                            }
                            if let Some(ref ts) = profile.created_at {
                                ui.label(RichText::new("Account age").color(t::TEXT_SECONDARY).small());
                                ui.label(RichText::new(fmt_account_age(ts)).color(t::TEXT_PRIMARY));
                                ui.end_row();
                                ui.label(RichText::new("Joined").color(t::TEXT_SECONDARY).small());
                                ui.label(RichText::new(fmt_join_date(ts)).color(t::TEXT_PRIMARY));
                                ui.end_row();
                            }
                            if !profile.is_live {
                                if let Some(ref ts) = profile.last_broadcast_at {
                                    ui.label(RichText::new("Last live").color(t::TEXT_SECONDARY).small());
                                    ui.label(RichText::new(fmt_join_date(ts)).color(t::TEXT_SECONDARY));
                                    ui.end_row();
                                }
                            }
                        });

                    if !profile.description.is_empty() {
                        ui.add_space(5.0);
                        ui.separator();
                        ui.add_space(3.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(&profile.description).color(t::TEXT_SECONDARY).small(),
                            )
                            .wrap(),
                        );
                    }

                    if profile.is_banned {
                        ui.add_space(5.0);
                        egui::Frame::new()
                            .fill(Color32::from_rgba_unmultiplied(200, 30, 30, 25))
                            .corner_radius(t::RADIUS_SM)
                            .inner_margin(egui::Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                let reason = profile.ban_reason.as_deref().unwrap_or("No reason provided");
                                ui.add(egui::Label::new(
                                    RichText::new(format!("⚠ Suspended: {reason}"))
                                        .color(t::RED).small(),
                                ).wrap());
                            });
                    }
                }

                // ── Moderation tab ───────────────────────────────────────
                if self.is_mod && self.active_tab == ProfileTab::Moderation {
                    if let Some(ref channel) = self.channel.clone() {
                        let login   = profile.login.clone();
                        let user_id = profile.id.clone();

                        // ── Reason field ──────────────────────────────────
                        ui.label(RichText::new("Reason (optional)").color(t::TEXT_SECONDARY).small());
                        ui.add_space(2.0);
                        ui.add_sized(
                            [ui.available_width(), 22.0],
                            egui::TextEdit::singleline(&mut self.mod_reason)
                                .hint_text("e.g. spam, hate speech…")
                                .font(t::small()),
                        );
                        ui.add_space(6.0);

                        // ── Timeout presets ───────────────────────────────
                        section_label(ui, "Timeout");
                        ui.add_space(3.0);
                        // Two rows of 6 preset buttons
                        egui::Grid::new("timeout_presets_row1")
                            .num_columns(6)
                            .spacing([4.0, 4.0])
                            .show(ui, |ui| {
                                const ROW1: &[(&str, u32)] = &[
                                    ("1s",  1), ("30s", 30), ("1m", 60),
                                    ("5m",  300), ("10m", 600), ("30m", 1_800),
                                ];
                                for &(lbl, secs) in ROW1 {
                                    if timeout_btn(ui, lbl) {
                                        action = Some(PopupAction::Timeout {
                                            channel: channel.clone(),
                                            login: login.clone(),
                                            user_id: user_id.clone(),
                                            seconds: secs,
                                            reason: reason_opt(&self.mod_reason),
                                        });
                                    }
                                }
                                ui.end_row();
                                const ROW2: &[(&str, u32)] = &[
                                    ("1h",   3_600), ("8h",  28_800), ("24h", 86_400),
                                    ("7d", 604_800), ("14d", 1_209_600),
                                ];
                                for &(lbl, secs) in ROW2 {
                                    if timeout_btn(ui, lbl) {
                                        action = Some(PopupAction::Timeout {
                                            channel: channel.clone(),
                                            login: login.clone(),
                                            user_id: user_id.clone(),
                                            seconds: secs,
                                            reason: reason_opt(&self.mod_reason),
                                        });
                                    }
                                }
                            });

                        // Custom timeout
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
                            let go_btn = ui.add_sized(
                                [38.0, 20.0],
                                egui::Button::new(RichText::new("Go").small()),
                            );
                            if pressed || go_btn.clicked() {
                                if let Ok(secs) = self.timeout_custom.trim().parse::<u32>() {
                                    if secs > 0 {
                                        action = Some(PopupAction::Timeout {
                                            channel: channel.clone(),
                                            login: login.clone(),
                                            user_id: user_id.clone(),
                                            seconds: secs,
                                            reason: reason_opt(&self.mod_reason),
                                        });
                                        self.timeout_custom.clear();
                                    }
                                }
                            }
                        });

                        ui.add_space(8.0);

                        // ── Lift restriction ──────────────────────────────
                        section_label(ui, "Lift restriction");
                        ui.add_space(3.0);
                        if ui.add(
                            egui::Button::new(
                                RichText::new("↩ Untimeout / Unban").small().color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgba_unmultiplied(40, 180, 90, 200))
                            .min_size(egui::vec2(160.0, 24.0)),
                        ).clicked() {
                            action = Some(PopupAction::Unban {
                                channel: channel.clone(),
                                login: login.clone(),
                                user_id: user_id.clone(),
                            });
                        }

                        ui.add_space(8.0);

                        // ── Permanent ban (two-click confirm) ─────────────
                        section_label(ui, "Permanent ban");
                        ui.add_space(3.0);
                        if self.ban_confirm {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let confirm = ui.add(
                                    egui::Button::new(
                                        RichText::new("⚠ Confirm ban").small().color(Color32::WHITE),
                                    )
                                    .fill(Color32::from_rgba_unmultiplied(200, 30, 30, 230))
                                    .min_size(egui::vec2(120.0, 24.0)),
                                );
                                let cancel = ui.add(
                                    egui::Button::new(RichText::new("Cancel").small())
                                        .min_size(egui::vec2(60.0, 24.0)),
                                );
                                if confirm.clicked() {
                                    action = Some(PopupAction::Ban {
                                        channel: channel.clone(),
                                        login: login.clone(),
                                        user_id: user_id.clone(),
                                        reason: reason_opt(&self.mod_reason),
                                    });
                                    self.ban_confirm = false;
                                }
                                if cancel.clicked() {
                                    self.ban_confirm = false;
                                }
                            });
                        } else if ui.add(
                            egui::Button::new(
                                RichText::new("🚫 Ban user").small().color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgba_unmultiplied(180, 40, 40, 180))
                            .min_size(egui::vec2(100.0, 24.0)),
                        ).clicked() {
                            self.ban_confirm = true;
                        }
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(3.0);

                // ── Footer ───────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    ui.hyperlink_to(
                        RichText::new("View on Twitch ↗").small().color(t::ACCENT),
                        format!("https://twitch.tv/{}", profile.login),
                    );
                    let logs_url = format!(
                        "https://logs.ivr.fi/?channel={}&username={}",
                        self.channel.as_ref().map(|c| c.as_str()).unwrap_or(""),
                        profile.login,
                    );
                    ui.hyperlink_to(
                        RichText::new("Lookup logs ↗").small().color(t::TEXT_SECONDARY),
                        logs_url,
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
                RichText::new(name).color(Color32::from_rgb(210, 210, 220)).size(10.0),
            ));
        });
}

/// Section heading inside the mod panel.
fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(
        RichText::new(text).small().strong().color(t::TEXT_SECONDARY),
    ));
}

/// Render a small timeout-preset button; returns true if clicked.
fn timeout_btn(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).small())
            .fill(Color32::from_rgba_unmultiplied(200, 140, 30, 60))
            .min_size(egui::vec2(36.0, 20.0)),
    )
    .clicked()
}

/// Convert the reason buffer to `Option<String>` (None if empty).
fn reason_opt(buf: &str) -> Option<String> {
    let s = buf.trim();
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

/// Parse a CSS hex color string (`#RRGGBB`) into an egui Color32.
fn parse_hex_color(hex: &str) -> Option<Color32> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 { return None; }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

/// Convert a badge name + version to a human-readable label.
fn badge_display_name(name: &str, version: &str) -> String {
    match name {
        "subscriber"  => match version.parse::<u32>().unwrap_or(0) {
            0 => "Subscriber".to_owned(),
            m => format!("{m}-month Sub"),
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
            let s = other.replace('_', " ");
            let mut c = s.chars();
            match c.next() {
                None    => String::new(),
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

/// Format an ISO 8601 timestamp as "Month YYYY".
fn fmt_join_date(ts: &str) -> String {
    let parts: Vec<&str> = ts.splitn(3, '-').collect();
    if parts.len() < 2 { return ts.to_owned(); }
    let year  = parts[0];
    let month = match parts[1] {
        "01" => "January",   "02" => "February", "03" => "March",
        "04" => "April",     "05" => "May",       "06" => "June",
        "07" => "July",      "08" => "August",    "09" => "September",
        "10" => "October",   "11" => "November",  "12" => "December",
        m => m,
    };
    format!("{month} {year}")
}

/// Compute account age ("X years, Ymo") from an ISO 8601 timestamp.
fn fmt_account_age(ts: &str) -> String {
    let Some(unix) = iso_to_unix_secs(ts) else { return String::new(); };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(unix);
    let years  = secs / 31_557_600;
    let months = (secs % 31_557_600) / 2_629_800;
    match years {
        0 => format!("{months} months"),
        1 => if months == 0 { "1 year".to_owned() } else { format!("1 year, {months}mo") },
        n => if months == 0 { format!("{n} years") } else { format!("{n} years, {months}mo") },
    }
}

/// Format an active stream as a live uptime string ("1h 23m", "45m").
fn fmt_uptime(ts: &str) -> String {
    let Some(started) = iso_to_unix_secs(ts) else { return String::new(); };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(started);
    if elapsed == 0 { return String::new(); }
    let h = elapsed / 3600;
    let m = (elapsed % 3600) / 60;
    if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

/// Parse an ISO 8601 UTC datetime into Unix seconds.
/// Handles `"YYYY-MM-DDTHH:MM:SSZ"`, `"…+00:00"`, and fractional seconds.
fn iso_to_unix_secs(ts: &str) -> Option<u64> {
    let ts = ts.trim_end_matches('Z');
    let ts = match ts.rfind('+') {
        Some(i) if i >= 10 => &ts[..i],
        _ => ts,
    };
    let t_idx = ts.find('T')?;
    let date_part = &ts[..t_idx];
    let time_part = &ts[t_idx + 1..];

    let dp: Vec<&str> = date_part.splitn(4, '-').collect();
    if dp.len() < 3 { return None; }
    let year:  i64 = dp[0].parse().ok()?;
    let month: i64 = dp[1].parse().ok()?;
    let day:   i64 = dp[2].parse().ok()?;

    let time_clean = time_part.splitn(2, '.').next().unwrap_or(time_part);
    let tp: Vec<&str> = time_clean.splitn(4, ':').collect();
    if tp.len() < 3 { return None; }
    let hour: u64 = tp[0].parse().ok()?;
    let min:  u64 = tp[1].parse().ok()?;
    let sec:  u64 = tp[2].parse().ok()?;

    // Gregorian date → Julian Day Number → Unix days
    let a   = (14 - month) / 12;
    let yy  = year - a;
    let mm  = month + 12 * a - 3;
    let jdn = day + (153 * mm + 2) / 5 + 365 * yy + yy / 4 - yy / 100 + yy / 400 - 32_045;
    let unix_days = jdn - 2_440_588; // JDN of 1970-01-01
    if unix_days < 0 { return None; }
    Some(unix_days as u64 * 86_400 + hour * 3_600 + min * 60 + sec)
}
