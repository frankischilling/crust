use crust_core::model::ChannelId;

/// Simple popup for joining a channel by name.
#[derive(Default)]
pub struct JoinDialog {
    pub open: bool,
    buf: String,
}

impl JoinDialog {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.buf.clear();
        }
    }

    /// Show the dialog. Returns `Some(ChannelId)` when the user submits.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<ChannelId> {
        let mut result = None;

        if !self.open {
            return None;
        }

        egui::Window::new("Join Channel")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(300.0);
                ui.label("Enter a Twitch channel name:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.buf)
                        .hint_text("channel_name")
                        .desired_width(250.0),
                );

                // Auto-focus
                if resp.gained_focus() || self.buf.is_empty() {
                    resp.request_focus();
                }

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    let enter = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Join").clicked() || enter) && !self.buf.trim().is_empty() {
                        result = Some(ChannelId::new(self.buf.trim()));
                        self.open = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.open = false;
                    }
                });
            });

        result
    }
}
