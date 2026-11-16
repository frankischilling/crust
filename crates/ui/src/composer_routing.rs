use super::*;
use crate::widgets::reply_thread::ThreadKey;

#[derive(Clone)]
enum ChatDestination {
    Classic,
    Pane(usize),
    Closed(usize),
    Inactive(ChannelId),
    InactivePane((u64, ChannelId)),
    Detached(usize),
    ClosedDetached(usize),
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
        if let Some(index) = self.detached_chats.open.iter().position(|window| window.pane.id == *pane_id && &window.pane.channel == channel
            && (!channel.is_twitch() || &window.account_id == account_id)) {
            return Some(ChatDestination::Detached(index));
        }
        if let Some(index) = self.detached_chats.closed.iter().position(|window| window.pane.id == *pane_id && &window.pane.channel == channel
            && (!channel.is_twitch() || &window.account_id == account_id)) {
            return Some(ChatDestination::ClosedDetached(index));
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
            ChatDestination::Detached(index) => &self.detached_chats.open[*index].pane.input_buf,
            ChatDestination::ClosedDetached(index) => &self.detached_chats.closed[*index].pane.input_buf,
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
            ChatDestination::Detached(index) => &mut self.detached_chats.open[index].pane.input_buf,
            ChatDestination::ClosedDetached(index) => &mut self.detached_chats.closed[index].pane.input_buf,
        }
    }

    fn can_reveal_chat_destination(&self, destination: &ChatDestination) -> bool {
        if self.egui_ctx.is_none() {
            return false;
        }
        match destination {
            ChatDestination::Detached(_) => return true,
            ChatDestination::ClosedDetached(_) => {
                return self.detached_chats.open.len() < crust_core::model::MAX_DETACHED_CHATS;
            }
            _ if self.pending_reply.is_some() => return false,
            ChatDestination::Classic | ChatDestination::Pane(_) => return true,
            _ => {}
        }
        let pane_id = match destination {
            ChatDestination::Closed(index) => self.split_panes.closed[*index].pane.id,
            ChatDestination::Inactive(channel) => self.inactive_classic_drafts[channel].id,
            ChatDestination::InactivePane((id, _)) => *id,
            _ => return false,
        };
        self.split_panes.panes.len() < MAX_CHAT_PANES
            || self.split_panes.panes.iter().any(|pane| pane.id == pane_id)
    }

    fn reveal_chat_destination(
        &mut self,
        channel: &ChannelId,
        target: &crust_core::ComposerTarget,
        destination: ChatDestination,
    ) -> Option<ChatDestination> {
        let ctx = self.egui_ctx.clone()?;
        self.cancel_pending_layout_restore();
        match destination {
            ChatDestination::Detached(index) => {
                let id = self.detached_chats.open[index].pane.id;
                return self.focus_detached_composer(id, &ctx)
                    .then(|| self.chat_destination(channel, target)).flatten();
            }
            ChatDestination::ClosedDetached(index) => {
                let id = self.detached_chats.closed[index].pane.id;
                return self.focus_detached_composer(id, &ctx)
                    .then(|| self.chat_destination(channel, target)).flatten();
            }
            ChatDestination::Classic | ChatDestination::Pane(_) => {}
            ChatDestination::Closed(index) => {
                let closed = self.split_panes.closed.remove(index)?;
                self.split_panes.closed.push_back(closed);
                self.reopen_closed_split(&ctx);
            }
            ChatDestination::Inactive(_)
                if self.split_panes.panes.is_empty()
                    && self.single_pane_identity.is_none_or(|(id, _)|
                        !self.inactive_pane_drafts.contains_key(&(id, channel.clone()))) =>
            {
                self.activate_channel(channel.clone());
            }
            ChatDestination::InactivePane((id, _))
                if self.split_panes.panes.is_empty()
                    && self.single_pane_identity.is_some_and(|(current, _)| current == id) =>
            {
                self.activate_channel(channel.clone());
            }
            hidden => {
                let (draft, was_classic) = match hidden {
                    ChatDestination::Inactive(channel) =>
                        (self.inactive_classic_drafts.remove(&channel)?, true),
                    ChatDestination::InactivePane(key) =>
                        (self.inactive_pane_drafts.remove(&key)?, false),
                    _ => return None,
                };
                let mut search = self.message_search.get(channel).cloned().unwrap_or_default();
                search.set_pane(Some(draft.id));
                if was_classic {
                    crate::widgets::chat_input::migrate_composer_events(&ctx,
                        chat_composer_id(None), chat_composer_id(Some(draft.id)), channel);
                }
                // Reopen immediately so a full closed-view history loses no other draft.
                self.split_panes.closed.push_back(ClosedPane {
                    pane: Pane {
                        id: draft.id,
                        channel: channel.clone(),
                        input_buf: draft.input_buf,
                        view_scope: draft.view_scope,
                        search,
                        filters: self.single_pane_filters.get(channel).cloned().unwrap_or_default(),
                        frac: 1.0,
                    },
                    index: self.split_panes.focused,
                    position: None,
                    reply: draft.reply,
                });
                self.reopen_closed_split(&ctx);
            }
        }
        let destination = self.chat_destination(channel, target)?;
        match destination {
            ChatDestination::Classic => {
                self.mark_channel_read(channel);
                self.focus_chat_composer(&ctx);
            }
            ChatDestination::Pane(index) => {
                self.split_panes.focused = index;
                self.sync_split_focus(&ctx);
            }
            _ => return None,
        }
        self.focus_main_viewport();
        ctx.request_repaint();
        Some(destination)
    }

    pub(super) fn can_restore_unsent(
        &self,
        message: &UnsentMessage,
        activate_composer: bool,
    ) -> bool {
        if !message.belongs_to(self.state.auth.user_id.as_deref()) {
            return false;
        }
        let Some(target) = &message.composer else {
            return self.unsent_input_pane(message).is_some();
        };
        if let Some(key) = ThreadKey::from_target(&message.channel, target) {
            return self.reply_threads.can_restore(&key, activate_composer);
        }
        let Some(destination) = self.chat_destination(&message.channel, target) else {
            return false;
        };
        if activate_composer && !self.can_reveal_chat_destination(&destination) {
            return false;
        }
        if !self.destination_draft(&destination).is_empty() {
            return false;
        }
        match &destination {
            ChatDestination::Detached(index) => return self.detached_chats.open[*index].reply.is_none(),
            ChatDestination::ClosedDetached(index) => return self.detached_chats.closed[*index].reply.is_none(),
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
        if !activate_composer && (message.reply.is_some() || message.reply_to_msg_id.is_some()) {
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
        activate_composer: bool,
    ) -> bool {
        if !self.can_restore_unsent(message, activate_composer) {
            return false;
        }
        let Some(target) = &message.composer else {
            let pane = self.unsent_input_pane(message);
            if !self.restore_unsent_message(message) {
                return false;
            }
            if activate_composer {
                if let Some(ctx) = self.egui_ctx.clone() {
                    if let Some(Some(index)) = pane {
                        self.split_panes.focused = index;
                        self.sync_split_focus(&ctx);
                    } else {
                        self.focus_chat_composer(&ctx);
                    }
                    self.focus_main_viewport();
                    ctx.request_repaint();
                }
            }
            return true;
        };
        if let Some(key) = ThreadKey::from_target(&message.channel, target) {
            let restored = self.reply_threads.restore_draft(
                key,
                &message.text,
                message.reply.as_ref(),
                activate_composer,
            );
            if restored && activate_composer {
                self.focus_main_viewport();
                if let Some(ctx) = &self.egui_ctx { ctx.request_repaint(); }
            }
            return restored;
        }
        let Some(destination) = self.chat_destination(&message.channel, target) else {
            return false;
        };
        let destination = if activate_composer {
            let Some(visible) = self.reveal_chat_destination(&message.channel, target, destination) else {
                return false;
            };
            visible
        } else {
            destination
        };
        self.destination_draft_mut(destination.clone())
            .clone_from(&message.text);
        match destination {
            ChatDestination::Detached(index) => self.detached_chats.open[index].reply = message.reply_info(),
            ChatDestination::ClosedDetached(index) => self.detached_chats.closed[index].reply = message.reply_info(),
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

    pub(super) fn open_reply_thread(&mut self, message: ChatMessage) -> bool {
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
            return false;
        }
        self.focus_main_viewport();
        true
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
            emote_picker_open: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (CrustApp, Context, mpsc::Receiver<AppCommand>) {
        let (commands, receiver) = mpsc::channel(64);
        let (_, events) = mpsc::channel(64);
        let mut app = CrustApp::with_event_channels(commands, events);
        let ctx = Context::default();
        app.egui_ctx = Some(ctx.clone());
        for name in ["first", "second", "third"] {
            app.state.join_channel(ChannelId::new(name));
        }
        app.activate_channel(ChannelId::new("first"));
        (app, ctx, receiver)
    }

    fn rejection(channel: &ChannelId, target: crust_core::ComposerTarget) -> UnsentMessage {
        UnsentMessage {
            channel: channel.clone(),
            text: "recovered draft".into(),
            reply_to_msg_id: None,
            reply: None,
            composer: Some(target),
            reason: "The message was not sent.".into(),
        }
    }

    fn assert_visible_draft(app: &CrustApp, message: &UnsentMessage) {
        assert_eq!(app.state.active_channel.as_ref(), Some(&message.channel));
        match app.chat_destination(&message.channel, message.composer.as_ref().unwrap()) {
            Some(ChatDestination::Classic) => assert_eq!(app.chat_input_buf, message.text),
            Some(ChatDestination::Pane(index)) => {
                assert_eq!(app.split_panes.focused, index);
                assert_eq!(app.split_panes.panes[index].input_buf, message.text);
            }
            _ => panic!("explicit restoration must show the original composer"),
        }
    }

    #[test]
    fn explicit_restore_reveals_an_inactive_classic_draft() {
        let (mut app, _ctx, _commands) = fixture();
        let first = ChannelId::new("first");
        let target = app.chat_composer_target(None);
        app.activate_channel(ChannelId::new("second"));
        app.chat_input_buf = "keep current draft".into();
        let message = rejection(&first, target);
        assert!(app.restore_targeted_unsent(&message, true));
        assert_visible_draft(&app, &message);
        assert_eq!(app.inactive_classic_drafts[&ChannelId::new("second")].input_buf,
            "keep current draft");
    }

    #[test]
    fn explicit_restore_focuses_a_background_pane_with_its_reply() {
        let (mut app, ctx, _commands) = fixture();
        app.split_active_pane(SplitAxis::Horizontal, &ctx);
        let first = ChannelId::new("first");
        let target = app.chat_composer_target(Some(0));
        let mut message = rejection(&first, target);
        message.reply = Some(ReplyInfo { parent_msg_id: "parent".into(),
            parent_msg_body: "retained parent".into(), ..Default::default() });
        app.split_panes.panes[1].input_buf = "keep other draft".into();
        assert!(!app.can_restore_unsent(&message, false));
        assert!(app.can_restore_unsent(&message, true));
        assert!(app.restore_targeted_unsent(&message, true));
        assert_visible_draft(&app, &message);
        assert_eq!(app.split_panes.panes[1].input_buf, "keep other draft");
        assert_eq!(app.pending_reply.as_ref().unwrap().info.parent_msg_body, "retained parent");
    }

    #[test]
    fn explicit_restore_reopens_the_original_closed_pane() {
        let (mut app, ctx, _commands) = fixture();
        app.chat_input_buf = "keep first draft".into();
        app.split_active_pane(SplitAxis::Horizontal, &ctx);
        app.activate_channel(ChannelId::new("second"));
        let target = app.chat_composer_target(Some(1));
        let message = rejection(&ChannelId::new("second"), target);
        app.split_active_pane(SplitAxis::Vertical, &ctx);
        app.activate_channel(ChannelId::new("third"));
        let third_id = app.split_panes.panes[2].id;
        app.split_panes.panes[2].input_buf = "keep third draft".into();
        app.close_split_pane(1, &ctx);
        app.close_split_pane(1, &ctx);
        assert!(app.restore_targeted_unsent(&message, true));
        assert_visible_draft(&app, &message);
        assert_eq!(app.split_panes.panes[0].input_buf, "keep first draft");
        let third = app.split_panes.closed.iter().find(|pane| pane.pane.id == third_id).unwrap();
        assert_eq!(third.pane.input_buf, "keep third draft");
    }

    #[test]
    fn explicit_restore_reveals_a_hidden_draft_in_its_original_pane() {
        let (mut app, ctx, _commands) = fixture();
        app.split_active_pane(SplitAxis::Horizontal, &ctx);
        let target = app.chat_composer_target(Some(1));
        let message = rejection(&ChannelId::new("first"), target);
        app.activate_channel(ChannelId::new("third"));
        app.split_panes.panes[1].input_buf = "keep third draft".into();
        let original_id = app.split_panes.panes[1].id;
        for index in 0..16 {
            app.split_active_pane(SplitAxis::Horizontal, &ctx);
            app.split_panes.panes[2].input_buf = format!("closed draft {index}");
            app.close_split_pane(2, &ctx);
        }
        assert!(app.restore_targeted_unsent(&message, true));
        assert_visible_draft(&app, &message);
        assert_eq!(app.split_panes.panes[1].id, original_id);
        assert_eq!(app.split_panes.panes.len(), 2);
        assert_eq!(app.inactive_pane_drafts[&(original_id, ChannelId::new("third"))].input_buf,
            "keep third draft");
        assert_eq!(app.split_panes.closed.len(), 16);
        for (index, closed) in app.split_panes.closed.iter().enumerate() {
            assert_eq!(closed.pane.input_buf, format!("closed draft {index}"));
        }
    }

    #[test]
    fn explicit_restore_keeps_a_closed_draft_recoverable_at_the_pane_limit() {
        let (mut app, ctx, _commands) = fixture();
        app.split_active_pane(SplitAxis::Horizontal, &ctx);
        let target = app.chat_composer_target(Some(1));
        let message = rejection(&ChannelId::new("first"), target);
        app.close_split_pane(1, &ctx);
        app.seed_active_split(&ctx);
        while app.split_panes.panes.len() < MAX_CHAT_PANES {
            app.split_panes.add_pane(ChannelId::new("second"), None);
        }
        assert!(!app.can_restore_unsent(&message, true));
        assert!(!app.restore_targeted_unsent(&message, true));
        assert!(app.split_panes.closed.back().unwrap().pane.input_buf.is_empty());
        assert_eq!(app.split_panes.panes.len(), MAX_CHAT_PANES);
    }

    #[test]
    fn automatic_restore_retains_an_inactive_draft_without_changing_focus() {
        let (mut app, _ctx, _commands) = fixture();
        let first = ChannelId::new("first");
        let target = app.chat_composer_target(None);
        let second = ChannelId::new("second");
        app.activate_channel(second.clone());
        let message = rejection(&first, target);
        assert!(app.restore_targeted_unsent(&message, false));
        assert_eq!(app.state.active_channel, Some(second));
        assert!(app.chat_input_buf.is_empty());
        assert_eq!(app.inactive_classic_drafts[&first].input_buf, message.text);
    }
}
