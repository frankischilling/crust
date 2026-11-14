use crate::image_cache::UiImageCache;
use std::collections::HashMap;

use egui::{Color32, CornerRadius, RichText, Vec2};

use crust_core::events::IvrLogEntry;
use crust_core::model::{Badge, ChannelId, ChatMessage, UserProfile};

use crate::theme as t;

// Public action returned from show()

/// Action emitted by the popup when a moderation button is pressed.
#[derive(Debug, Clone)]
pub enum PopupAction {
    Timeout {
        channel: ChannelId,
        login: String,
        user_id: String,
        seconds: u32,
        reason: Option<String>,
    },
    Ban {
        channel: ChannelId,
        login: String,
        user_id: String,
        reason: Option<String>,
    },
    Unban {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    Warn {
        channel: ChannelId,
        login: String,
        user_id: String,
        reason: String,
    },
    Monitor {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    Restrict {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    Unmonitor {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    Unrestrict {
        channel: ChannelId,
        login: String,
        user_id: String,
    },
    ClearUserMessagesLocally {
        channel: ChannelId,
        login: String,
    },
    /// Request IVR chat logs for the displayed user.
    FetchIvrLogs {
        channel: String,
        username: String,
    },
    /// Open a URL in the system browser.
    OpenUrl {
        url: String,
    },
    OpenModerationTools {
        channel: ChannelId,
    },
    /// Execute an arbitrary chat command (e.g. from a mod action preset).
    ExecuteCommand {
        channel: ChannelId,
        command: String,
    },
}

// Tab state

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum ProfileTab {
    #[default]
    Profile,
    Moderation,
    ModLogs,
    Logs,
    IvrLogs,
}

// Struct

/// Floating popup showing a Twitch user's profile.  Open it by calling
/// [`UserProfilePopup::set_loading`]; it stays open until the user closes it.
#[derive(Default)]
pub struct UserProfilePopup {
    pub open: bool,
    profile: Option<UserProfile>,
    original_profile_display_name: Option<String>,
    /// Show a spinner while the network request is in flight.
    loading: bool,
    /// Login name of whoever is being fetched (shown while loading).
    loading_login: String,
    /// Badges from the most-recently clicked message sender.
    badges: Vec<Badge>,
    /// Channel the sender was seen in (used for mod actions).
    pub channel: Option<ChannelId>,
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
    /// Recent messages from this user in the current channel (most-recent first).
    logs: Vec<ChatMessage>,
    /// Recent moderation actions targeting this user in the current channel.
    mod_logs: Vec<ChatMessage>,
    /// External IVR chat logs fetched from logs.ivr.fi.
    ivr_logs: Vec<IvrLogEntry>,
    /// Whether IVR logs are currently being fetched.
    ivr_logs_loading: bool,
    /// Error message if IVR log fetch failed.
    ivr_logs_error: Option<String>,
    /// Whether IVR logs have been requested for the current popup session.
    ivr_logs_requested: bool,
    /// Distinguishes refreshes and reopened cards from earlier requests.
    ivr_logs_request_id: u64,
    /// Channels where this user appears in local state (chatters or recent messages).
    shared_channels: Vec<String>,
    /// Pronouns resolved via alejo.io before the profile itself arrived.
    /// Stored by lowercase login so `set_profile` can merge them in when the
    /// profile finally loads.  `Some(None)` means "fetch confirmed unspecified".
    pending_pronouns: std::collections::HashMap<String, Option<String>>,
    /// Tri-state follow-age resolution for the channel this card was opened in.
    /// `None` = not yet known, `Some(None)` = confirmed not following,
    /// `Some(Some(ts))` = following since the ISO 8601 timestamp.
    follow_age_resolved: Option<Option<String>>,
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
        self.original_profile_display_name = None;
        self.badges = badges;
        self.channel = channel;
        self.is_mod = is_mod;
        self.active_tab = ProfileTab::Profile;
        self.mod_reason.clear();
        self.timeout_custom.clear();
        self.ban_confirm = false;
        self.logs.clear();
        self.mod_logs.clear();
        self.ivr_logs.clear();
        self.ivr_logs_loading = false;
        self.ivr_logs_error = None;
        self.ivr_logs_requested = false;
        self.shared_channels.clear();
        self.follow_age_resolved = None;
    }

    fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.loading_login.clear();
        self.ivr_logs_loading = false;
        self.ban_confirm = false;
    }

    /// Apply a silent Helix follow-age lookup result to the popup. Only
    /// applied when the popup is still showing the same login + channel.
    pub fn set_follow_age_resolved(
        &mut self,
        login: &str,
        channel: &ChannelId,
        followed_at: Option<String>,
    ) {
        let channel_matches = self.channel.as_ref().map(|c| c == channel).unwrap_or(false);
        if !channel_matches {
            return;
        }
        let login_matches = match self.profile.as_ref() {
            Some(p) => p.login.eq_ignore_ascii_case(login),
            None => self.loading_login.eq_ignore_ascii_case(login),
        };
        if !login_matches {
            return;
        }
        if let Some(ref ts) = followed_at {
            if let Some(p) = self.profile.as_mut() {
                if p.followed_at.is_none() {
                    p.followed_at = Some(ts.clone());
                }
            }
        }
        self.follow_age_resolved = Some(followed_at);
    }

    /// Store pre-filtered chat logs for this user (called from `app.rs` when
    /// the profile is loaded).  Expects messages in most-recent-first order.
    pub fn set_logs(&mut self, logs: Vec<ChatMessage>) {
        self.logs = logs;
    }

    /// Store pre-filtered moderation actions targeting this user.
    pub fn set_mod_logs(&mut self, logs: Vec<ChatMessage>) {
        self.mod_logs = logs;
    }

    /// Store channels shared with the viewed user (computed from local state).
    pub fn set_shared_channels(&mut self, channels: Vec<String>) {
        self.shared_channels = channels;
    }

    /// Start a log request for the card that is still open.
    pub fn begin_ivr_logs_request(&mut self, channel: &str, username: &str) -> Option<u64> {
        if !self.accepts_profile(username)
            || !self.channel.as_ref().is_some_and(|current| {
                current.as_str().eq_ignore_ascii_case(channel)
            })
        {
            return None;
        }
        self.ivr_logs_request_id = self.ivr_logs_request_id.wrapping_add(1);
        self.ivr_logs_loading = true;
        self.ivr_logs_error = None;
        self.ivr_logs_requested = true;
        Some(self.ivr_logs_request_id)
    }

    /// Apply only the result of this card's current log request.
    pub fn apply_ivr_logs_result(
        &mut self,
        channel: &str,
        username: &str,
        request_id: Option<u64>,
        result: Result<Vec<IvrLogEntry>, String>,
    ) -> bool {
        if !self.ivr_logs_loading
            || request_id != Some(self.ivr_logs_request_id)
            || !self.accepts_profile(username)
            || !self.channel.as_ref().is_some_and(|current| {
                current.as_str().eq_ignore_ascii_case(channel)
            })
        {
            return false;
        }
        self.ivr_logs_loading = false;
        match result {
            Ok(logs) => {
                self.ivr_logs = logs;
                self.ivr_logs_error = None;
            }
            Err(error) => self.ivr_logs_error = Some(error),
        }
        true
    }

    /// End a pending log request when the background service cannot complete it.
    pub fn abort_ivr_logs_request(&mut self, error: &str) -> bool {
        if !self.ivr_logs_loading {
            return false;
        }
        self.ivr_logs_loading = false;
        self.ivr_logs_error = Some(error.to_owned());
        true
    }

    /// Called when `AppEvent::UserProfileLoaded` arrives.
    pub fn set_profile(&mut self, mut profile: UserProfile) {
        if !self.accepts_profile(&profile.login) {
            return;
        }
        // If a UserPronounsLoaded event arrived *before* the profile did
        // (alejo.io is usually faster than IVR/Helix), apply the stashed
        // pronouns here so the usercard shows them on the first paint.
        if profile.pronouns.is_none() {
            let key = profile.login.to_ascii_lowercase();
            if let Some(pending) = self.pending_pronouns.remove(&key) {
                tracing::info!("popup: merging pending pronouns for {key} -> {:?}", pending);
                profile.pronouns = pending;
            } else {
                tracing::info!(
                    "popup: set_profile for {key}, no pending pronouns yet (pending keys: {:?})",
                    self.pending_pronouns.keys().collect::<Vec<_>>()
                );
            }
        }
        self.loading = false;
        self.original_profile_display_name = Some(profile.display_name.clone());
        self.profile = Some(profile);
    }

    pub fn apply_nicknames(&mut self, nicknames: &[crust_core::model::Nickname]) {
        if let Some(profile) = self.profile.as_mut() {
            if let Some(original) = self.original_profile_display_name.as_ref() {
                profile.display_name.clone_from(original);
            }
            let channel = self
                .channel
                .as_ref()
                .map_or("", |channel| channel.display_name());
            crust_core::model::apply_nickname(
                nicknames,
                &profile.login,
                channel,
                &mut profile.display_name,
            );
        }
        for message in self.logs.iter_mut().chain(self.mod_logs.iter_mut()) {
            crust_core::model::apply_sender_nickname(
                nicknames,
                message.channel.display_name(),
                &mut message.sender,
            );
        }
    }

    /// Called when `AppEvent::UserPronounsLoaded` arrives.  If the popup is
    /// currently showing `login` and the profile has no pronouns yet, merge
    /// them in so the row appears without waiting on a profile refetch.
    /// If the profile hasn't loaded yet, stash the result so `set_profile`
    /// can pick it up.
    pub fn set_pronouns(&mut self, login: &str, pronouns: Option<String>) {
        let key = login.to_ascii_lowercase();
        match self.profile.as_mut() {
            Some(profile) if profile.login.eq_ignore_ascii_case(login) => {
                tracing::info!(
                    "popup: set_pronouns live merge for {login} -> {:?} (prev={:?})",
                    pronouns,
                    profile.pronouns
                );
                if profile.pronouns.is_none() {
                    profile.pronouns = pronouns;
                }
            }
            Some(other) => {
                tracing::info!(
                    "popup: set_pronouns stashed for {key} (popup is showing {:?})",
                    other.login
                );
                self.pending_pronouns.insert(key, pronouns);
            }
            None => {
                tracing::info!("popup: set_pronouns stashed for {key} (no profile loaded yet)");
                self.pending_pronouns.insert(key, pronouns);
            }
        }
    }

    /// Called when a profile request completes without data.
    pub fn set_unavailable(&mut self, login: &str) {
        if !self.accepts_profile(login) {
            return;
        }
        self.loading = false;
        self.loading_login = login.to_owned();
        self.profile = None;
        self.original_profile_display_name = None;
    }

    /// Returns true when this popup is currently expecting a profile for
    /// `login` (either while loading or while refreshing an already-open card).
    pub fn accepts_profile(&self, login: &str) -> bool {
        if !self.open {
            return false;
        }
        if self.loading {
            return self.loading_login.eq_ignore_ascii_case(login);
        }
        self.profile
            .as_ref()
            .is_some_and(|p| p.login.eq_ignore_ascii_case(login))
    }

    /// The Twitch user-id of the currently displayed profile (if loaded).
    pub fn profile_id(&self) -> Option<&str> {
        self.profile.as_ref().map(|p| p.id.as_str())
    }

    fn popup_title(&self) -> String {
        if self.loading {
            format!("@{}", self.loading_login)
        } else if let Some(ref p) = self.profile {
            format!("@{}", p.login)
        } else {
            "User Profile".to_owned()
        }
    }

    fn current_channel_name(&self) -> String {
        self.channel
            .as_ref()
            .map(|c| c.as_str().to_owned())
            .unwrap_or_default()
    }

    fn render_loading_state(ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Loading profile...")
                    .color(t::text_secondary())
                    .italics(),
            );
        });
        ui.add_space(20.0);
    }

    fn render_unavailable_state(ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("Profile unavailable.").color(t::red()));
        ui.add_space(8.0);
    }

    fn render_profile_header(
        &self,
        ui: &mut egui::Ui,
        profile: &UserProfile,
        emote_bytes: &UiImageCache,
        stv_avatars: &HashMap<String, String>,
    ) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;

            const AV: f32 = 76.0;
            let (av_rect, _) = ui.allocate_exact_size(Vec2::splat(AV), egui::Sense::hover());

            let stv_av = stv_avatars.get(&profile.id);
            let avatar_entry = stv_av
                .and_then(|url| emote_bytes.get(url.as_str()).map(|e| (url.as_str(), e)))
                .or_else(|| {
                    profile
                        .avatar_url
                        .as_deref()
                        .and_then(|url| emote_bytes.get(url).map(|e| (url, e)))
                });

            if let Some((logo, (_, _, ref raw))) = avatar_entry {
                let uri = super::bytes_uri(logo, raw.as_ref());
                ui.painter()
                    .circle_filled(av_rect.center(), AV / 2.0, t::bg_raised());
                ui.put(
                    av_rect,
                    egui::Image::from_bytes(uri, egui::load::Bytes::Shared(raw.clone()))
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
                p.circle_filled(av_rect.center(), AV / 2.0, t::accent_dim());
                p.circle_stroke(
                    av_rect.center(),
                    AV / 2.0,
                    egui::Stroke::new(2.0_f32, t::accent()),
                );
                p.text(
                    av_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    initial.to_string(),
                    t::usercard_font(),
                    t::text_primary(),
                );
            }

            if profile.is_live {
                let dot_center = egui::pos2(av_rect.max.x - 6.0, av_rect.max.y - 6.0);
                ui.painter().circle_filled(dot_center, 7.0, t::bg_base());
                ui.painter().circle_filled(dot_center, 5.0, t::red());
            }

            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width());
                ui.add_space(2.0);

                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(&profile.display_name)
                                .strong()
                                .size((t::usercard_font_size() * 0.57).max(10.0))
                                .color(t::text_primary()),
                        )
                        .wrap(),
                    );
                    if profile.is_live {
                        let frame = egui::Frame::new()
                            .fill(t::danger_strong_bg())
                            .corner_radius(t::RADIUS_SM)
                            .inner_margin(egui::Margin::symmetric(5, 2));
                        wrapped_text_frame(
                            ui,
                            frame,
                            RichText::new("● LIVE")
                                .color(t::text_on_accent())
                                .size((t::usercard_font_size() * 0.36).max(8.0))
                                .strong(),
                        );
                    }
                    if profile.is_banned {
                        let frame = egui::Frame::new()
                            .fill(t::danger_strong_bg().gamma_multiply(0.9))
                            .corner_radius(t::RADIUS_SM)
                            .inner_margin(egui::Margin::symmetric(5, 2));
                        wrapped_text_frame(
                            ui,
                            frame,
                            RichText::new("BANNED")
                                .color(t::text_on_accent())
                                .size((t::usercard_font_size() * 0.36).max(8.0))
                                .strong(),
                        );
                    }
                });

                if profile.login.to_lowercase() != profile.display_name.to_lowercase() {
                    ui.add(egui::Label::new(
                        RichText::new(format!("@{}", profile.login))
                            .color(t::text_secondary())
                            .small(),
                    ));
                }

                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(3.0, 2.0);
                    if profile.is_partner {
                        role_pill(ui, "Partner", t::accent());
                    } else if profile.is_affiliate {
                        role_pill(ui, "Affiliate", t::text_secondary());
                    }
                    if let Some(ref hex) = profile.chat_color {
                        if let Some(c) = parse_hex_color(hex) {
                            let frame = egui::Frame::new()
                                .fill(t::alpha(c, 35))
                                .corner_radius(t::RADIUS_SM)
                                .inner_margin(egui::Margin::symmetric(5, 2))
                                .stroke(egui::Stroke::new(1.0_f32, c));
                            wrapped_text_frame(
                                ui,
                                frame,
                                RichText::new("● Chat color").color(c).small(),
                            );
                        }
                    }
                    for role in badge_role_labels(&self.badges) {
                        role_pill(ui, &role, t::text_secondary());
                    }
                });

                if !self.badges.is_empty() {
                    ui.add_space(3.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                        for badge in &self.badges {
                            let badge_name = badge_display_name(&badge.name, &badge.version);
                            if let Some(url) = &badge.url {
                                if let Some((_, _, ref raw)) = emote_bytes.get(url.as_str()) {
                                    let uri = super::bytes_uri(url, raw.as_ref());
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
                                            ui.ctx(),
                                            ui.layer_id(),
                                            egui::Id::new(url.as_str()),
                                            &badge_name,
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
    }

    fn render_quick_actions(
        &self,
        ui: &mut egui::Ui,
        profile: &UserProfile,
        actions: &mut Vec<PopupAction>,
    ) {
        let display_differs = !profile
            .display_name
            .eq_ignore_ascii_case(profile.login.as_str());

        egui::Frame::new()
            .fill(t::bg_card())
            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle()))
            .corner_radius(t::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(6, 5))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

                    if small_action_btn(ui, "Copy login") {
                        ui.ctx().copy_text(profile.login.clone());
                    }
                    if display_differs && small_action_btn(ui, "Copy display") {
                        ui.ctx().copy_text(profile.display_name.clone());
                    }
                    if small_action_btn(ui, "Copy user ID") {
                        ui.ctx().copy_text(profile.id.clone());
                    }
                    if let Some(url) = profile.avatar_url.as_ref() {
                        if small_action_btn(ui, "Open avatar") {
                            actions.push(PopupAction::OpenUrl { url: url.clone() });
                        }
                    }
                });
            });
    }

    fn render_live_info(&self, ui: &mut egui::Ui, profile: &UserProfile) {
        if !profile.is_live {
            return;
        }

        ui.add_space(6.0);
        egui::Frame::new()
            .fill(t::danger_soft_bg())
            .corner_radius(t::RADIUS)
            .stroke(egui::Stroke::new(1.0_f32, t::alpha(t::red(), 80)))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                if let Some(ref title) = profile.stream_title {
                    ui.add(
                        egui::Label::new(
                            RichText::new(title)
                                .color(t::text_primary())
                                .small()
                                .strong(),
                        )
                        .wrap(),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if let Some(ref game) = profile.stream_game {
                        ui.add(egui::Label::new(
                            RichText::new(format!("🎮 {game}"))
                                .color(t::accent())
                                .small(),
                        ));
                    }
                    if let Some(v) = profile.stream_viewers {
                        ui.add(egui::Label::new(
                            RichText::new(format!("👁 {}", fmt_count(v)))
                                .color(t::text_secondary())
                                .small(),
                        ));
                    }
                    if let Some(ref started) = profile.last_broadcast_at {
                        let uptime = fmt_uptime(started);
                        if !uptime.is_empty() {
                            ui.add(egui::Label::new(
                                RichText::new(format!("⏱ {uptime}"))
                                    .color(t::text_secondary())
                                    .small(),
                            ));
                        }
                    }
                });
            });
    }

    fn render_tab_bar(
        &mut self,
        ui: &mut egui::Ui,
        profile: &UserProfile,
        actions: &mut Vec<PopupAction>,
    ) {
        let mut tabs: Vec<(&str, ProfileTab)> = vec![("Profile", ProfileTab::Profile)];
        tabs.push(("Logs", ProfileTab::Logs));
        if self.is_mod {
            tabs.push(("Moderation", ProfileTab::Moderation));
            tabs.push(("Mod Logs", ProfileTab::ModLogs));
        }
        tabs.push(("IVR Logs", ProfileTab::IvrLogs));

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (tab_label, tab_val) in &tabs {
                let active = self.active_tab == *tab_val;
                let fg = if active {
                    t::text_primary()
                } else {
                    t::text_secondary()
                };
                let bg = if active {
                    t::bg_raised()
                } else {
                    Color32::TRANSPARENT
                };
                let frame = egui::Frame::new()
                    .fill(bg)
                    .corner_radius(CornerRadius::same(4))
                    .stroke(if active {
                        egui::Stroke::new(1.0_f32, t::border_subtle())
                    } else {
                        egui::Stroke::NONE
                    })
                    .inner_margin(egui::Margin::symmetric(10, 4));
                let resp = wrapped_text_frame(
                    ui,
                    frame,
                    RichText::new(*tab_label).color(fg).small().strong(),
                );
                if resp.interact(egui::Sense::click()).clicked() {
                    self.active_tab = *tab_val;
                    self.ban_confirm = false;
                    if *tab_val == ProfileTab::IvrLogs && !self.ivr_logs_requested {
                        actions.push(PopupAction::FetchIvrLogs {
                            channel: self.current_channel_name(),
                            username: profile.login.clone(),
                        });
                    }
                }
            }
        });
        ui.add_space(4.0);
    }

    fn render_logs_tab(&self, ui: &mut egui::Ui) {
        let log_count = self.logs.len();
        egui::Frame::new()
            .fill(t::scrim_bg())
            .corner_radius(t::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(if log_count == 0 {
                        "No messages from this user in the current session.".to_owned()
                    } else {
                        format!(
                            "{log_count} message{} (current session, newest first)",
                            if log_count == 1 { "" } else { "s" }
                        )
                    })
                    .color(t::text_muted())
                    .small(),
                );
            });

        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("user_logs_scroll")
            .max_height(320.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.logs.is_empty() {
                    ui.add_space(8.0);
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("No recent messages.")
                                .color(t::text_muted())
                                .small(),
                        );
                    });
                }
                for msg in &self.logs {
                    let time_str = msg.timestamp.format("%H:%M:%S").to_string();
                    let text = chat_message_text(msg);

                    let is_action = msg.flags.is_action;
                    let is_deleted = msg.flags.is_deleted;

                    egui::Frame::new()
                        .fill(if is_deleted {
                            t::alpha(t::red(), 20)
                        } else {
                            Color32::TRANSPARENT
                        })
                        .inner_margin(egui::Margin::symmetric(4, 2))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.add(egui::Label::new(
                                    RichText::new(&time_str)
                                        .color(t::text_muted())
                                        .small()
                                        .monospace(),
                                ));
                                let msg_color = if is_deleted {
                                    t::text_muted()
                                } else if is_action {
                                    t::alpha(t::mention(), 230)
                                } else {
                                    t::text_primary()
                                };
                                let rich = RichText::new(&text).color(msg_color).small();
                                let rich = if is_action || is_deleted {
                                    rich.italics()
                                } else {
                                    rich
                                };
                                ui.add(egui::Label::new(rich).wrap());
                            });
                        });
                    ui.add_space(1.0);
                }
            });
    }

    fn render_mod_logs_tab(&self, ui: &mut egui::Ui) {
        let log_count = self.mod_logs.len();
        egui::Frame::new()
            .fill(t::scrim_bg())
            .corner_radius(t::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(if log_count == 0 {
                        "No moderation actions for this user in the current session.".to_owned()
                    } else {
                        format!(
                            "{log_count} moderation action{} (newest first)",
                            if log_count == 1 { "" } else { "s" }
                        )
                    })
                    .color(t::text_muted())
                    .small(),
                );
            });

        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("user_mod_logs_scroll")
            .max_height(320.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.mod_logs.is_empty() {
                    ui.add_space(8.0);
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("No recent moderation actions.")
                                .color(t::text_muted())
                                .small(),
                        );
                    });
                }
                for msg in &self.mod_logs {
                    let time_str = msg.timestamp.format("%H:%M:%S").to_string();
                    egui::Frame::new()
                        .fill(t::warning_soft_bg())
                        .inner_margin(egui::Margin::symmetric(4, 2))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.add(egui::Label::new(
                                    RichText::new(&time_str)
                                        .color(t::text_muted())
                                        .small()
                                        .monospace(),
                                ));
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&msg.raw_text)
                                            .color(t::text_primary())
                                            .small(),
                                    )
                                    .wrap(),
                                );
                            });
                        });
                    ui.add_space(1.0);
                }
            });
    }

    fn render_profile_tab(&self, ui: &mut egui::Ui, profile: &UserProfile) {
        egui::Frame::new()
            .fill(t::bg_card())
            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle()))
            .corner_radius(t::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                let mut add_row = |label: &str, value: String| {
                    let label = RichText::new(label).color(t::text_secondary()).small();
                    let value =
                        egui::Label::new(RichText::new(value).color(t::text_primary()).small())
                            .wrap();
                    if ui.available_width() < 300.0 {
                        ui.label(label);
                        ui.add(value);
                    } else {
                        ui.horizontal_top(|ui| {
                            ui.label(label);
                            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                                ui.add(value);
                            });
                        });
                    }
                    ui.add_space(2.0);
                };

                if let Some(f) = profile.followers {
                    add_row("Followers", fmt_count(f));
                }
                if let Some(ref pronouns) = profile.pronouns {
                    add_row("Pronouns", pronouns.clone());
                }
                match (&self.follow_age_resolved, &profile.followed_at) {
                    (Some(Some(ts)), _) | (None, Some(ts)) => {
                        let age = fmt_follow_age(ts);
                        if !age.is_empty() {
                            add_row("Follow age", age);
                        }
                    }
                    (Some(None), _) => {
                        add_row("Follow age", "Not following".to_owned());
                    }
                    (None, None) => {}
                }
                if let Some(ref ts) = profile.created_at {
                    add_row("Account age", fmt_account_age(ts));
                    add_row("Joined", fmt_join_date(ts));
                }
                if !self.shared_channels.is_empty() {
                    add_row("Shared channels", self.shared_channels.len().to_string());
                }
                if !profile.is_live {
                    if let Some(ref ts) = profile.last_broadcast_at {
                        add_row("Last live", fmt_join_date(ts));
                    }
                }
            });

        if !self.shared_channels.is_empty() {
            ui.add_space(5.0);
            egui::Frame::new()
                .fill(t::bg_card())
                .stroke(egui::Stroke::new(1.0_f32, t::border_subtle()))
                .corner_radius(t::RADIUS_SM)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Shared Channels")
                            .small()
                            .strong()
                            .color(t::text_secondary()),
                    );
                    ui.add_space(3.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for channel in &self.shared_channels {
                            badge_text_pill(ui, channel);
                        }
                    });
                });
        }

        if !profile.description.is_empty() {
            ui.add_space(5.0);
            egui::Frame::new()
                .fill(t::bg_card())
                .stroke(egui::Stroke::new(1.0_f32, t::border_subtle()))
                .corner_radius(t::RADIUS_SM)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Bio")
                            .small()
                            .strong()
                            .color(t::text_secondary()),
                    );
                    ui.add_space(2.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&profile.description)
                                .color(t::text_secondary())
                                .small(),
                        )
                        .wrap(),
                    );
                });
        }

        if profile.is_banned {
            ui.add_space(5.0);
            egui::Frame::new()
                .fill(t::danger_soft_bg())
                .corner_radius(t::RADIUS_SM)
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    let reason = profile
                        .ban_reason
                        .as_deref()
                        .unwrap_or("No reason provided");
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("⚠ Suspended: {reason}"))
                                .color(t::red())
                                .small(),
                        )
                        .wrap(),
                    );
                });
        }
    }

    fn render_moderation_tab(
        &mut self,
        ui: &mut egui::Ui,
        profile: &UserProfile,
        actions: &mut Vec<PopupAction>,
        presets: &[crust_core::model::mod_actions::ModActionPreset],
    ) {
        let Some(channel) = self.channel.clone() else {
            return;
        };

        let login = profile.login.clone();
        let user_id = profile.id.clone();
        let channel_name = channel.display_name().to_ascii_lowercase();

        ui.label(
            RichText::new("Reason (optional)")
                .color(t::text_secondary())
                .small(),
        );
        ui.add_space(2.0);
        ui.add_sized(
            [ui.available_width(), 22.0],
            egui::TextEdit::singleline(&mut self.mod_reason)
                .hint_text("e.g. spam, hate speech...")
                .font(t::small()),
        );
        ui.add_space(6.0);

        section_label(ui, "Quick Actions");
        ui.add_space(3.0);

        let default_presets = crust_core::model::mod_actions::ModActionPreset::defaults();
        let active_presets = if presets.is_empty() {
            default_presets.as_slice()
        } else {
            presets
        };

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            for preset in active_presets {
                if ui.button(RichText::new(&preset.label).small()).clicked() {
                    let mut cmd = preset.expand(&login, &channel_name);
                    if let Some(reason) = reason_opt(&self.mod_reason) {
                        cmd.push(' ');
                        cmd.push_str(&reason);
                    }
                    actions.push(PopupAction::ExecuteCommand {
                        channel: channel.clone(),
                        command: cmd,
                    });
                }
            }
        });

        ui.add_space(3.0);
        ui.horizontal(|ui| {
            let resp = ui.add_sized(
                [80.0, 20.0],
                egui::TextEdit::singleline(&mut self.timeout_custom)
                    .hint_text("custom (s)")
                    .font(t::small()),
            );
            let pressed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let go_btn = ui.add_sized([38.0, 20.0], egui::Button::new(RichText::new("Go").small()));
            if pressed || go_btn.clicked() {
                if let Ok(secs) = self.timeout_custom.trim().parse::<u32>() {
                    if secs > 0 {
                        actions.push(PopupAction::Timeout {
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
        section_label(ui, "Lift restriction");
        ui.add_space(3.0);
        if ui
            .add(
                egui::Button::new(
                    RichText::new("Untimeout / Unban")
                        .small()
                        .color(t::text_on_accent()),
                )
                .fill(t::success_strong_bg())
                .min_size(egui::vec2(160.0, 24.0)),
            )
            .clicked()
        {
            actions.push(PopupAction::Unban {
                channel: channel.clone(),
                login: login.clone(),
                user_id: user_id.clone(),
            });
        }

        ui.add_space(6.0);
        section_label(ui, "Local cleanup");
        ui.add_space(3.0);
        if ui
            .add(egui::Button::new("Hide this user's messages locally").wrap())
            .on_hover_text("Removes visible messages from this user in the current channel only.")
            .clicked()
        {
            actions.push(PopupAction::ClearUserMessagesLocally {
                channel: channel.clone(),
                login: login.clone(),
            });
        }

        let channel_login = channel.display_name().to_ascii_lowercase();
        if !channel_login.is_empty() {
            ui.add_space(8.0);
            section_label(ui, "Workflows");
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open mod view").clicked() {
                    actions.push(PopupAction::OpenUrl {
                        url: format!("https://www.twitch.tv/moderator/{channel_login}/chat"),
                    });
                }
                if ui.button("Open AutoMod").clicked() {
                    actions.push(PopupAction::OpenUrl {
                        url: format!(
                            "https://dashboard.twitch.tv/u/{channel_login}/settings/moderation/automod"
                        ),
                    });
                }
                if ui.button("In-app mod tools").clicked() {
                    actions.push(PopupAction::OpenModerationTools {
                        channel: channel.clone(),
                    });
                }
                if ui.button("Unban requests").clicked() {
                    actions.push(PopupAction::OpenUrl {
                        url: format!(
                            "https://dashboard.twitch.tv/u/{channel_login}/community/unban-requests"
                        ),
                    });
                }
            });
        }

        ui.add_space(8.0);
        section_label(ui, "Low trust");
        ui.add_space(3.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            let can_warn = !self.mod_reason.trim().is_empty();
            if ui
                .add_enabled(can_warn, egui::Button::new("Warn"))
                .clicked()
            {
                let reason = self.mod_reason.trim().to_owned();
                actions.push(PopupAction::Warn {
                    channel: channel.clone(),
                    login: login.clone(),
                    user_id: user_id.clone(),
                    reason,
                });
            }
            if ui.button("Monitor").clicked() {
                actions.push(PopupAction::Monitor {
                    channel: channel.clone(),
                    login: login.clone(),
                    user_id: user_id.clone(),
                });
            }
            if ui.button("Restrict").clicked() {
                actions.push(PopupAction::Restrict {
                    channel: channel.clone(),
                    login: login.clone(),
                    user_id: user_id.clone(),
                });
            }
            if ui.button("Unmonitor").clicked() {
                actions.push(PopupAction::Unmonitor {
                    channel: channel.clone(),
                    login: login.clone(),
                    user_id: user_id.clone(),
                });
            }
            if ui.button("Unrestrict").clicked() {
                actions.push(PopupAction::Unrestrict {
                    channel: channel.clone(),
                    login: login.clone(),
                    user_id: user_id.clone(),
                });
            }
        });

        ui.add_space(8.0);
        section_label(ui, "Permanent ban");
        ui.add_space(3.0);
        if self.ban_confirm {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let confirm = ui.add(
                    egui::Button::new(
                        RichText::new("Confirm ban")
                            .small()
                            .color(t::text_on_accent()),
                    )
                    .fill(t::danger_strong_bg())
                    .min_size(egui::vec2(120.0, 24.0)),
                );
                let cancel = ui.add(
                    egui::Button::new(RichText::new("Cancel").small())
                        .min_size(egui::vec2(60.0, 24.0)),
                );
                if confirm.clicked() {
                    actions.push(PopupAction::Ban {
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
        } else if ui
            .add(
                egui::Button::new(RichText::new("Ban user").small().color(t::text_on_accent()))
                    .fill(t::danger_strong_bg().gamma_multiply(0.9))
                    .min_size(egui::vec2(100.0, 24.0)),
            )
            .clicked()
        {
            self.ban_confirm = true;
        }
    }

    fn render_ivr_logs_tab(
        &mut self,
        ui: &mut egui::Ui,
        profile: &UserProfile,
        actions: &mut Vec<PopupAction>,
    ) {
        let ivr_url = format!(
            "https://logs.ivr.fi/?channel={}&username={}",
            self.channel.as_ref().map(|c| c.as_str()).unwrap_or(""),
            profile.login,
        );

        ui.horizontal_wrapped(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Open in Browser").small().color(t::accent()))
                        .fill(t::bg_raised())
                        .min_size(egui::vec2(130.0, 22.0)),
                )
                .clicked()
            {
                actions.push(PopupAction::OpenUrl {
                    url: ivr_url.clone(),
                });
            }
            if !self.ivr_logs_loading {
                if ui
                    .add(
                        egui::Button::new(RichText::new("↻ Refresh").small())
                            .min_size(egui::vec2(70.0, 22.0)),
                    )
                    .clicked()
                {
                    actions.push(PopupAction::FetchIvrLogs {
                        channel: self.current_channel_name(),
                        username: profile.login.clone(),
                    });
                }
            }
        });
        ui.add_space(4.0);

        if self.ivr_logs_loading {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Fetching logs...")
                        .color(t::text_secondary())
                        .italics(),
                );
            });
            ui.add_space(8.0);
            return;
        }

        if let Some(ref err) = self.ivr_logs_error {
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(t::danger_soft_bg())
                .corner_radius(t::RADIUS_SM)
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(format!("⚠ {err}")).color(t::red()).small())
                            .wrap(),
                    );
                });
            return;
        }

        if !self.ivr_logs_requested {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Click the IVR Logs tab to load external chat history.")
                        .color(t::text_muted())
                        .small(),
                );
            });
            ui.add_space(8.0);
            return;
        }

        let log_count = self.ivr_logs.len();
        egui::Frame::new()
            .fill(t::scrim_bg())
            .corner_radius(t::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(if log_count == 0 {
                        "No external logs found for this user.".to_owned()
                    } else {
                        format!(
                            "{log_count} message{} (from logs.ivr.fi, newest first)",
                            if log_count == 1 { "" } else { "s" }
                        )
                    })
                    .color(t::text_muted())
                    .small(),
                );
            });
        ui.add_space(4.0);

        if self.ivr_logs.is_empty() {
            ui.add_space(8.0);
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No messages.").color(t::text_muted()).small());
            });
            return;
        }

        const ROW_HEIGHT: f32 = 22.0;
        let total_rows = self.ivr_logs.len();
        egui::ScrollArea::vertical()
            .id_salt("ivr_logs_scroll")
            .max_height(320.0)
            .auto_shrink([false, true])
            .show_rows(ui, ROW_HEIGHT, total_rows, |ui, row_range| {
                for idx in row_range {
                    let entry = &self.ivr_logs[idx];
                    let time_str = fmt_ivr_timestamp(&entry.timestamp);
                    let is_timeout = entry.msg_type == 2;

                    egui::Frame::new()
                        .fill(if is_timeout {
                            t::alpha(t::red(), 20)
                        } else {
                            Color32::TRANSPARENT
                        })
                        .inner_margin(egui::Margin::symmetric(4, 2))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.add(egui::Label::new(
                                    RichText::new(&time_str)
                                        .color(t::text_muted())
                                        .small()
                                        .monospace(),
                                ));
                                let msg_color = if is_timeout {
                                    t::red()
                                } else {
                                    t::text_primary()
                                };
                                let rich = RichText::new(&entry.text).color(msg_color).small();
                                let rich = if is_timeout { rich.italics() } else { rich };
                                ui.add(egui::Label::new(rich).wrap());
                            });
                        });
                    ui.add_space(1.0);
                }
            });
    }

    fn render_footer(&self, ui: &mut egui::Ui, profile: &UserProfile) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let (primary_label, primary_url) =
                if self.channel.as_ref().map(|c| c.is_kick()).unwrap_or(false) {
                    (
                        "View on Kick".to_owned(),
                        format!("https://kick.com/{}", profile.login),
                    )
                } else {
                    (
                        "View on Twitch".to_owned(),
                        format!("https://twitch.tv/{}", profile.login),
                    )
                };
            ui.hyperlink_to(
                RichText::new(primary_label).small().color(t::accent()),
                primary_url,
            );
            let logs_url = format!(
                "https://logs.ivr.fi/?channel={}&username={}",
                self.channel.as_ref().map(|c| c.as_str()).unwrap_or(""),
                profile.login,
            );
            ui.hyperlink_to(
                RichText::new("Lookup logs")
                    .small()
                    .color(t::text_secondary()),
                logs_url,
            );
        });
    }

    /// Render the popup, returning any actions the user triggered.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        emote_bytes: &UiImageCache,
        stv_avatars: &HashMap<String, String>,
        presets: &[crust_core::model::mod_actions::ModActionPreset],
    ) -> Vec<PopupAction> {
        if !self.open {
            self.close();
            return Vec::new();
        }

        let title = self.popup_title();
        let bounds = ctx.screen_rect().shrink(8.0);
        let style = ctx.style();
        let frame = egui::Frame::window(&style);
        let title_height = ctx
            .fonts(|fonts| fonts.row_height(&egui::TextStyle::Heading.resolve(&style)))
            .max(style.spacing.interact_size.y)
            + frame.inner_margin.sum().y
            + frame.stroke.width;
        let max_size = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
            .max(egui::Vec2::splat(1.0));
        let content_width = max_size.x.min(430.0);
        let content_height = max_size.y;

        let mut actions: Vec<PopupAction> = Vec::new();
        let mut open = self.open;

        egui::Window::new(title)
            .frame(frame)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .min_width(0.0)
            .max_width(content_width)
            .default_width(content_width)
            .max_height(content_height)
            .constrain_to(bounds)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(content_width);
                ui.style_mut().spacing.item_spacing = egui::vec2(4.0, 3.0);
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);

                egui::ScrollArea::vertical()
                    .id_salt("user_profile_body")
                    .max_height(content_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.loading {
                            Self::render_loading_state(ui);
                            return;
                        }

                        let Some(profile) = self.profile.clone() else {
                            Self::render_unavailable_state(ui);
                            return;
                        };

                        self.render_profile_header(ui, &profile, emote_bytes, stv_avatars);
                        ui.add_space(4.0);
                        self.render_quick_actions(ui, &profile, &mut actions);
                        self.render_live_info(ui, &profile);

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(2.0);
                        self.render_tab_bar(ui, &profile, &mut actions);

                        match self.active_tab {
                            ProfileTab::Profile => self.render_profile_tab(ui, &profile),
                            ProfileTab::Moderation if self.is_mod => {
                                self.render_moderation_tab(ui, &profile, &mut actions, presets)
                            }
                            ProfileTab::ModLogs if self.is_mod => self.render_mod_logs_tab(ui),
                            ProfileTab::Logs => self.render_logs_tab(ui),
                            ProfileTab::IvrLogs => {
                                self.render_ivr_logs_tab(ui, &profile, &mut actions)
                            }
                            _ => {}
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(3.0);

                        self.render_footer(ui, &profile);

                        ui.add_space(4.0);
                    });
            });

        // Close the popup after a mod action so the user doesn't have to.
        let has_mod_action = actions.iter().any(|a| {
            matches!(
                a,
                PopupAction::Timeout { .. }
                    | PopupAction::Ban { .. }
                    | PopupAction::Unban { .. }
                    | PopupAction::Warn { .. }
                    | PopupAction::Monitor { .. }
                    | PopupAction::Restrict { .. }
                    | PopupAction::Unmonitor { .. }
                    | PopupAction::Unrestrict { .. }
                    | PopupAction::ClearUserMessagesLocally { .. }
                    | PopupAction::ExecuteCommand { .. }
            )
        });
        if !open || has_mod_action {
            self.close();
        }
        actions
    }
}

