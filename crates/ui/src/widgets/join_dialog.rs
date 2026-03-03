use crust_core::model::ChannelId;

/// Simple popup for joining a channel by name.
///
/// Supports Twitch, Kick and generic IRC channels:
/// - `channelname` or `twitch:channelname` → Twitch
/// - `kick:channelname` → Kick
/// - `irc://host[:port]/channel` or `ircs://host[:port]/channel` → IRC
/// - `irc://host` or `ircs://host` → connect server tab, then `/join #channel`
#[derive(Default)]
pub struct JoinDialog {
    pub open: bool,
    buf: String,
    error: Option<String>,
}

impl JoinDialog {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.buf.clear();
            self.error = None;
        }
    }

    /// Show the dialog. Returns `Some(ChannelId)` when the user submits.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        allow_kick: bool,
        allow_irc: bool,
    ) -> Option<ChannelId> {
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
                let examples = match (allow_kick, allow_irc) {
                    (true, true) => {
                        "Examples: channel  |  kick:channel  |  ircs://irc.libera.chat/#rust"
                    }
                    (true, false) => "Examples: channel  |  kick:channel",
                    (false, true) => "Examples: channel  |  ircs://irc.libera.chat/#rust",
                    (false, false) => "Examples: channel  |  twitch:channel",
                };
                ui.label(egui::RichText::new(examples).small().weak());
                ui.add_space(4.0);
                let hint = match (allow_kick, allow_irc) {
                    (true, true) => "channel  |  kick:channel  |  ircs://host[/#channel]",
                    (true, false) => "channel  |  kick:channel",
                    (false, true) => "channel  |  ircs://host[/#channel]",
                    (false, false) => "channel  |  twitch:channel",
                };
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.buf)
                        .hint_text(hint)
                        .desired_width(280.0),
                );

                // Auto-focus
                if resp.gained_focus() || self.buf.is_empty() {
                    resp.request_focus();
                }

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Join").clicked() || enter) && !self.buf.trim().is_empty() {
                        match parse_channel_input(self.buf.trim(), allow_kick, allow_irc) {
                            Ok(ch) => {
                                result = Some(ch);
                                self.open = false;
                                self.error = None;
                            }
                            Err(msg) => {
                                self.error = Some(msg);
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.open = false;
                        self.error = None;
                    }
                });

                if let Some(err) = &self.error {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(err)
                            .small()
                            .color(egui::Color32::from_rgb(220, 110, 110)),
                    );
                }
            });

        result
    }
}

/// Parse user input into a `ChannelId`.
fn parse_channel_input(
    input: &str,
    allow_kick: bool,
    allow_irc: bool,
) -> Result<ChannelId, String> {
    let Some(id) = ChannelId::parse_user_input(input) else {
        return Err("Invalid channel format.".to_owned());
    };
    if id.is_kick() && !allow_kick {
        return Err("Kick compatibility is disabled in Settings (beta).".to_owned());
    }
    if id.is_irc() && !allow_irc {
        return Err("IRC compatibility is disabled in Settings (beta).".to_owned());
    }
    Ok(id)
}
