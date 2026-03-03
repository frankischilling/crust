use crust_core::model::ChannelId;

/// Simple popup for joining a channel by name.
///
/// Supports both Twitch and Kick channels:
/// - `channelname` or `twitch:channelname` → Twitch
/// - `kick:channelname` → Kick
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
                ui.set_min_width(320.0);
                ui.label("Enter a channel name:");
                ui.label(
                    egui::RichText::new("Prefix with kick: for Kick channels")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.buf)
                        .hint_text("channel  or  kick:channel")
                        .desired_width(280.0),
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
                        result = Some(parse_channel_input(self.buf.trim()));
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

/// Parse user input into a `ChannelId`, detecting the `kick:` prefix.
fn parse_channel_input(input: &str) -> ChannelId {
    if let Some(slug) = input.strip_prefix("kick:") {
        ChannelId::kick(slug.trim())
    } else if let Some(name) = input.strip_prefix("twitch:") {
        ChannelId::new(name.trim())
    } else {
        ChannelId::new(input)
    }
}
