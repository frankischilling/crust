use super::*;
use crate::widgets::reply_thread::ThreadKey;

#[derive(Clone)]
enum ChatDestination {
    Classic,
    Pane(usize),
    Closed(usize),
    Inactive(ChannelId),
    InactivePane((u64, ChannelId)),
}

impl CrustApp {
    pub(super) fn chat_composer_target(
        &mut self,
        input_pane: Option<usize>,
    ) -> crust_core::ComposerTarget {
        if input_pane.is_none() {
            self.sync_classic_draft_owner();
        }
        let pane_id = if let Some(index) = input_pane {
            self.split_panes.panes[index].id
        } else {
            self.single_pane_identity
                .get_or_insert_with(|| (SplitPanes::next_id(), None))
                .0
        };
        crust_core::ComposerTarget::Chat {
            account_id: self.state.auth.user_id.clone(),
            pane_id,
        }
    }

    fn chat_destination(
        &self,
        channel: &ChannelId,
        target: &crust_core::ComposerTarget,
    ) -> Option<ChatDestination> {
        let crust_core::ComposerTarget::Chat {
            account_id,
            pane_id,
        } = target
        else {
            return None;
        };
        if channel.is_twitch() && account_id != &self.state.auth.user_id {
            return None;
        }
        if self.split_panes.panes.len() > 1 {
            if let Some(index) = self
                .split_panes
                .panes
                .iter()
                .position(|pane| pane.id == *pane_id && &pane.channel == channel)
            {
                return Some(ChatDestination::Pane(index));
            }
        } else if self
            .single_pane_identity
            .is_some_and(|(id, _)| id == *pane_id)
            && self
                .classic_draft_channel
                .as_ref()
                .or(self.state.active_channel.as_ref())
                == Some(channel)
        {
            return Some(ChatDestination::Classic);
        }
        let pane_key = (*pane_id, channel.clone());
        let pane_reachable = self.split_panes.panes.iter().any(|pane| pane.id == *pane_id)
            || self.split_panes.closed.iter().any(|closed| closed.pane.id == *pane_id)
            || self.single_pane_identity.is_some_and(|(id, _)| id == *pane_id)
            || self.inactive_classic_drafts.values().any(|draft| draft.id == *pane_id);
        if pane_reachable && self.inactive_pane_drafts.contains_key(&pane_key) {
            return Some(ChatDestination::InactivePane(pane_key));
        }
        if self
            .inactive_classic_drafts
            .get(channel)
            .is_some_and(|draft| draft.id == *pane_id)
        {
            return Some(ChatDestination::Inactive(channel.clone()));
        }
        self.split_panes
            .closed
            .iter()
            .position(|closed| closed.pane.id == *pane_id && &closed.pane.channel == channel)
            .map(ChatDestination::Closed)
    }

    fn destination_draft(&self, destination: &ChatDestination) -> &String {
        match destination {
            ChatDestination::Classic => &self.chat_input_buf,
            ChatDestination::Pane(index) => &self.split_panes.panes[*index].input_buf,
            ChatDestination::Closed(index) => &self.split_panes.closed[*index].pane.input_buf,
            ChatDestination::Inactive(channel) => &self.inactive_classic_drafts[channel].input_buf,
            ChatDestination::InactivePane(key) => &self.inactive_pane_drafts[key].input_buf,
        }
    }

    fn destination_draft_mut(&mut self, destination: ChatDestination) -> &mut String {
        match destination {
            ChatDestination::Classic => &mut self.chat_input_buf,
            ChatDestination::Pane(index) => &mut self.split_panes.panes[index].input_buf,
            ChatDestination::Closed(index) => &mut self.split_panes.closed[index].pane.input_buf,
            ChatDestination::Inactive(channel) => {
                &mut self
                    .inactive_classic_drafts
                    .get_mut(&channel)
                    .unwrap()
                    .input_buf
            }
            ChatDestination::InactivePane(key) => {
                &mut self.inactive_pane_drafts.get_mut(&key).unwrap().input_buf
            }
        }
    }

