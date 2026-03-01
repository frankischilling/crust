use egui::{Color32, RichText, Ui};

use crust_core::model::ChannelId;

/// Chat input bar shown at the bottom of the message area.
pub struct ChatInput<'a> {
    /// The active channel to send messages to.
    pub channel: &'a ChannelId,
    /// Whether the user is authenticated (can send).
    pub logged_in: bool,
    /// The current username (for display).
    pub username: Option<&'a str>,
}

/// Result from showing the chat input.
pub struct ChatInputResult {
    /// The message text to send, if any.
    pub send: Option<String>,
}

impl<'a> ChatInput<'a> {
    /// Show the chat input. The `buf` is stored externally so it persists across frames.
    pub fn show(&self, ui: &mut Ui, buf: &mut String) -> ChatInputResult {
        let mut result = ChatInputResult { send: None };

        egui::Frame::new()
            .fill(Color32::from_rgb(30, 30, 35))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                if !self.logged_in {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Log in to send messages")
                                .color(Color32::from_rgb(120, 120, 120))
                                .italics(),
                        );
                    });
                    return;
                }

                ui.horizontal(|ui| {
                    // Username label
                    if let Some(name) = self.username {
                        ui.label(
                            RichText::new(format!("{name}:"))
                                .color(Color32::from_rgb(180, 130, 255))
                                .strong()
                                .small(),
                        );
                    }

                    // Text input
                    let resp = ui.add(
                        egui::TextEdit::singleline(buf)
                            .hint_text("Send a message...")
                            .desired_width(ui.available_width() - 60.0)
                            .text_color(Color32::WHITE)
                            .frame(true),
                    );

                    let enter_pressed = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    if enter_pressed && !buf.trim().is_empty() {
                        result.send = Some(buf.trim().to_owned());
                        buf.clear();
                        // Re-focus the input after sending
                        resp.request_focus();
                    }

                    if ui.button("Chat").clicked() && !buf.trim().is_empty() {
                        result.send = Some(buf.trim().to_owned());
                        buf.clear();
                    }
                });
            });

        result
    }
}
