use std::collections::VecDeque;

use crust_core::{ChannelId, ReplyInfo};
use egui::{Context, RichText};

use crate::theme as t;

#[derive(Clone, Debug)]
pub(crate) struct UnsentMessage {
    pub channel: ChannelId,
    pub text: String,
    pub reply_to_msg_id: Option<String>,
    pub reply: Option<ReplyInfo>,
    pub reason: String,
}

impl UnsentMessage {
    pub fn reply_info(&self) -> Option<ReplyInfo> {
        self.reply.clone().or_else(|| {
            self.reply_to_msg_id.as_ref().map(|id| ReplyInfo {
                parent_msg_id: id.clone(),
                parent_user_login: String::new(),
                parent_display_name: "unknown".to_owned(),
                parent_msg_body: String::new(),
            })
        })
    }
}

#[derive(Default)]
pub(crate) struct UnsentMessages {
    messages: VecDeque<UnsentMessage>,
}

impl UnsentMessages {
    pub fn push(&mut self, message: UnsentMessage) {
        self.messages.push_back(message);
    }

    pub fn front(&self) -> Option<&UnsentMessage> {
        self.messages.front()
    }

    pub fn show(&mut self, ctx: &Context, can_restore: bool) -> Option<UnsentMessage> {
        let count = self.messages.len();
        let message = self.messages.front_mut()?;
        let mut restore = false;
        let mut dismiss = false;
        egui::Window::new("Unsent messages")
            .id(egui::Id::new("unsent_messages"))
            .default_width(420.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("Message to #{} was not sent", message.channel.display_name()))
                    .strong().color(t::red()));
                ui.label(&message.reason);
                if count > 1 {
                    ui.label(format!("{count} messages are waiting to be recovered."));
                }
                if let Some(reply) = message.reply.as_ref() {
                    ui.label(format!("Reply to @{}: {}", reply.parent_display_name, reply.parent_msg_body));
                }
                ui.add(egui::TextEdit::multiline(&mut message.text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(3));
                if !can_restore {
                    ui.label("Focus this channel with an empty composer and no selected reply to restore the draft.");
                }
                ui.horizontal_wrapped(|ui| {
                    restore = ui.add_enabled(can_restore, egui::Button::new("Restore draft")).clicked();
                    if ui.button("Copy message").clicked() {
                        ui.ctx().copy_text(message.text.clone());
                    }
                    dismiss = ui.button("Dismiss").clicked();
                });
            });
        if restore {
            self.messages.pop_front()
        } else {
            if dismiss {
                self.messages.pop_front();
            }
            None
        }
    }
}
