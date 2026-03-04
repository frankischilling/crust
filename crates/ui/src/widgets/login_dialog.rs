use std::collections::HashMap;
use std::sync::Arc;

use egui::RichText;

use crate::theme as t;

/// Action emitted by the account-manager dialog.
pub enum LoginAction {
    /// Add / log in with a new OAuth token.
    Login(String),
    /// Sign out of the currently active account (session goes anonymous, account stays saved).
    Logout,
    /// Switch the active session to a different already-saved account.
    SwitchAccount(String),
    /// Permanently remove a saved account (and its stored token).
    RemoveAccount(String),
    /// Mark an account as the one that auto-logs in on startup.
    SetDefaultAccount(String),
}

/// Multi-account manager dialog.
///
/// Call [`LoginDialog::update_accounts`] whenever the account list changes so
/// the dialog stays in sync with the runtime state.
#[derive(Default)]
pub struct LoginDialog {
    pub open: bool,
    /// All saved account usernames (kept in sync by CrustApp via update_accounts).
    pub accounts: Vec<String>,
    /// Username of the currently active account (None = anonymous).
    pub active_account: Option<String>,
    /// Account pinned to auto-login on startup.
    pub default_account: Option<String>,
    token_buf: String,
    show_add_form: bool,
    error_msg: Option<String>,
}

