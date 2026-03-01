use egui::{Color32, RichText};

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
                    // ── Logged-in view ──────────────────────────────
                    ui.vertical_centered(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("✓ Logged in")
                                .color(Color32::from_rgb(80, 200, 100))
                                .strong()
                                .size(16.0),
                        );
                        if let Some(name) = username {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(name)
                                    .color(Color32::from_rgb(180, 130, 255))
                                    .strong()
                                    .size(14.0),
                            );
                        }
                        ui.add_space(12.0);
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Logout").clicked() {
                            result = Some(LoginAction::Logout);
                            self.open = false;
                        }
                        if ui.button("Close").clicked() {
                            self.open = false;
                        }
                    });
                } else {
                    // ── Login view ──────────────────────────────────
                    ui.label("Paste your Twitch OAuth token to send messages.");
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Get a token from twitchtokengenerator.com (chat:edit + chat:read scopes)")
                            .small()
                            .color(Color32::from_rgb(140, 140, 140)),
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
                        ui.label(RichText::new(err).color(Color32::from_rgb(255, 80, 80)).small());
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
