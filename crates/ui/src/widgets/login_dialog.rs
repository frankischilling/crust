use egui::RichText;

use crate::theme as t;

/// Result from the login dialog.
pub enum LoginAction {
    /// User submitted an OAuth token.
    Login(String),
    /// User requested to log out.
    Logout,
}

/// Dialog for logging in with a Twitch OAuth token.
///
/// Supports two modes:
/// 1. Paste an OAuth token directly (e.g. from twitchtokengenerator.com)
/// 2. Open browser for Twitch OAuth (requires CRUST_CLIENT_ID env var)
#[derive(Default)]
pub struct LoginDialog {
    pub open: bool,
    token_buf: String,
    error_msg: Option<String>,
}

impl LoginDialog {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.token_buf.clear();
            self.error_msg = None;
        }
    }

    /// Show the dialog. Returns `Some(LoginAction)` when the user acts.
    pub fn show(&mut self, ctx: &egui::Context, logged_in: bool, username: Option<&str>) -> Option<LoginAction> {
        if !self.open {
            return None;
        }

        let mut result = None;

        egui::Window::new(if logged_in { "Account" } else { "Login to Twitch" })
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(360.0);

                if logged_in {
                    // ── Profile card ────────────────────────────────
                    let name = username.unwrap_or("User");
                    let initial = name.chars().next().unwrap_or('?')
                        .to_uppercase().next().unwrap_or('?');

                    ui.add_space(8.0);

                    ui.vertical_centered(|ui| {
                        // Avatar circle
                        let avatar_size = 56.0;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(avatar_size, avatar_size),
                            egui::Sense::hover(),
                        );
                        let painter = ui.painter();
                        painter.circle_filled(rect.center(), avatar_size * 0.5, t::ACCENT_DIM);
                        painter.circle_stroke(
                            rect.center(),
                            avatar_size * 0.5,
                            egui::Stroke::new(2.0, t::ACCENT),
                        );
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            initial.to_string(),
                            egui::FontId::proportional(24.0),
                            t::TEXT_PRIMARY,
                        );

                        ui.add_space(10.0);

                        // Username
                        ui.label(
                            RichText::new(name)
                                .color(t::TEXT_PRIMARY)
                                .strong()
                                .size(15.0),
                        );

                        ui.add_space(6.0);

                        // "Connected" status pill — drawn manually so we
                        // can give it a tinted background + border.
                        let pill_text = "● Connected";
                        let pill_font = egui::FontId::proportional(11.0);
                        let galley = ui.painter().layout_no_wrap(
                            pill_text.to_owned(),
                            pill_font.clone(),
                            t::GREEN,
                        );
                        let hpad = 8.0;
                        let vpad = 3.0;
                        let pill_size = galley.size() + egui::vec2(hpad * 2.0, vpad * 2.0);
                        let (pill_rect, _) = ui.allocate_exact_size(pill_size, egui::Sense::hover());
                        let cr = egui::CornerRadius::same((pill_rect.height() / 2.0) as u8);
                        ui.painter().rect(
                            pill_rect,
                            cr,
                            t::GREEN.gamma_multiply(0.12),
                            egui::Stroke::new(1.0, t::GREEN.gamma_multiply(0.35)),
                            egui::StrokeKind::Outside,
                        );
                        ui.painter().text(
                            pill_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            pill_text,
                            pill_font,
                            t::GREEN,
                        );

                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("twitch.tv")
                                .font(t::small())
                                .color(t::TEXT_MUTED),
                        );
                    });

                    ui.add_space(14.0);
                    ui.add(egui::Separator::default().spacing(0.0));
                    ui.add_space(10.0);

                    // Action buttons
                    ui.columns(2, |cols| {
                        if cols[0]
                            .add_sized(
                                [cols[0].available_width(), t::BAR_H],
                                egui::Button::new(
                                    RichText::new("Log out").font(t::small()).color(t::RED),
                                )
                                .fill(t::RED.gamma_multiply(0.1))
                                .stroke(egui::Stroke::new(1.0, t::RED.gamma_multiply(0.4))),
                            )
                            .clicked()
                        {
                            result = Some(LoginAction::Logout);
                            self.open = false;
                        }

                        if cols[1]
                            .add_sized(
                                [cols[1].available_width(), t::BAR_H],
                                egui::Button::new(RichText::new("Close").font(t::small())),
                            )
                            .clicked()
                        {
                            self.open = false;
                        }
                    });

                    ui.add_space(4.0);
                } else {
                    // ── Login view ──────────────────────────────────
                    ui.label("Paste your Twitch OAuth token to send messages.");
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Get a token from twitchtokengenerator.com (chat:edit + chat:read scopes)")
                            .font(t::small())
                            .color(t::TEXT_SECONDARY),
                    );
                    ui.add_space(8.0);

                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.token_buf)
                            .hint_text("oauth:abc123...")
                            .password(true)
                            .desired_width(320.0),
                    );

                    // Auto-focus
                    if resp.gained_focus() || self.token_buf.is_empty() {
                        resp.request_focus();
                    }

                    if let Some(err) = &self.error_msg {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(err).color(t::RED).font(t::small()),
                        );
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        let enter = resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));

                        if (ui.button("Login").clicked() || enter) && !self.token_buf.trim().is_empty() {
                            let token = self.token_buf.trim().to_owned();
                            result = Some(LoginAction::Login(token));
                            self.open = false;
                            self.token_buf.clear();
                        }
                        if ui.button("Cancel").clicked() {
                            self.open = false;
                            self.token_buf.clear();
                        }
                    });
                }
            });

        result
    }
}
