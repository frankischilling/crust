use std::collections::VecDeque;

use crust_core::{ChannelId, ComposerTarget, ReplyInfo};
use egui::{Context, RichText};

use crate::theme as t;

#[derive(Clone, Debug)]
pub(crate) struct UnsentMessage {
    pub channel: ChannelId,
    pub text: String,
    pub reply_to_msg_id: Option<String>,
    pub reply: Option<ReplyInfo>,
    pub composer: Option<ComposerTarget>,
    pub reason: String,
}

impl UnsentMessage {
    pub fn belongs_to(&self, account_id: Option<&str>) -> bool {
        !self.channel.is_twitch()
            || self.composer.as_ref().is_none_or(|target| target.account_id() == account_id)
    }

    pub fn reply_info(&self) -> Option<ReplyInfo> {
        self.reply.clone().or_else(|| {
            self.reply_to_msg_id.as_ref().map(|id| ReplyInfo {
                parent_msg_id: id.clone(),
                ..Default::default()
            })
        })
    }
}

#[derive(Default)]
pub(crate) struct UnsentMessages {
    messages: VecDeque<UnsentMessage>,
    front_generation: u64,
}

impl UnsentMessages {
    pub fn push(&mut self, message: UnsentMessage) {
        self.messages.push_back(message);
    }

    #[cfg(test)]
    pub fn front(&self) -> Option<&UnsentMessage> {
        self.messages.front()
    }

    pub fn front_for_account(&self, account_id: Option<&str>) -> Option<&UnsentMessage> {
        self.messages.iter().find(|message| message.belongs_to(account_id))
    }

    #[cfg(test)]
    pub fn show(&mut self, ctx: &Context, can_restore: bool) -> Option<UnsentMessage> {
        self.show_for_account(ctx, can_restore, None)
    }