impl LoginDialog {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.token_buf.clear();
            self.error_msg = None;
            // Auto-expand the add-account form when there are no accounts yet.
            self.show_add_form = self.accounts.is_empty() && self.active_account.is_none();
        }
    }

    /// Keep the dialog's account list in sync with the application state.
    pub fn update_accounts(
        &mut self,
        accounts: Vec<String>,
        active: Option<String>,
        default: Option<String>,
    ) {
        self.accounts = accounts;
        self.active_account = active;
        self.default_account = default;
    }

    /// Show the dialog.  Returns `Some(LoginAction)` when the user performs an action.
    ///
    /// * `logged_in`   – whether a session is currently authenticated.
    /// * `username`    – display name of the active user (legacy / fallback).
    /// * `avatar_url`  – CDN URL for the active user's avatar image.
    /// * `emote_bytes` – shared image cache (used to render the avatar).
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        logged_in: bool,
        username: Option<&str>,
        avatar_url: Option<&str>,
        emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
    ) -> Option<LoginAction> {
        if !self.open {
            return None;
        }

        // Build the account list to display.  We rely on `self.accounts`
        // (from AccountListUpdated events) but gracefully fall back to the
        // legacy `username` param so the dialog works even before the first
        // event arrives.
        let mut shown_accounts: Vec<String> = self.accounts.clone();
        let active = self.active_account.clone().or_else(|| {
            if logged_in {
                username.map(|s| s.to_owned())
            } else {
                None
            }
        });
        if let Some(ref uname) = active {
            if !shown_accounts.iter().any(|a| a == uname) {
                shown_accounts.insert(0, uname.clone());
            }
        }

        let mut result: Option<LoginAction> = None;

        egui::Window::new("Accounts")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let screen_w = ui.ctx().screen_rect().width();
                let dialog_w = (screen_w - 64.0)
                    .clamp(120.0, 380.0)
                    .min((screen_w - 16.0).max(80.0));
                ui.set_min_width(dialog_w);
                ui.set_max_width(dialog_w);

                ui.add_space(4.0);

                // ── Account list ────────────────────────────────────────────
                if shown_accounts.is_empty() {
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("No accounts added yet.")
                                .color(t::TEXT_MUTED)
                                .font(t::small()),
                        );
                    });
                    ui.add_space(6.0);
                } else {
                    for acc_name in &shown_accounts {
                        let is_active = active.as_deref() == Some(acc_name.as_str());
                        let is_default = self.default_account.as_deref() == Some(acc_name.as_str());
                        draw_account_row(
                            ui,
                            acc_name,
                            is_active,
                            is_default,
                            avatar_url,
                            emote_bytes,
                            &mut result,
                            &mut self.open,
                        );
                    }
                    ui.add_space(6.0);
                    ui.add(egui::Separator::default().spacing(4.0));
                }

                // ── Add-account form / button ────────────────────────────────
                if self.show_add_form {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Paste your Twitch OAuth token:")
                            .font(t::small())
                            .color(t::TEXT_SECONDARY),
                    );
                    ui.add_space(4.0);

                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.token_buf)
                            .hint_text("oauth:abc123…")
                            .password(true)
                            .desired_width((ui.available_width() - 8.0).max(96.0)),
                    );
                    if self.token_buf.is_empty() {
                        resp.request_focus();
                    }

                    ui.label(
                        RichText::new("Get a token at twitchtokengenerator.com  (chat:edit + chat:read scopes)")
                            .font(t::small())
                            .color(t::TEXT_MUTED),
                    );

                    if let Some(err) = &self.error_msg {
                        ui.add_space(2.0);
                        ui.label(RichText::new(err).color(t::RED).font(t::small()));
                    }

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let enter = resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (ui
                            .add_sized(
                                [84.0, t::BAR_H],
                                egui::Button::new(RichText::new("Add account").font(t::small())),
                            )
                            .clicked()
                            || enter)
                            && !self.token_buf.trim().is_empty()
                        {
                            let token = self.token_buf.trim().to_owned();
                            result = Some(LoginAction::Login(token));
                            self.token_buf.clear();
                            self.show_add_form = false;
                            self.open = false;
                        }
                        if ui
                            .add_sized(
                                [52.0, t::BAR_H],
                                egui::Button::new(RichText::new("Cancel").font(t::small())),
                            )
                            .clicked()
                        {
                            self.show_add_form = false;
                            self.token_buf.clear();
                            self.error_msg = None;
                        }
                    });
                    ui.add_space(4.0);
                } else {
                    ui.add_space(4.0);
                    if ui
                        .add_sized(
                            [ui.available_width(), t::BAR_H],
                            egui::Button::new(RichText::new("+ Add Account").font(t::small())),
                        )
                        .on_hover_text("Add another Twitch account")
                        .clicked()
                    {
                        self.show_add_form = true;
                        self.token_buf.clear();
                        self.error_msg = None;
                    }
                    ui.add_space(4.0);
                }

                // ── Footer ───────────────────────────────────────────────────
                ui.add_space(2.0);
                ui.add(egui::Separator::default().spacing(4.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if ui
                        .add_sized(
                            [52.0, t::BAR_H],
                            egui::Button::new(RichText::new("Close").font(t::small())),
                        )
                        .clicked()
                    {
                        self.open = false;
                    }
                });
                ui.add_space(2.0);
            });

        result
    }
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Render a single account row: avatar • name • status • action buttons.
fn draw_account_row(
    ui: &mut egui::Ui,
    acc_name: &str,
    is_active: bool,
    is_default: bool,
    avatar_url: Option<&str>,
    emote_bytes: &HashMap<String, (u32, u32, Arc<[u8]>)>,
    result: &mut Option<LoginAction>,
    dialog_open: &mut bool,
) {
    let initial = acc_name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .next()
        .unwrap_or('?');
    let avatar_r = 16.0_f32;

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        // Avatar circle
        let (av_rect, _) = ui.allocate_exact_size(
            egui::vec2(avatar_r * 2.0, avatar_r * 2.0),
            egui::Sense::hover(),
        );
        let center = av_rect.center();

        if is_active {
            let avatar_data = avatar_url
                .and_then(|url| emote_bytes.get(url).map(|(_, _, raw)| (url, raw.clone())));
            if let Some((logo, raw)) = avatar_data {
                let uri = format!("bytes://{logo}");
                let av_size = avatar_r * 2.0;
                ui.painter().circle_filled(center, avatar_r, t::BG_RAISED);
                ui.put(
                    av_rect,
                    egui::Image::from_bytes(uri, egui::load::Bytes::Shared(raw))
                        .fit_to_exact_size(egui::vec2(av_size, av_size))
                        .corner_radius(egui::CornerRadius::same(avatar_r as u8)),
                );
            } else {
                ui.painter().circle_filled(center, avatar_r, t::ACCENT_DIM);
                ui.painter()
                    .circle_stroke(center, avatar_r, egui::Stroke::new(1.5, t::ACCENT));
                ui.painter().text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    initial.to_string(),
                    egui::FontId::proportional(avatar_r * 1.1),
                    t::TEXT_PRIMARY,
                );
            }
        } else {
            ui.painter().circle_filled(center, avatar_r, t::BG_RAISED);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                initial.to_string(),
                egui::FontId::proportional(avatar_r * 1.1),
                t::TEXT_SECONDARY,
            );
        }

        ui.add_space(4.0);

        // Username + status
        ui.vertical(|ui| {
            ui.label(
                RichText::new(acc_name)
                    .color(if is_active {
                        t::TEXT_PRIMARY
                    } else {
                        t::TEXT_SECONDARY
                    })
                    .strong(),
            );
            if is_active {
                ui.label(
                    RichText::new("● Active")
                        .font(egui::FontId::proportional(10.0))
                        .color(t::GREEN),
                );
            }
            if is_default {
                ui.label(
                    RichText::new("★ Auto-login")
                        .font(egui::FontId::proportional(10.0))
                        .color(egui::Color32::from_rgb(220, 180, 50)),
                );
            }
        });

        // Right-side action buttons
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Remove
            if ui
                .add_sized(
                    [56.0, t::BAR_H],
                    egui::Button::new(RichText::new("Remove").font(t::small()).color(t::RED))
                        .fill(t::RED.gamma_multiply(0.08))
                        .stroke(egui::Stroke::new(1.0, t::RED.gamma_multiply(0.3))),
                )
                .on_hover_text(format!("Remove {acc_name} from saved accounts"))
                .clicked()
            {
                *result = Some(LoginAction::RemoveAccount(acc_name.to_owned()));
                *dialog_open = false;
            }

            if is_active {
                if ui
                    .add_sized(
                        [60.0, t::BAR_H],
                        egui::Button::new(RichText::new("Sign out").font(t::small())),
                    )
                    .on_hover_text("Disconnect and resume as anonymous viewer")
                    .clicked()
                {
                    *result = Some(LoginAction::Logout);
                    *dialog_open = false;
                }
            } else if ui
                .add_sized(
                    [52.0, t::BAR_H],
                    egui::Button::new(RichText::new("Switch").font(t::small())),
                )
                .on_hover_text(format!("Switch active account to {acc_name}"))
                .clicked()
            {
                *result = Some(LoginAction::SwitchAccount(acc_name.to_owned()));
                *dialog_open = false;
            }

            // Default-account star toggle (rendered last = leftmost in RTL)
            let star = if is_default { "★" } else { "☆" };
            let star_color = if is_default {
                egui::Color32::from_rgb(220, 180, 50)
            } else {
                t::TEXT_MUTED
            };
            let star_tooltip = if is_default {
                "Auto-login account (click to unset)"
            } else {
                "Set as auto-login account on startup"
            };
            let star_btn = ui
                .add_sized(
                    [t::BAR_H, t::BAR_H],
                    egui::Button::new(RichText::new(star).color(star_color).font(t::small()))
                        .fill(egui::Color32::TRANSPARENT),
                )
                .on_hover_text(star_tooltip);
            if star_btn.clicked() {
                if is_default {
                    // Unset: empty string sentinel clears the default
                    *result = Some(LoginAction::SetDefaultAccount(String::new()));
                } else {
                    *result = Some(LoginAction::SetDefaultAccount(acc_name.to_owned()));
                }
            }
        });
    });
}