// Helpers

/// Format an IVR timestamp ("2026-03-05T09:35:03.061Z") to "YYYY-MM-DD HH:MM".
fn fmt_ivr_timestamp(ts: &str) -> String {
    // Try to extract "YYYY-MM-DD HH:MM" from ISO 8601
    let ts = ts.trim_end_matches('Z');
    if let Some(t_idx) = ts.find('T') {
        let date = &ts[..t_idx];
        let time = &ts[t_idx + 1..];
        let time_clean = time.split('.').next().unwrap_or(time);
        // Show HH:MM:SS only
        format!("{} {}", date, time_clean)
    } else {
        ts.to_owned()
    }
}

/// Allocate the complete frame before painting so horizontal rows can wrap it.
fn wrapped_text_frame(ui: &mut egui::Ui, frame: egui::Frame, text: RichText) -> egui::Response {
    let margin = frame.total_margin().sum();
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Wrap),
        (ui.available_width() - margin.x).max(1.0),
        egui::TextStyle::Body,
    );
    ui.allocate_ui_with_layout(
        galley.size() + margin,
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            frame
                .show(ui, |ui| ui.add(egui::Label::new(galley)))
                .response
        },
    )
    .inner
}

/// Render a small inline role pill using egui Frame.
fn role_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let frame = egui::Frame::new()
        .fill(t::alpha(color, 40))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(5, 2));
    wrapped_text_frame(ui, frame, RichText::new(text).color(color).small().strong());
}