    pub fn show_for_account(&mut self, ctx: &Context, can_restore: bool, account_id: Option<&str>) -> Option<UnsentMessage> {
        let index = self.messages.iter().position(|message| message.belongs_to(account_id))?;
        let count = self.messages.iter().filter(|message| message.belongs_to(account_id)).count();
        let generation = self.front_generation;
        let message = &mut self.messages[index];
        let mut restore = false;
        let mut dismiss = false;
        let mut copy = false;
        let screen = ctx.screen_rect();
        let width = (screen.width() - 48.0).clamp(100.0, 420.0);
        let height = (screen.height() - 64.0).clamp(80.0, 420.0);
        egui::Window::new("Unsent messages")
            .id(egui::Id::new("unsent_messages"))
            .default_width(width)
            .max_width(width)
            .min_width(0.0)
            .default_height(height)
            .max_height(height)
            .min_height(80.0)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                ui.set_min_height(ui.available_height().min(height));
                egui::TopBottomPanel::bottom("unsent_messages_actions")
                    .frame(egui::Frame::NONE)
                    .min_height(0.0)
                    .show_inside(ui, |ui| {
                        ui.separator();
                        ui.horizontal_wrapped(|ui| {
                            restore = ui.add_enabled(can_restore, egui::Button::new("Restore draft")).clicked();
                            copy = ui.button("Copy message").clicked();
                            dismiss = ui.button("Dismiss").clicked();
                        });
                    });
                egui::ScrollArea::vertical()
                    .id_salt(("unsent_messages_body", account_id, generation))
                    .max_height(ui.available_height())
                    .min_scrolled_height(0.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(format!(
                            "Message to #{} was not sent", message.channel.display_name(),
                        )).strong().color(t::red())).wrap());
                        ui.add(egui::Label::new(&message.reason).wrap());
                        if count > 1 {
                            ui.label(format!("{count} messages are waiting to be recovered."));
                        }
                        if let Some(reply) = message.reply.as_ref() {
                            ui.add(egui::Label::new(format!("Reply to @{}: {}",
                                reply.parent_display_name, reply.parent_msg_body)).wrap());
                        }
                        ui.add(egui::TextEdit::multiline(&mut message.text)
                            .id(egui::Id::new("unsent_message_draft").with((account_id, generation)))
                            .desired_width(f32::INFINITY)
                            .desired_rows(3));
                        if !can_restore {
                            ui.add(egui::Label::new("Open the original channel or thread with an empty composer to restore this draft.").wrap());
                        }
                    });
            });
        if copy {
            ctx.copy_text(message.text.clone());
        }
        if restore || dismiss {
            self.front_generation = self.front_generation.wrapping_add(1);
        }
        if restore {
            self.messages.remove(index)
        } else {
            if dismiss {
                self.messages.remove(index);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_message(channel: &str) -> UnsentMessage {
        UnsentMessage {
            channel: ChannelId::new(channel),
            text: (0..120)
                .map(|line| format!("Draft line {line}: keep 🙂 every word.\n"))
                .collect(),
            reply_to_msg_id: Some("parent-id".into()),
            composer: None,
            reply: Some(ReplyInfo {
                parent_msg_id: "parent-id".into(),
                parent_user_login: "author".into(),
                parent_display_name: "Author".into(),
                parent_msg_body: "Full reply context with several lines.\n".repeat(40),
                ..Default::default()
            }),
            reason: "The connection is unavailable; this draft has been kept.\n".repeat(30),
        }
    }

    fn frame(
        ctx: &Context,
        dialog: &mut UnsentMessages,
        size: egui::Vec2,
        can_restore: bool,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<UnsentMessage>) {
        let mut restored = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                events,
                ..Default::default()
            },
            |ctx| {
                restored = dialog.show(ctx, can_restore);
            },
        );
        (output, restored)
    }

    fn label_rect(output: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                if text.galley.text() == label {
                    let rect = text.galley.rect.translate(text.pos.to_vec2());
                    if shape.clip_rect.expand(0.5).contains_rect(rect) {
                        return Some(rect);
                    }
                }
            }
            None
        })
    }

    fn click(
        ctx: &Context,
        dialog: &mut UnsentMessages,
        size: egui::Vec2,
        can_restore: bool,
        point: egui::Pos2,
    ) -> (egui::FullOutput, Option<UnsentMessage>) {
        let mut result = (egui::FullOutput::default(), None);
        for pressed in [true, false] {
            result = frame(
                ctx,
                dialog,
                size,
                can_restore,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        result
    }

    #[test]
    fn long_unsent_content_keeps_actions_visible_in_short_windows() {
        for (size, font_size) in [
            (egui::vec2(480.0, 320.0), 14.0),
            (egui::vec2(320.0, 240.0), 14.0),
            (egui::vec2(480.0, 320.0), 24.0),
        ] {
            let ctx = Context::default();
            let mut style = (*ctx.style()).clone();
            style.override_font_id = Some(egui::FontId::proportional(font_size));
            ctx.set_style(style);
            let mut dialog = UnsentMessages::default();
            let message = long_message("room");
            dialog.push(message.clone());
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = frame(&ctx, &mut dialog, size, false, Vec::new()).0;
            }
            let bounds = ctx
                .memory(|memory| memory.area_rect(egui::Id::new("unsent_messages")))
                .unwrap();
            assert!(
                egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                    .expand(1.0)
                    .contains_rect(bounds),
                "dialog {bounds:?} escaped viewport {size:?} with font {font_size}"
            );
            for label in ["Restore draft", "Copy message", "Dismiss"] {
                assert!(
                    label_rect(&output, label).is_some(),
                    "{label} is clipped at {size:?}"
                );
            }
            let restored = click(
                &ctx,
                &mut dialog,
                size,
                false,
                label_rect(&output, "Restore draft").unwrap().center(),
            )
            .1;
            assert!(restored.is_none());
            assert_eq!(dialog.front().unwrap().text, message.text);
            assert_eq!(dialog.front().unwrap().reason, message.reason);
            assert_eq!(
                dialog
                    .front()
                    .unwrap()
                    .reply_info()
                    .unwrap()
                    .parent_msg_body,
                message.reply.unwrap().parent_msg_body
            );
        }
    }

    #[test]
    fn id_only_recovery_keeps_the_parent_without_inventing_metadata() {
        let mut message = long_message("room");
        message.reply = None;
        let reply = message.reply_info().unwrap();
        assert_eq!(reply.parent_msg_id, "parent-id");
        assert!(reply.parent_user_login.is_empty());
        assert!(reply.parent_display_name.is_empty());
        assert!(reply.parent_msg_body.is_empty());
    }

    #[test]
    fn recovery_queue_only_exposes_the_current_accounts_targeted_drafts() {
        let mut dialog = UnsentMessages::default();
        let mut alice = long_message("alice-room");
        alice.composer = Some(ComposerTarget::ReplyThread { account_id: Some("alice".into()), root_msg_id: "root".into() });
        dialog.push(alice);
        assert!(dialog.front_for_account(Some("bob")).is_none());
        let mut bob = long_message("bob-room");
        bob.composer = Some(ComposerTarget::Chat { account_id: Some("bob".into()), pane_id: 12 });
        dialog.push(bob);
        assert_eq!(dialog.front_for_account(Some("alice")).unwrap().channel, ChannelId::new("alice-room"));
        assert_eq!(dialog.front_for_account(Some("bob")).unwrap().channel, ChannelId::new("bob-room"));
        assert!(dialog.front_for_account(None).is_none());
    }

    #[test]
    fn copy_and_restore_keep_the_entire_draft_reply_and_reason() {
        let ctx = Context::default();
        let size = egui::vec2(480.0, 320.0);
        let mut dialog = UnsentMessages::default();
        let original = long_message("room");
        dialog.push(original.clone());
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = frame(&ctx, &mut dialog, size, true, Vec::new()).0;
        }
        let (copied, restored) = click(
            &ctx,
            &mut dialog,
            size,
            true,
            label_rect(&output, "Copy message").unwrap().center(),
        );
        assert!(restored.is_none());
        assert!(copied.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == &original.text)
        }));
        assert_eq!(dialog.messages.len(), 1);
        output = frame(&ctx, &mut dialog, size, true, Vec::new()).0;
        let (_, restored) = click(
            &ctx,
            &mut dialog,
            size,
            true,
            label_rect(&output, "Restore draft").unwrap().center(),
        );
        let restored = restored.expect("the fixed footer restores the full draft");
        assert_eq!(restored.channel, original.channel);
        assert_eq!(restored.text, original.text);
        assert_eq!(restored.reply_to_msg_id, original.reply_to_msg_id);
        assert_eq!(restored.reason, original.reason);
        assert_eq!(
            restored.reply_info().unwrap().parent_msg_body,
            original.reply_info().unwrap().parent_msg_body
        );
        assert!(dialog.front().is_none());
    }

    #[test]
    fn keyboard_actions_copy_and_restore_long_drafts_in_short_windows() {
        fn key_event(key: egui::Key, pressed: bool) -> egui::Event {
            egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }
        }

        fn tab_to_action(
            ctx: &Context,
            dialog: &mut UnsentMessages,
            size: egui::Vec2,
            label: &str,
        ) {
            // Window movement and eight resize handles precede its action buttons.
            for _ in 0..24 {
                let output = frame(
                    ctx,
                    dialog,
                    size,
                    true,
                    vec![
                        key_event(egui::Key::Tab, false),
                        key_event(egui::Key::Tab, true),
                    ],
                )
                .0;
                assert!(
                    label_rect(&output, label).is_some(),
                    "the footer remains visible"
                );
                let action_focused = output.platform_output.events.iter().any(|event| {
                    matches!(event, egui::output::OutputEvent::FocusGained(info)
                        if info.typ == egui::WidgetType::Button && info.enabled
                            && info.label.as_deref() == Some(label))
                });
                if action_focused {
                    frame(
                        ctx,
                        dialog,
                        size,
                        true,
                        vec![key_event(egui::Key::Tab, false)],
                    );
                    return;
                }
            }
            panic!("Tab could not reach {label}");
        }

        for activate in [egui::Key::Space, egui::Key::Enter] {
            let ctx = Context::default();
            let size = egui::vec2(480.0, 320.0);
            let mut dialog = UnsentMessages::default();
            let original = long_message("room");
            dialog.push(original.clone());
            for _ in 0..4 {
                frame(&ctx, &mut dialog, size, true, Vec::new());
            }
            tab_to_action(&ctx, &mut dialog, size, "Copy message");
            let mut copied = false;
            for pressed in [true, false] {
                let (output, restored) = frame(
                    &ctx,
                    &mut dialog,
                    size,
                    true,
                    vec![key_event(activate, pressed)],
                );
                assert!(restored.is_none());
                copied |= output.platform_output.commands.iter().any(|command| {
                    matches!(command, egui::OutputCommand::CopyText(text) if text == &original.text)
                });
            }
            assert!(
                copied,
                "{activate:?} did not copy the focused recovery action"
            );
            assert_eq!(dialog.front().unwrap().text, original.text);
            tab_to_action(&ctx, &mut dialog, size, "Restore draft");
            let restored = frame(
                &ctx,
                &mut dialog,
                size,
                true,
                vec![key_event(activate, true)],
            )
            .1
            .expect("keyboard restores the focused draft");
            assert_eq!(restored.text, original.text);
            assert_eq!(restored.reason, original.reason);
            assert_eq!(
                restored.reply_info().unwrap().parent_msg_body,
                original.reply_info().unwrap().parent_msg_body
            );
            assert!(dialog.front().is_none());
        }
    }

    #[test]
    fn dismiss_after_scrolling_opens_the_next_recovered_message_at_the_top() {
        let ctx = Context::default();
        let size = egui::vec2(480.0, 320.0);
        let mut dialog = UnsentMessages::default();
        dialog.push(long_message("first"));
        dialog.push(long_message("second"));
        for _ in 0..4 {
            frame(&ctx, &mut dialog, size, true, Vec::new());
        }
        let bounds = ctx
            .memory(|memory| memory.area_rect(egui::Id::new("unsent_messages")))
            .unwrap();
        let pointer = egui::pos2(bounds.center().x, bounds.top() + 60.0);
        frame(
            &ctx,
            &mut dialog,
            size,
            true,
            vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -100_000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let output = frame(&ctx, &mut dialog, size, true, Vec::new()).0;
        assert!(
            label_rect(&output, "Message to #first was not sent").is_none(),
            "the body did not scroll"
        );
        let (_, restored) = click(
            &ctx,
            &mut dialog,
            size,
            true,
            label_rect(&output, "Dismiss")
                .expect("dismiss stays outside the scroll area")
                .center(),
        );
        assert!(restored.is_none());
        assert_eq!(dialog.front().unwrap().channel, ChannelId::new("second"));
        let output = frame(&ctx, &mut dialog, size, true, Vec::new()).0;
        assert!(label_rect(&output, "Message to #second was not sent").is_some());
        assert!(label_rect(&output, "Dismiss").is_some());
    }
}
