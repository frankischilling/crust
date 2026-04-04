use crate::theme as t;
use egui::{Context, RichText};

/// Account switcher widget state for displaying and switching between multiple saved accounts.
#[derive(Clone, Debug, Default)]
pub struct AccountSwitcherState {
    pub show_popup: bool,
    pub add_account_mode: bool,
}

/// An account entry shown in the switcher list.
#[derive(Clone, Debug)]
pub struct AccountEntry {
    pub username: String,
    pub is_active: bool,
    pub has_token: bool,
}

/// Result of account switcher interaction.
#[derive(Clone, Debug)]
pub enum AccountSwitcherAction {
    SwitchTo(String),
    Remove(String),
    AddNew,
    Close,
}

impl AccountSwitcherState {
    pub fn new() -> Self {
        Self {
            show_popup: false,
            add_account_mode: false,
        }
    }

    /// Render the account switcher button and popup.
    pub fn show(
        &mut self,
        ctx: &Context,
        ui: &mut egui::Ui,
        current_username: &str,
        accounts: &[AccountEntry],
    ) -> Option<AccountSwitcherAction> {
        let mut action = None;

        // Account switcher button
        let button_text = if current_username.is_empty() {
            "No Account"
        } else {
            current_username
        };

        if ui
            .button(RichText::new(format!("👤 {}", button_text)).font(t::small()))
            .clicked()
        {
            self.show_popup = !self.show_popup;
        }

        // Popup window
        if self.show_popup {
            let popup_id = egui::Id::new("account_switcher_popup");
            egui::Window::new("Accounts")
                .id(popup_id)
                .collapsible(false)
                .resizable(false)
                .default_width(250.0)
                .show(ctx, |ui| {
                    ui.set_min_width(250.0);

                    ui.heading(RichText::new("Switch Account").font(t::body()));
                    ui.add_space(8.0);

                    // Accounts list
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for account in accounts {
                                ui.horizontal(|ui| {
                                    let username_text = if account.is_active {
                                        RichText::new(&account.username)
                                            .font(t::body())
                                            .color(t::link())
                                            .strong()
                                    } else {
                                        RichText::new(&account.username).font(t::body())
                                    };

                                    if ui
                                        .selectable_label(account.is_active, username_text)
                                        .clicked()
                                        && !account.is_active
                                    {
                                        action = Some(AccountSwitcherAction::SwitchTo(
                                            account.username.clone(),
                                        ));
                                        self.show_popup = false;
                                    }

                                    // Token status indicator
                                    let token_indicator =
                                        if account.has_token { "🔑" } else { "⚠" };
                                    let token_color = if account.has_token {
                                        t::green()
                                    } else {
                                        t::bits_orange()
                                    };
                                    ui.label(
                                        RichText::new(token_indicator)
                                            .font(t::small())
                                            .color(token_color),
                                    )
                                    .on_hover_text(
                                        if account.has_token {
                                            "Token saved"
                                        } else {
                                            "No token - login required"
                                        },
                                    );

                                    // Remove button
                                    if !account.is_active {
                                        if ui
                                            .button(
                                                RichText::new("🗑").font(t::tiny()).color(t::red()),
                                            )
                                            .on_hover_text("Remove account")
                                            .clicked()
                                        {
                                            action = Some(AccountSwitcherAction::Remove(
                                                account.username.clone(),
                                            ));
                                            self.show_popup = false;
                                        }
                                    }
                                });
                            }
                        });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Add new account button
                    if ui
                        .button(RichText::new("+ Add Account").font(t::body()))
                        .clicked()
                    {
                        action = Some(AccountSwitcherAction::AddNew);
                        self.show_popup = false;
                    }

                    // Close button
                    if ui.button("Close").clicked() {
                        action = Some(AccountSwitcherAction::Close);
                        self.show_popup = false;
                    }
                });
        }

        action
    }
}