    pub(super) fn can_restore_unsent(
        &self,
        message: &UnsentMessage,
        activate_thread: bool,
    ) -> bool {
        if !message.belongs_to(self.state.auth.user_id.as_deref()) {
            return false;
        }
        let Some(target) = &message.composer else {
            return self.unsent_input_pane(message).is_some();
        };
        if let Some(key) = ThreadKey::from_target(&message.channel, target) {
            return self.reply_threads.can_restore(&key, activate_thread);
        }
        let Some(destination) = self.chat_destination(&message.channel, target) else {
            return false;
        };
        if !self.destination_draft(&destination).is_empty() {
            return false;
        }
        match &destination {
            ChatDestination::Inactive(channel) => {
                return self.inactive_classic_drafts[channel].reply.is_none()
            }
            ChatDestination::Closed(index) => {
                return self.split_panes.closed[*index].reply.is_none()
            }
            ChatDestination::InactivePane(key) => {
                return self.inactive_pane_drafts[key].reply.is_none()
            }
            _ if self.pending_reply.is_some() => return false,
            _ => {}
        }
        if message.reply.is_some() || message.reply_to_msg_id.is_some() {
            match destination {
                ChatDestination::Pane(index) if index != self.split_panes.focused => return false,
                _ => {}
            }
        }
        true
    }

    pub(super) fn restore_targeted_unsent(
        &mut self,
        message: &UnsentMessage,
        activate_thread: bool,
    ) -> bool {
        if !self.can_restore_unsent(message, activate_thread) {
            return false;
        }
        let Some(target) = &message.composer else {
            return self.restore_unsent_message(message);
        };
        if let Some(key) = ThreadKey::from_target(&message.channel, target) {
            return self.reply_threads.restore_draft(
                key,
                &message.text,
                message.reply.as_ref(),
                activate_thread,
            );
        }
        let Some(destination) = self.chat_destination(&message.channel, target) else {
            return false;
        };
        self.destination_draft_mut(destination.clone())
            .clone_from(&message.text);
        match destination {
            ChatDestination::Inactive(channel) => {
                self.inactive_classic_drafts
                    .get_mut(&channel)
                    .unwrap()
                    .reply = message.reply_info()
            }
            ChatDestination::Closed(index) => {
                self.split_panes.closed[index].reply = message.reply_info()
            }
            ChatDestination::InactivePane(key) => {
                self.inactive_pane_drafts.get_mut(&key).unwrap().reply = message.reply_info()
            }
            _ => {
                self.pending_reply = message.reply_info().map(|info| PendingReply {
                    channel: message.channel.clone(),
                    info,
                })
            }
        }
        true
    }

    pub(super) fn restore_failed_submission(
        &mut self,
        channel: &ChannelId,
        draft: String,
        reply: Option<ReplyInfo>,
        target: crust_core::ComposerTarget,
    ) {
        let restored = if let Some(key) = ThreadKey::from_target(channel, &target) {
            self.reply_threads
                .restore_draft(key, &draft, reply.as_ref(), false)
        } else if let Some(destination) = self.chat_destination(channel, &target) {
            if self.destination_draft(&destination).is_empty() {
                self.destination_draft_mut(destination).clone_from(&draft);
                true
            } else {
                false
            }
        } else {
            false
        };
        if !restored {
            self.unsent_messages.push(UnsentMessage {
                channel: channel.clone(),
                text: draft,
                reply_to_msg_id: reply.as_ref().map(|reply| reply.parent_msg_id.clone()),
                reply,
                composer: Some(target),
                reason: "The command queue is unavailable. Your draft has been kept.".into(),
            });
        }
    }

    pub(super) fn append_upload_to_composer(
        &mut self,
        channel: &ChannelId,
        target: Option<crust_core::ComposerTarget>,
        url: &str,
    ) -> bool {
        let appended = if let Some(target) = &target {
            if channel.is_twitch() && target.account_id() != self.state.auth.user_id.as_deref() {
                false
            } else if let Some(key) = ThreadKey::from_target(channel, target) {
                self.reply_threads.append_upload(key, url)
            } else if let Some(destination) = self.chat_destination(channel, target) {
                append_upload_url(self.destination_draft_mut(destination), url);
                true
            } else {
                false
            }
        } else {
            append_upload_url(self.input_buf_for_channel_mut(channel), url);
            true
        };
        if !appended {
            self.unsent_messages.push(UnsentMessage { channel: channel.clone(), text: format!("{url} "),
                reply_to_msg_id: None, reply: None, composer: target,
                reason: "The upload finished after its composer closed or the account changed. The link has been kept.".into() });
        }
        appended
    }

