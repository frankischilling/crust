use crate::theme as t;
use crust_core::model::ChannelId;

/// Simple popup for joining a channel by name.
///
/// Supports Twitch, Kick and generic IRC channels:
/// - `channelname` or `twitch:channelname` -> Twitch
/// - `kick:channelname` -> Kick
/// - `irc://host[:port]/channel` or `ircs://host[:port]/channel` -> IRC
/// - `irc://host` or `ircs://host` -> connect server tab, then `/join #channel`
#[derive(Default)]
pub struct JoinDialog {
    pub open: bool,
    buf: String,
    error: Option<String>,
    focus_input: bool,
    focused_control: Option<egui::Id>,
}

impl JoinDialog {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.buf.clear();
            self.error = None;
        }
        self.focus_input = self.open;
        self.focused_control = None;
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

        let input_id = egui::Id::new("join_channel_input");
        let window_id = egui::Id::new("join_channel_window");
        let dialog_layer = egui::LayerId::new(egui::Order::Middle, window_id);
        // Escape clears egui's focus before widgets run, so retain the last
        // focused control only when it still owned focus at the start of this pass.
        let focused = ctx.memory(|memory| memory.focused().or_else(|| {
            self.focused_control.filter(|id| memory.had_focus_last_frame(*id))
        }));
        let owns_escape = focused.and_then(|id| ctx.read_response(id))
            .is_some_and(|response| response.layer_id == dialog_layer);
        let owns_submit = focused == Some(input_id);
        let (cancel, submit) = if !ctx.is_context_menu_open() {
            ctx.input_mut(|input| (
                owns_escape && crate::app::consume_exact_key(input, egui::Modifiers::NONE, egui::Key::Escape),
                owns_submit && crate::app::consume_exact_key(input, egui::Modifiers::NONE, egui::Key::Enter),
            ))
        } else {
            (false, false)
        };
        if cancel {
            self.open = false;
            self.focus_input = false;
            self.focused_control = None;
            return None;
        }
        let mut open = self.open;
        let width = (ctx.screen_rect().width() - 48.0).clamp(100.0, 360.0);
        egui::Window::new("Join Channel")
            .id(window_id)
            .collapsible(false)
            .resizable(false)
            .min_width(0.0)
            .max_width(width)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(width);
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
                        .id(input_id)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY),
                );

                if std::mem::take(&mut self.focus_input) {
                    resp.request_focus();
                }

                ui.add_space(8.0);

                ui.horizontal_wrapped(|ui| {
                    let has_channel = !self.buf.trim().is_empty();
                    if (ui.add_enabled(has_channel, egui::Button::new("Join")).clicked() || submit) && has_channel {
                        match parse_channel_input(self.buf.trim(), allow_kick, allow_irc) {
                            Ok(ch) => {
                                result = Some(ch);
                                self.open = false;
                                self.error = None;
                            }
                            Err(msg) => {
                                self.error = Some(msg);
                                self.focus_input = true;
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
                    ui.label(egui::RichText::new(err).small().color(t::red()));
                }
            });

        self.open &= open;
        self.focused_control = ctx.memory(|memory| memory.focused()).filter(|id| {
            ctx.read_response(*id).is_some_and(|response| response.layer_id == dialog_layer)
        });
        if !self.open {
            self.focus_input = false;
            self.focused_control = None;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, dialog: &mut JoinDialog, events: Vec<egui::Event>, width: f32) -> Option<ChannelId> {
        let mut selected = None;
        let _ = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 500.0))),
            events,
            ..Default::default()
        }, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut String::new()).id(egui::Id::new("external_join_composer")));
            });
            selected = dialog.show(ctx, true, true);
        });
        selected
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    #[test]
    fn blank_join_field_allows_tab_navigation_and_owned_escape() {
        let ctx = egui::Context::default();
        let mut dialog = JoinDialog::default();
        dialog.toggle();
        frame(&ctx, &mut dialog, Vec::new(), 300.0);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new("join_channel_input"))));
        frame(&ctx, &mut dialog, vec![key(egui::Key::Tab)], 300.0);
        frame(&ctx, &mut dialog, Vec::new(), 300.0);
        assert!(!ctx.memory(|memory| memory.has_focus(egui::Id::new("join_channel_input"))));
        assert!(dialog.focused_control.is_some());
        frame(&ctx, &mut dialog, vec![key(egui::Key::Escape)], 300.0);
        assert!(!dialog.open);
    }

    #[test]
    fn join_escape_does_not_close_for_an_external_composer() {
        let ctx = egui::Context::default();
        let mut dialog = JoinDialog::default();
        dialog.toggle();
        frame(&ctx, &mut dialog, Vec::new(), 300.0);
        let external_id = egui::Id::new("external_join_composer");
        ctx.memory_mut(|memory| memory.request_focus(external_id));
        frame(&ctx, &mut dialog, Vec::new(), 300.0);
        assert!(ctx.memory(|memory| memory.has_focus(external_id)));
        frame(&ctx, &mut dialog, vec![key(egui::Key::Escape)], 300.0);
        assert!(dialog.open);
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("join_channel_input")));
        frame(&ctx, &mut dialog, Vec::new(), 300.0);
        frame(&ctx, &mut dialog, vec![key(egui::Key::Escape)], 300.0);
        assert!(!dialog.open);
    }

    #[test]
    fn join_cancel_button_handles_enter_without_submitting() {
        let ctx = egui::Context::default();
        let mut dialog = JoinDialog::default();
        dialog.toggle();
        frame(&ctx, &mut dialog, Vec::new(), 300.0);
        frame(&ctx, &mut dialog, vec![egui::Event::Text("rustlang".to_owned())], 300.0);
        for _ in 0..2 {
            frame(&ctx, &mut dialog, vec![key(egui::Key::Tab)], 300.0);
            frame(&ctx, &mut dialog, Vec::new(), 300.0);
        }
        assert!(frame(&ctx, &mut dialog, vec![key(egui::Key::Enter)], 300.0).is_none());
        assert!(!dialog.open);
    }

    #[test]
    fn narrow_join_dialog_remains_visible_and_submits_keyboard_input() {
        let ctx = egui::Context::default();
        let mut dialog = JoinDialog::default();
        dialog.toggle();
        for _ in 0..3 { frame(&ctx, &mut dialog, Vec::new(), 300.0); }
        let rect = ctx.memory(|memory| memory.area_rect(egui::Id::new("join_channel_window"))).unwrap();
        assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 500.0)).contains_rect(rect));
        frame(&ctx, &mut dialog, vec![egui::Event::Text("rustlang".to_owned())], 300.0);
        assert_eq!(frame(&ctx, &mut dialog, vec![key(egui::Key::Enter)], 300.0), Some(ChannelId::new("rustlang")));
        assert!(!dialog.open);
    }
}