/// Render a small text badge pill (fallback when image is unavailable).
fn badge_text_pill(ui: &mut egui::Ui, name: &str) {
    let (fill, text_col) = if t::is_light() {
        (t::pill_bg(), t::text_secondary())
    } else {
        (t::pill_bg(), t::text_primary())
    };
    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(4, 1));
    wrapped_text_frame(
        ui,
        frame,
        RichText::new(name)
            .color(text_col)
            .size((t::usercard_font_size() * 0.36).max(8.0)),
    );
}

/// Section heading inside the mod panel.
fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(
        RichText::new(text)
            .small()
            .strong()
            .color(t::text_secondary()),
    ));
}

/// Render a compact action button used in the usercard quick-actions strip.
fn small_action_btn(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).small())
            .fill(t::bg_raised())
            .stroke(egui::Stroke::new(1.0_f32, t::border_subtle()))
            .min_size(egui::vec2(72.0, 20.0)),
    )
    .clicked()
}

/// Convert the reason buffer to `Option<String>` (None if empty).
fn reason_opt(buf: &str) -> Option<String> {
    let s = buf.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

/// Resolve user log row text, reconstructing from spans when raw_text is absent.
fn chat_message_text(msg: &ChatMessage) -> String {
    if !msg.raw_text.is_empty() {
        return msg.raw_text.clone();
    }

    msg.spans
        .iter()
        .filter_map(|s| match s {
            crust_core::model::Span::Text { text, .. } => Some(text.as_str()),
            crust_core::model::Span::Mention { login } => Some(login.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a CSS hex color string (`#RRGGBB`) into an egui Color32.
fn parse_hex_color(hex: &str) -> Option<Color32> {
    t::parse_hex_rgb(hex)
}

/// Convert a badge name + version to a human-readable label.
fn badge_display_name(name: &str, version: &str) -> String {
    let canonical = name.replace('_', "-");
    match canonical.as_str() {
        "subscriber" => match version.parse::<u32>().unwrap_or(0) {
            0 => "Subscriber".to_owned(),
            m => format!("{m}-month Sub"),
        },
        "moderator" => "Moderator".to_owned(),
        "broadcaster" => "Broadcaster".to_owned(),
        "vip" => "VIP".to_owned(),
        "staff" => "Staff".to_owned(),
        "admin" => "Admin".to_owned(),
        "global-mod" => "Global Mod".to_owned(),
        "partner" => "Partner".to_owned(),
        "sub-gifter" => "Sub Gifter".to_owned(),
        "artist-badge" => "Artist".to_owned(),
        "bits" => {
            let val: u64 = version.parse().unwrap_or(0);
            if val > 0 {
                format!("{val} Bits")
            } else {
                "Bits".to_owned()
            }
        }
        _ => {
            let s = canonical.replace('-', " ");
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

/// Format an ISO 8601 timestamp as "Month YYYY".
fn fmt_join_date(ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return dt.format("%b %-d, %Y").to_string();
    }
    let parts: Vec<&str> = ts.splitn(3, '-').collect();
    if parts.len() < 2 {
        return ts.to_owned();
    }
    let year = parts[0];
    let month = match parts[1] {
        "01" => "January",
        "02" => "February",
        "03" => "March",
        "04" => "April",
        "05" => "May",
        "06" => "June",
        "07" => "July",
        "08" => "August",
        "09" => "September",
        "10" => "October",
        "11" => "November",
        "12" => "December",
        m => m,
    };
    format!("{month} {year}")
}

/// Compute account age ("X years, Ymo") from an ISO 8601 timestamp.
fn fmt_account_age(ts: &str) -> String {
    let Some(unix) = iso_to_unix_secs(ts) else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(unix);
    let years = secs / 31_557_600;
    let months = (secs % 31_557_600) / 2_629_800;
    match years {
        0 => format!("{months} months"),
        1 => {
            if months == 0 {
                "1 year".to_owned()
            } else {
                format!("1 year, {months}mo")
            }
        }
        n => {
            if months == 0 {
                format!("{n} years")
            } else {
                format!("{n} years, {months}mo")
            }
        }
    }
}

/// Compute follow age from an ISO follow timestamp.
fn fmt_follow_age(ts: &str) -> String {
    fmt_account_age(ts)
}

/// Convert known badge identifiers into role labels shown near profile identity.
fn badge_role_labels(badges: &[Badge]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for badge in badges {
        let key = badge.name.to_ascii_lowercase();
        let label = match key.as_str() {
            "broadcaster" => Some("Broadcaster"),
            "moderator" => Some("Moderator"),
            "vip" => Some("VIP"),
            "founder" => Some("Founder"),
            "subscriber" => Some("Subscriber"),
            "staff" => Some("Staff"),
            "admin" => Some("Admin"),
            "global_mod" | "global-mod" => Some("Global Mod"),
            _ => None,
        };
        if let Some(label) = label {
            if !out.iter().any(|v| v.eq_ignore_ascii_case(label)) {
                out.push(label.to_owned());
            }
        }
    }
    out
}

/// Format an active stream as a live uptime string ("1h 23m", "45m").
fn fmt_uptime(ts: &str) -> String {
    let Some(started) = iso_to_unix_secs(ts) else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(started);
    if elapsed == 0 {
        return String::new();
    }
    let h = elapsed / 3600;
    let m = (elapsed % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Parse an ISO 8601 UTC datetime into Unix seconds.
/// Handles `"YYYY-MM-DDTHH:MM:SSZ"`, `"...+00:00"`, and fractional seconds.
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
    if dp.len() < 3 {
        return None;
    }
    let year: i64 = dp[0].parse().ok()?;
    let month: i64 = dp[1].parse().ok()?;
    let day: i64 = dp[2].parse().ok()?;

    let time_clean = time_part.splitn(2, '.').next().unwrap_or(time_part);
    let tp: Vec<&str> = time_clean.splitn(4, ':').collect();
    if tp.len() < 3 {
        return None;
    }
    let hour: u64 = tp[0].parse().ok()?;
    let min: u64 = tp[1].parse().ok()?;
    let sec: u64 = tp[2].parse().ok()?;

    // Gregorian date -> Julian Day Number -> Unix days
    let a = (14 - month) / 12;
    let yy = year + 4800 - a;
    let mm = month + 12 * a - 3;
    let jdn = day + (153 * mm + 2) / 5 + 365 * yy + yy / 4 - yy / 100 + yy / 400 - 32_045;
    let unix_days = jdn - 2_440_588; // JDN of 1970-01-01
    if unix_days < 0 {
        return None;
    }
    Some(unix_days as u64 * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(display_name: &str) -> UserProfile {
        UserProfile {
            id: "1".to_owned(),
            login: "alice".to_owned(),
            display_name: display_name.to_owned(),
            description: String::new(),
            created_at: None,
            avatar_url: None,
            followers: None,
            is_partner: false,
            is_affiliate: false,
            pronouns: Some("she/her".to_owned()),
            followed_at: None,
            chat_color: None,
            is_live: false,
            stream_title: None,
            stream_game: None,
            stream_viewers: None,
            last_broadcast_at: None,
            is_banned: false,
            ban_reason: None,
        }
    }

    fn profile_frame(
        ctx: &egui::Context,
        popup: &mut UserProfilePopup,
        size: egui::Vec2,
        events: Vec<egui::Event>,
        presets: &[crust_core::model::mod_actions::ModActionPreset],
    ) -> (egui::FullOutput, Vec<PopupAction>) {
        crate::app::apply_theme_visuals(ctx);
        let mut actions = Vec::new();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |_| {});
                actions = popup.show(ctx, &UiImageCache::default(), &HashMap::new(), presets);
            },
        );
        (output, actions)
    }

    fn visible_profile_text(
        output: &egui::FullOutput,
        size: egui::Vec2,
        label: &str,
    ) -> Option<egui::Rect> {
        fn find(shape: &egui::epaint::Shape, clip: egui::Rect, label: &str) -> Option<egui::Rect> {
            match shape {
                egui::epaint::Shape::Vec(shapes) => {
                    shapes.iter().find_map(|shape| find(shape, clip, label))
                }
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                    clip.contains_rect(rect).then_some(rect)
                }
                _ => None,
            }
        }
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        output
            .shapes
            .iter()
            .find_map(|shape| find(&shape.shape, shape.clip_rect.intersect(screen), label))
    }

    fn profile_bounds(ctx: &egui::Context, size: egui::Vec2) -> egui::Rect {
        let bounds = ctx
            .memory(|memory| memory.area_rect(egui::Id::new("@alice")))
            .unwrap();
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                .expand(1.0)
                .contains_rect(bounds),
            "profile window {bounds:?} exceeds {size:?}"
        );
        bounds
    }

    fn scroll_profile_to_bottom(
        ctx: &egui::Context,
        popup: &mut UserProfilePopup,
        size: egui::Vec2,
        presets: &[crust_core::model::mod_actions::ModActionPreset],
    ) -> egui::FullOutput {
        let pointer = profile_bounds(ctx, size).center();
        let mut output = egui::FullOutput::default();
        for _ in 0..2 {
            profile_frame(
                ctx,
                popup,
                size,
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -4000.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                presets,
            );
            for _ in 0..8 {
                output = profile_frame(ctx, popup, size, Vec::new(), presets).0;
            }
        }
        profile_bounds(ctx, size);
        output
    }

    fn click_profile(
        ctx: &egui::Context,
        popup: &mut UserProfilePopup,
        size: egui::Vec2,
        point: egui::Pos2,
        presets: &[crust_core::model::mod_actions::ModActionPreset],
    ) -> Vec<PopupAction> {
        let mut actions = Vec::new();
        for pressed in [true, false] {
            actions.extend(
                profile_frame(
                    ctx,
                    popup,
                    size,
                    vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    presets,
                )
                .1,
            );
        }
        actions
    }

    #[test]
    fn long_profile_scrolls_in_short_viewports_and_keeps_footer_and_close_reachable() {
        for size in [
            egui::vec2(220.0, 260.0),
            egui::vec2(320.0, 260.0),
            egui::vec2(640.0, 320.0),
        ] {
            let ctx = egui::Context::default();
            let mut popup = UserProfilePopup::default();
            popup.set_loading("alice", Vec::new(), Some(ChannelId::new("room")), true);
            let mut data = profile("Alice with a longer display name");
            data.description =
                "A profile with several lines of descriptive text and links to read. ".repeat(5);
            data.created_at = Some("2015-01-01T00:00:00Z".into());
            data.followers = Some(1000);
            data.is_partner = true;
            data.chat_color = Some("#4AA8FF".into());
            data.is_live = true;
            data.stream_title = Some("A stream with a longer descriptive title".into());
            data.stream_game = Some("A category with a longer name".into());
            data.stream_viewers = Some(123456);
            popup.set_profile(data);
            popup.set_shared_channels(
                (0..32)
                    .map(|n| format!("#a_very_long_shared_channel_{n:02}"))
                    .collect(),
            );
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = profile_frame(&ctx, &mut popup, size, Vec::new(), &[]).0;
            }
            profile_bounds(&ctx, size);
            assert!(visible_profile_text(&output, size, "@alice").is_some());
            assert!(visible_profile_text(&output, size, "Lookup logs").is_none());
            let output = scroll_profile_to_bottom(&ctx, &mut popup, size, &[]);
            assert!(
                visible_profile_text(&output, size, "View on Twitch").is_some(),
                "profile footer remains clipped at {size:?}"
            );
            assert!(visible_profile_text(&output, size, "Lookup logs").is_some());
            assert!(visible_profile_text(&output, size, "@alice").is_some());
            let bounds = profile_bounds(&ctx, size);
            assert!(click_profile(
                &ctx,
                &mut popup,
                size,
                egui::pos2(bounds.right() - 14.0, bounds.top() + 14.0),
                &[]
            )
            .is_empty());
            assert!(
                !popup.open,
                "title-bar close remains usable after scrolling"
            );
        }
    }

    #[test]
    fn full_moderation_scrolls_to_ban_confirmation_and_cancel_in_short_viewports() {
        let presets: Vec<_> = (0..24)
            .map(|n| crust_core::model::mod_actions::ModActionPreset {
                label: format!("Action {n:02}"),
                command_template: "/timeout {user} 60".into(),
                icon_url: None,
            })
            .collect();
        for size in [
            egui::vec2(220.0, 260.0),
            egui::vec2(320.0, 260.0),
            egui::vec2(640.0, 320.0),
        ] {
            let ctx = egui::Context::default();
            let mut popup = UserProfilePopup::default();
            popup.set_loading("alice", Vec::new(), Some(ChannelId::new("room")), true);
            popup.set_profile(profile("Alice"));
            popup.active_tab = ProfileTab::Moderation;
            for _ in 0..4 {
                profile_frame(&ctx, &mut popup, size, Vec::new(), &presets);
            }
            let output = scroll_profile_to_bottom(&ctx, &mut popup, size, &presets);
            let ban = visible_profile_text(&output, size, "Ban user")
                .expect("ban action is reachable by scrolling");
            assert!(click_profile(&ctx, &mut popup, size, ban.center(), &presets).is_empty());
            assert!(popup.ban_confirm);
            let output = scroll_profile_to_bottom(&ctx, &mut popup, size, &presets);
            assert!(visible_profile_text(&output, size, "Confirm ban").is_some());
            let cancel = visible_profile_text(&output, size, "Cancel")
                .expect("cancel remains visible beside confirmation");
            assert!(click_profile(&ctx, &mut popup, size, cancel.center(), &presets).is_empty());
            assert!(!popup.ban_confirm);
            assert!(popup.open);
            profile_bounds(&ctx, size);
        }
    }

    #[test]
    fn closing_a_loading_profile_rejects_late_success_and_failure() {
        let mut popup = UserProfilePopup::default();
        popup.set_loading("alice", vec![], Some(ChannelId::new("room")), false);
        popup.open = false;
        assert!(!popup.accepts_profile("Alice"));
        popup.set_profile(profile("Alice"));
        popup.set_unavailable("alice");
        assert!(!popup.open);
        assert!(popup.profile.is_none());

        popup.set_loading("alice", vec![], Some(ChannelId::new("room")), false);
        popup.close();
        assert!(!popup.loading);
        assert!(popup.loading_login.is_empty());
        assert!(!popup.accepts_profile("alice"));
        popup.set_profile(profile("Alice"));
        assert!(!popup.open);
        assert!(popup.profile.is_none());
    }

    #[test]
    fn only_the_explicitly_opened_profile_accepts_completion_and_refresh() {
        let mut popup = UserProfilePopup::default();
        popup.set_profile(profile("Unrequested"));
        popup.set_unavailable("alice");
        assert!(!popup.open);
        assert!(popup.profile.is_none());

        popup.set_loading("alice", vec![], Some(ChannelId::new("first")), false);
        popup.set_loading("bob", vec![], Some(ChannelId::new("second")), false);
        popup.set_profile(profile("Old request"));
        popup.set_unavailable("alice");
        assert!(popup.loading);
        assert!(popup.profile.is_none());
        assert!(popup.accepts_profile("BOB"));

        let mut bob = profile("Bob");
        bob.login = "bob".to_owned();
        popup.set_profile(bob.clone());
        assert!(popup.open);
        assert!(!popup.loading);
        assert_eq!(popup.profile.as_ref().unwrap().display_name, "Bob");
        bob.display_name = "Updated Bob".to_owned();
        popup.set_profile(bob);
        assert_eq!(popup.profile.as_ref().unwrap().display_name, "Updated Bob");
        assert_eq!(popup.channel, Some(ChannelId::new("second")));

        popup.close();
        popup.set_loading("alice", vec![], Some(ChannelId::new("first")), false);
        popup.set_unavailable("ALICE");
        assert!(popup.open);
        assert!(!popup.loading);
        assert!(popup.profile.is_none());
    }

    fn ivr_log(text: &str) -> IvrLogEntry {
        IvrLogEntry {
            text: text.to_owned(),
            timestamp: "2026-11-14T12:00:00Z".to_owned(),
            display_name: "Alice".to_owned(),
            msg_type: 1,
        }
    }

    #[test]
    fn ivr_logs_reject_other_channels_users_and_unscoped_results() {
        let mut popup = UserProfilePopup::default();
        popup.set_loading("alice", vec![], Some(ChannelId::new("first")), false);
        popup.set_profile(profile("Alice"));
        let first = popup.begin_ivr_logs_request("first", "alice").unwrap();

        popup.set_loading("alice", vec![], Some(ChannelId::new("second")), false);
        popup.set_profile(profile("Alice"));
        let second = popup.begin_ivr_logs_request("second", "alice").unwrap();
        assert_ne!(first, second);
        for (channel, user, request) in [
            ("first", "alice", Some(first)),
            ("first", "alice", Some(second)),
            ("second", "bob", Some(second)),
            ("second", "alice", None),
        ] {
            assert!(!popup.apply_ivr_logs_result(channel, user, request, Ok(vec![ivr_log("stale")])));
            assert!(!popup.apply_ivr_logs_result(channel, user, request, Err("stale error".into())));
            assert!(popup.ivr_logs_loading);
            assert!(popup.ivr_logs.is_empty());
            assert!(popup.ivr_logs_error.is_none());
        }
        assert!(popup.apply_ivr_logs_result(
            "SECOND", "ALICE", Some(second), Ok(vec![ivr_log("current")]),
        ));
        assert!(!popup.ivr_logs_loading);
        assert_eq!(popup.ivr_logs[0].text, "current");
        assert!(!popup.apply_ivr_logs_result(
            "second", "alice", Some(second), Err("duplicate completion".into()),
        ));
        assert!(popup.ivr_logs_error.is_none());
    }

    #[test]
    fn ivr_logs_reject_closed_reopened_and_superseded_requests() {
        let mut popup = UserProfilePopup::default();
        popup.set_loading("alice", vec![], Some(ChannelId::new("room")), false);
        popup.set_profile(profile("Alice"));
        let closed = popup.begin_ivr_logs_request("room", "alice").unwrap();
        popup.close();
        assert!(!popup.apply_ivr_logs_result("room", "alice", Some(closed), Ok(vec![ivr_log("closed")])));
        assert!(popup.begin_ivr_logs_request("room", "alice").is_none());

        popup.set_loading("alice", vec![], Some(ChannelId::new("room")), false);
        popup.set_profile(profile("Alice"));
        let reopened = popup.begin_ivr_logs_request("room", "alice").unwrap();
        assert!(popup.begin_ivr_logs_request("other", "alice").is_none());
        assert!(popup.begin_ivr_logs_request("room", "bob").is_none());
        assert!(!popup.apply_ivr_logs_result("room", "alice", Some(closed), Err("closed request".into())));
        assert!(popup.ivr_logs_loading);
        assert!(popup.apply_ivr_logs_result("room", "alice", Some(reopened), Ok(vec![ivr_log("loaded")])));

        let refresh = popup.begin_ivr_logs_request("room", "alice").unwrap();
        assert!(!popup.apply_ivr_logs_result("room", "alice", Some(reopened), Err("old refresh".into())));
        assert!(popup.ivr_logs_loading);
        assert_eq!(popup.ivr_logs[0].text, "loaded");
        assert!(popup.apply_ivr_logs_result("room", "alice", Some(refresh), Err("current failure".into())));
        assert!(!popup.ivr_logs_loading);
        assert_eq!(popup.ivr_logs_error.as_deref(), Some("current failure"));
        assert_eq!(popup.ivr_logs[0].text, "loaded");
    }

    #[test]
    fn open_profile_restores_the_server_name_after_alias_changes() {
        let mut popup = UserProfilePopup::default();
        popup.set_loading("alice", vec![], Some(ChannelId::new("first")), false);
        popup.set_profile(profile("Alice"));
        popup.apply_nicknames(&[crust_core::model::Nickname::new("alice", "Custom")]);
        assert_eq!(popup.profile.as_ref().unwrap().display_name, "Custom");
        let mut scoped = crust_core::model::Nickname::new("alice", "Elsewhere");
        scoped.channel = Some("second".to_owned());
        popup.apply_nicknames(&[scoped]);
        assert_eq!(popup.profile.as_ref().unwrap().display_name, "Alice");
        popup.apply_nicknames(&[crust_core::model::Nickname::new("alice", "Another")]);
        popup.apply_nicknames(&[]);
        assert_eq!(popup.profile.as_ref().unwrap().display_name, "Alice");
        popup.set_profile(profile("New server name"));
        popup.apply_nicknames(&[crust_core::model::Nickname::new("alice", "Custom")]);
        popup.apply_nicknames(&[]);
        assert_eq!(
            popup.profile.as_ref().unwrap().display_name,
            "New server name"
        );
    }

    #[test]
    fn profile_log_snapshots_restore_original_sender_names() {
        let channel = ChannelId::new("first");
        let message = ChatMessage {
            id: crust_core::MessageId(1),
            server_id: None,
            timestamp: chrono::Utc::now(),
            channel,
            sender: crust_core::model::Sender {
                user_id: crust_core::model::UserId("1".to_owned()),
                login: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: "example".to_owned(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: Default::default(),
            reply: None,
            msg_kind: crust_core::model::MsgKind::Chat,
            shared: None,
        };
        let mut popup = UserProfilePopup::default();
        popup.set_logs(vec![message.clone()]);
        popup.set_mod_logs(vec![message]);
        popup.apply_nicknames(&[crust_core::model::Nickname::new("alice", "Custom")]);
        assert_eq!(popup.logs[0].sender.display_name, "Custom");
        assert_eq!(popup.mod_logs[0].sender.display_name, "Custom");
        popup.apply_nicknames(&[]);
        assert_eq!(popup.logs[0].sender.display_name, "Alice");
        assert_eq!(popup.mod_logs[0].sender.display_name, "Alice");
    }
}