    pub(super) fn open_reply_thread(&mut self, message: ChatMessage) {
        let empty = VecDeque::new();
        let source = self
            .state
            .channels
            .get(&message.channel)
            .map_or(&empty, |channel| &channel.messages);
        if !self
            .reply_threads
            .open(self.state.auth.user_id.as_deref(), message, source)
        {
            self.push_event_toast(
                "The thread could not be opened. Close an unused thread draft and try again."
                    .into(),
                t::red(),
                false,
            );
        }
    }

    pub(super) fn show_reply_thread(&mut self, ctx: &Context, animations_allowed: bool) {
        let Some(key) = self
            .reply_threads
            .active_key(self.state.auth.user_id.as_deref())
            .cloned()
        else {
            return;
        };
        self.ensure_input_emote_catalog(&key.channel);
        let empty = VecDeque::new();
        let source = self
            .state
            .channels
            .get(&key.channel)
            .map_or(&empty, |channel| &channel.messages);
        let preview = MessagePreviewContext {
            source_channels: Some(&self.state.channels),
            viewer_login: self.state.auth.username.as_deref(),
            show_timestamps: self.show_timestamps,
            show_timestamp_seconds: self.show_timestamp_seconds,
            use_24h_timestamps: self.use_24h_timestamps,
            animate_emotes: animations_allowed,
            hide_link_previews: self.streamer_mode_active && self.streamer_hide_link_previews,
            highlight_rules: &self.highlight_rules,
            mod_action_presets: &self.mod_action_presets,
            ..MessagePreviewContext::new(&self.emote_bytes, &self.cmd_tx, &self.link_previews)
        };
        let input = ChatInput {
            id: Some(key.composer_id()),
            hotkey_bindings: &self.hotkey_bindings,
            channel: &key.channel,
            logged_in: self.state.auth.logged_in,
            username: self.state.auth.username.as_deref(),
            emote_catalog: self
                .input_emote_catalogs
                .get(&key.channel)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            emote_bytes: &self.emote_bytes,
            pending_reply: None,
            message_history: &self.message_history,
            slash_usage_counts: &self.slash_usage_counts,
            known_channels: &self.state.channel_order,
            chatters: self
                .sorted_chatters
                .get(&key.channel)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            prevent_overlong_twitch_messages: self.prevent_overlong_twitch_messages,
            animate_emotes: animations_allowed,
            accept_dropped_files: false,
        };
        let thread_owned_focus = self.reply_threads.owns_keyboard(ctx, self.state.auth.user_id.as_deref());
        let output = self.reply_threads.show(
            ctx,
            self.state.auth.user_id.as_deref(),
            source,
            preview,
            input,
            &self.emote_picker.preferences(),
        );
        if self.reply_threads.active_key(self.state.auth.user_id.as_deref()).is_none()
            && (thread_owned_focus || ctx.memory(|memory| memory.focused()).is_none()) {
            self.focus_chat_composer(ctx);
        }
        self.apply_message_preview_actions(output.actions);
        if let Some(submission) = output.submission {
            let target = submission.key.target();
            self.submit_composer_input(
                &submission.key.channel,
                submission.draft,
                Some(submission.reply),
                target,
            );
        }
        for (key, upload) in output.uploads {
            let composer = Some(key.target());
            self.send_cmd(AppCommand::UploadImage {
                channel: key.channel,
                bytes: upload.bytes,
                format: upload.format,
                source_path: upload.source_path.map(|path| path.display().to_string()),
                composer,
            });
        }
        if let Some(word) = output.add_to_dictionary {
            self.send_cmd(AppCommand::AddWordToDictionary { word });
        }
        if let Some(preferences) = output.picker_preferences {
            self.emote_picker.apply_preferences(&preferences);
        }
    }
}

fn append_upload_url(draft: &mut String, url: &str) {
    if !draft.is_empty() && !draft.ends_with(' ') {
        draft.push(' ');
    }
    draft.push_str(url);
    draft.push(' ');
}
