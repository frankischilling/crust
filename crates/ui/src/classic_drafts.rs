use super::*;

#[derive(Clone)]
pub(super) struct ClassicDraft {
    pub id: u64,
    pub view_scope: Option<u64>,
    pub input_buf: String,
    pub reply: Option<ReplyInfo>,
}

impl CrustApp {
    pub(super) fn sync_classic_draft_owner(&mut self) {
        if !self.split_panes.panes.is_empty() {
            return;
        }
        let next = self
            .state
            .active_channel
            .clone()
            .filter(|channel| !channel.is_virtual());
        if self.classic_draft_channel == next {
            if next.is_some() {
                self.single_pane_identity
                    .get_or_insert_with(|| (SplitPanes::next_id(), None));
            }
            return;
        }
        // Before the first render, an existing buffer belongs to the first
        // selected channel. Later changes always have an explicit owner.
        if self.classic_draft_channel.is_none()
            && next.is_some()
            && !next
                .as_ref()
                .is_some_and(|channel| self.inactive_classic_drafts.contains_key(channel))
            && (!self.chat_input_buf.is_empty()
                || self.single_pane_identity.is_some()
                || self
                    .pending_reply
                    .as_ref()
                    .is_some_and(|reply| Some(&reply.channel) == next.as_ref()))
        {
            self.classic_draft_channel = next;
            self.single_pane_identity
                .get_or_insert_with(|| (SplitPanes::next_id(), None));
            return;
        }
        let previous_id = self.single_pane_identity.map(|(id, _)| id);
        self.stash_classic_draft();
        self.classic_draft_channel = next.clone();
        let Some(channel) = next else {
            return;
        };
        let pane_draft =
            previous_id.and_then(|id| self.inactive_pane_drafts.remove(&(id, channel.clone())));
        let from_pane = pane_draft.is_some();
        let draft = if let Some(draft) = pane_draft {
            if let Some(previous) = self.inactive_classic_drafts.remove(&channel) {
                if previous.id != draft.id {
                    self.retain_replaced_classic_draft(channel.clone(), previous);
                }
            }
            draft
        } else {
            self.inactive_classic_drafts
                .remove(&channel)
                .unwrap_or_else(|| ClassicDraft {
                    id: SplitPanes::next_id(),
                    view_scope: None,
                    input_buf: String::new(),
                    reply: None,
                })
        };
        self.single_pane_identity = Some((draft.id, draft.view_scope));
        self.chat_input_buf = draft.input_buf;
        self.pending_reply = draft.reply.map(|info| PendingReply {
            channel: channel.clone(),
            info,
        });
        if let Some(ctx) = &self.egui_ctx {
            if from_pane {
                crate::widgets::chat_input::migrate_composer_events(
                    ctx,
                    chat_composer_id(Some(draft.id)),
                    chat_composer_id(None),
                    &channel,
                );
            }
            let state =
                egui::TextEdit::load_state(ctx, inactive_draft_state_id(draft.id, &channel))
                    .unwrap_or_default();
            egui::TextEdit::store_state(ctx, chat_composer_id(None), state);
        }
    }

    pub(super) fn stash_classic_draft(&mut self) {
        let Some(channel) = self.classic_draft_channel.take() else {
            return;
        };
        let (id, view_scope) = self
            .single_pane_identity
            .take()
            .unwrap_or_else(|| (SplitPanes::next_id(), None));
        if let Some(ctx) = &self.egui_ctx {
            if let Some(state) = egui::TextEdit::load_state(ctx, chat_composer_id(None)) {
                egui::TextEdit::store_state(ctx, inactive_draft_state_id(id, &channel), state);
            }
        }
        let reply = if self
            .pending_reply
            .as_ref()
            .is_some_and(|reply| reply.channel == channel)
        {
            self.pending_reply.take().map(|reply| reply.info)
        } else {
            None
        };
        let draft = ClassicDraft {
            id,
            view_scope,
            input_buf: std::mem::take(&mut self.chat_input_buf),
            reply,
        };
        if let Some(previous) = self.inactive_classic_drafts.insert(channel.clone(), draft) {
            if previous.id != id {
                self.retain_replaced_classic_draft(channel, previous);
            }
        }
    }

    pub(super) fn retain_replaced_classic_draft(
        &mut self,
        channel: ChannelId,
        draft: ClassicDraft,
    ) -> bool {
        self.retain_hidden_draft(channel, draft, true)
    }

    pub(super) fn retain_hidden_draft(
        &mut self,
        channel: ChannelId,
        draft: ClassicDraft,
        was_classic: bool,
    ) -> bool {
        if draft.input_buf.is_empty() && draft.reply.is_none() && !self.layout_restore_cancelled {
            return false;
        }
        let mut search = self
            .message_search
            .get(&channel)
            .cloned()
            .unwrap_or_default();
        search.set_pane(Some(draft.id));
        if let Some(ctx) = self.egui_ctx.as_ref().filter(|_| was_classic) {
            crate::widgets::chat_input::migrate_composer_events(
                ctx,
                chat_composer_id(None),
                chat_composer_id(Some(draft.id)),
                &channel,
            );
        }
        self.split_panes.closed.push_back(ClosedPane {
            pane: Pane {
                id: draft.id,
                channel: channel.clone(),
                input_buf: draft.input_buf,
                view_scope: draft.view_scope,
                search,
                filters: self
                    .single_pane_filters
                    .get(&channel)
                    .cloned()
                    .unwrap_or_default(),
                frac: 1.0,
            },
            index: 0,
            position: None,
            reply: draft.reply,
        });
        while self.split_panes.closed.len() > 16 {
            self.split_panes.closed.pop_front();
        }
        true
    }

    pub(super) fn activate_split_channel_draft(&mut self, channel: &ChannelId) {
        if self
            .split_panes
            .panes
            .iter()
            .any(|pane| &pane.channel == channel)
        {
            self.split_panes.activate_channel(channel);
            return;
        }
        let Some(pane) = self.split_panes.panes.get_mut(self.split_panes.focused) else {
            return;
        };
        let old_channel = pane.channel.clone();
        let id = pane.id;
        let draft = ClassicDraft {
            id,
            view_scope: pane.view_scope,
            input_buf: std::mem::take(&mut pane.input_buf),
            reply: if self
                .pending_reply
                .as_ref()
                .is_some_and(|reply| reply.channel == old_channel)
            {
                self.pending_reply.take().map(|reply| reply.info)
            } else {
                None
            },
        };
        if let Some(ctx) = &self.egui_ctx {
            if let Some(state) = egui::TextEdit::load_state(ctx, chat_composer_id(Some(id))) {
                egui::TextEdit::store_state(ctx, inactive_draft_state_id(id, &old_channel), state);
            }
        }
        self.inactive_pane_drafts.insert((id, old_channel), draft);
        let mut from_classic = false;
        let draft = self
            .inactive_pane_drafts
            .remove(&(id, channel.clone()))
            .or_else(|| {
                if self
                    .inactive_classic_drafts
                    .get(channel)
                    .is_some_and(|draft| draft.id == id)
                {
                    from_classic = true;
                    self.inactive_classic_drafts.remove(channel)
                } else {
                    None
                }
            });
        self.split_panes.activate_channel(channel);
        let pane = &mut self.split_panes.panes[self.split_panes.focused];
        if let Some(draft) = draft {
            pane.input_buf = draft.input_buf;
            if let Some(info) = draft.reply {
                self.pending_reply = Some(PendingReply {
                    channel: channel.clone(),
                    info,
                });
            }
        }
        if let Some(ctx) = &self.egui_ctx {
            if from_classic {
                crate::widgets::chat_input::migrate_composer_events(
                    ctx,
                    chat_composer_id(None),
                    chat_composer_id(Some(id)),
                    channel,
                );
            }
            let state = egui::TextEdit::load_state(ctx, inactive_draft_state_id(id, channel))
                .unwrap_or_default();
            egui::TextEdit::store_state(ctx, chat_composer_id(Some(id)), state);
        }
    }

    pub(super) fn remove_split_pane_with_reply(&mut self, index: usize) {
        let Some(pane) = self.split_panes.panes.get(index) else {
            return;
        };
        let reply = if index == self.split_panes.focused
            && self
                .pending_reply
                .as_ref()
                .is_some_and(|reply| reply.channel == pane.channel)
        {
            self.pending_reply.take().map(|reply| reply.info)
        } else {
            None
        };
        if let Some(ctx) = &self.egui_ctx {
            if let Some(state) = egui::TextEdit::load_state(ctx, chat_composer_id(Some(pane.id))) {
                egui::TextEdit::store_state(
                    ctx,
                    inactive_draft_state_id(pane.id, &pane.channel),
                    state,
                );
            }
        }
        self.split_panes.remove_pane(index);
        if let Some(closed) = self.split_panes.closed.back_mut() {
            closed.reply = reply;
        }
    }

    pub(super) fn reopen_existing_pane_draft(&mut self, ctx: &Context) -> Option<ChannelId> {
        let id = self.split_panes.closed.back()?.pane.id;
        let index = self
            .split_panes
            .panes
            .iter()
            .position(|pane| pane.id == id)?;
        let closed = self.split_panes.closed.pop_back()?;
        let pane = &mut self.split_panes.panes[index];
        let channel = pane.channel.clone();
        let reply = if self
            .pending_reply
            .as_ref()
            .is_some_and(|reply| reply.channel == channel)
        {
            self.pending_reply.take().map(|reply| reply.info)
        } else {
            None
        };
        self.inactive_pane_drafts.insert(
            (id, channel.clone()),
            ClassicDraft {
                id,
                view_scope: pane.view_scope,
                input_buf: std::mem::take(&mut pane.input_buf),
                reply,
            },
        );
        if let Some(state) = egui::TextEdit::load_state(ctx, chat_composer_id(Some(id))) {
            egui::TextEdit::store_state(ctx, inactive_draft_state_id(id, &channel), state);
        }
        let fraction = pane.frac;
        *pane = closed.pane;
        pane.frac = fraction;
        self.split_panes.focused = index;
        let channel = pane.channel.clone();
        self.pending_reply = closed.reply.map(|info| PendingReply {
            channel: channel.clone(),
            info,
        });
        Self::restore_closed_pane_cursor(ctx, id, &channel);
        Some(channel)
    }

    pub(super) fn restore_closed_pane_cursor(ctx: &Context, id: u64, channel: &ChannelId) {
        if let Some(state) = egui::TextEdit::load_state(ctx, inactive_draft_state_id(id, channel)) {
            egui::TextEdit::store_state(ctx, chat_composer_id(Some(id)), state);
        }
    }

    pub(super) fn redirect_classic_drafts(&mut self, old: &ChannelId, new: &ChannelId) {
        if old == new {
            return;
        }
        if self.classic_draft_channel.as_ref() == Some(old) {
            if let Some(mut draft) = self.inactive_classic_drafts.remove(new) {
                if self
                    .single_pane_identity
                    .is_some_and(|(id, _)| id == draft.id)
                {
                    let old_id = draft.id;
                    self.reidentify_hidden_draft(new, &mut draft, true);
                    self.retain_hidden_draft(new.clone(), draft, false);
                    self.rekey_live_draft_owner(old_id);
                } else {
                    self.retain_replaced_classic_draft(new.clone(), draft);
                }
            }
            self.classic_draft_channel = Some(new.clone());
        }
        let redirecting: Vec<_> = self
            .split_panes
            .panes
            .iter()
            .filter(|pane| &pane.channel == old)
            .map(|pane| pane.id)
            .collect();
        for id in redirecting {
            if let Some(mut draft) = self.inactive_pane_drafts.remove(&(id, new.clone())) {
                self.reidentify_hidden_draft(new, &mut draft, false);
                self.retain_hidden_draft(new.clone(), draft, false);
                self.rekey_live_draft_owner(id);
            }
        }
        if let Some(draft) = self.inactive_classic_drafts.remove(old) {
            if let Some(ctx) = &self.egui_ctx {
                if let Some(state) =
                    egui::TextEdit::load_state(ctx, inactive_draft_state_id(draft.id, old))
                {
                    egui::TextEdit::store_state(ctx, inactive_draft_state_id(draft.id, new), state);
                }
            }
            if self.inactive_classic_drafts.contains_key(new) {
                if self.retain_replaced_classic_draft(old.clone(), draft) {
                    if let Some(closed) = self.split_panes.closed.back_mut() {
                        closed.pane.channel = new.clone();
                    }
                }
            } else {
                self.inactive_classic_drafts.insert(new.clone(), draft);
            }
        }
        let keys: Vec<_> = self
            .inactive_pane_drafts
            .keys()
            .filter(|(_, channel)| channel == old)
            .cloned()
            .collect();
        for key in keys {
            let mut draft = self.inactive_pane_drafts.remove(&key).unwrap();
            if self
                .inactive_pane_drafts
                .contains_key(&(key.0, new.clone()))
            {
                // Both names have a draft in this pane. Give the superseded
                // draft its own recoverable identity before merging aliases.
                self.reidentify_hidden_draft(old, &mut draft, false);
            }
            if let Some(ctx) = &self.egui_ctx {
                if let Some(state) =
                    egui::TextEdit::load_state(ctx, inactive_draft_state_id(draft.id, old))
                {
                    egui::TextEdit::store_state(ctx, inactive_draft_state_id(draft.id, new), state);
                }
            }
            if self
                .inactive_pane_drafts
                .contains_key(&(key.0, new.clone()))
            {
                if self.retain_hidden_draft(old.clone(), draft, false) {
                    if let Some(closed) = self.split_panes.closed.back_mut() {
                        closed.pane.channel = new.clone();
                    }
                }
            } else {
                self.inactive_pane_drafts
                    .insert((key.0, new.clone()), draft);
            }
        }
        if let Some(ctx) = &self.egui_ctx {
            crate::widgets::chat_input::rename_composer_event_channel(ctx, old, new);
        }
    }

    fn reidentify_hidden_draft(
        &self,
        channel: &ChannelId,
        draft: &mut ClassicDraft,
        was_classic: bool,
    ) {
        let id = SplitPanes::next_id();
        if let Some(ctx) = &self.egui_ctx {
            crate::widgets::chat_input::migrate_composer_events(
                ctx,
                chat_composer_id((!was_classic).then_some(draft.id)),
                chat_composer_id(Some(id)),
                channel,
            );
            if let Some(state) =
                egui::TextEdit::load_state(ctx, inactive_draft_state_id(draft.id, channel))
            {
                egui::TextEdit::store_state(ctx, inactive_draft_state_id(id, channel), state);
            }
        }
        draft.id = id;
        draft.view_scope = Some(id);
    }

    // A redirect can merge two channels previously edited in the same pane.
    // Retire that ambiguous transport identity; late results stay recoverable
    // instead of being attached to either draft by guesswork.
    fn rekey_live_draft_owner(&mut self, old: u64) {
        let id = SplitPanes::next_id();
        let mut channels = HashSet::new();
        for pane in self.split_panes.panes.iter_mut().chain(
            self.split_panes
                .closed
                .iter_mut()
                .map(|closed| &mut closed.pane),
        ) {
            if pane.id == old {
                pane.id = id;
                pane.search.set_pane(Some(id));
                channels.insert(pane.channel.clone());
            }
        }
        if let Some(layout) = &mut self.split_panes.layout {
            layout.swap(old, id);
        }
        if let Some((pane_id, _)) = self.single_pane_identity.as_mut() {
            if *pane_id == old {
                *pane_id = id;
                if let Some(channel) = &self.classic_draft_channel {
                    channels.insert(channel.clone());
                }
            }
        }
        for (channel, draft) in &mut self.inactive_classic_drafts {
            if draft.id == old {
                draft.id = id;
                channels.insert(channel.clone());
            }
        }
        let keys: Vec<_> = self
            .inactive_pane_drafts
            .keys()
            .filter(|(pane_id, _)| *pane_id == old)
            .cloned()
            .collect();
        for key in keys {
            let mut draft = self.inactive_pane_drafts.remove(&key).unwrap();
            draft.id = id;
            channels.insert(key.1.clone());
            self.inactive_pane_drafts.insert((id, key.1), draft);
        }
        if let Some(ctx) = &self.egui_ctx {
            for channel in channels {
                crate::widgets::chat_input::migrate_composer_events(
                    ctx,
                    chat_composer_id(Some(old)),
                    chat_composer_id(Some(id)),
                    &channel,
                );
                if let Some(state) =
                    egui::TextEdit::load_state(ctx, inactive_draft_state_id(old, &channel))
                {
                    egui::TextEdit::store_state(ctx, inactive_draft_state_id(id, &channel), state);
                }
            }
            if let Some(state) = egui::TextEdit::load_state(ctx, chat_composer_id(Some(old))) {
                egui::TextEdit::store_state(ctx, chat_composer_id(Some(id)), state);
            }
            if ctx.memory(|memory| memory.has_focus(chat_composer_id(Some(old)))) {
                request_composer_focus(ctx, Some(chat_composer_id(Some(id))));
            }
        }
    }
}

fn inactive_draft_state_id(id: u64, channel: &ChannelId) -> egui::Id {
    egui::Id::new("inactive_chat_draft")
        .with(id)
        .with(channel.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (CrustApp, Context, mpsc::Receiver<AppCommand>) {
        let (sender, receiver) = mpsc::channel(64);
        let (_, events) = mpsc::channel(64);
        let mut app = CrustApp::with_event_channels(sender, events);
        let ctx = Context::default();
        app.egui_ctx = Some(ctx.clone());
        for name in ["first", "second", "third"] {
            app.state.join_channel(ChannelId::new(name));
        }
        app.activate_channel(ChannelId::new("first"));
        (app, ctx, receiver)
    }

    fn reply(channel: &ChannelId, body: &str) -> PendingReply {
        PendingReply {
            channel: channel.clone(),
            info: ReplyInfo {
                parent_msg_id: body.to_owned(),
                parent_msg_body: body.to_owned(),
                ..Default::default()
            },
        }
    }

    fn select(ctx: &Context, id: egui::Id, start: usize, end: usize) {
        let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(start),
                egui::text::CCursor::new(end),
            )));
        egui::TextEdit::store_state(ctx, id, state);
    }

    fn selection(ctx: &Context, id: egui::Id) -> (usize, usize) {
        let range = egui::TextEdit::load_state(ctx, id)
            .unwrap()
            .cursor
            .char_range()
            .unwrap();
        (range.secondary.index, range.primary.index)
    }

    #[test]
    fn classic_channels_keep_separate_drafts_replies_cursors_and_targets() {
        let (mut app, ctx, _) = fixture();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let target = app.chat_composer_target(None);
        let identity = app.single_pane_identity;
        app.sync_classic_draft_owner();
        assert_eq!(
            app.single_pane_identity, identity,
            "an empty submitted draft retains its target"
        );
        app.chat_input_buf = "first draft 🙂".into();
        app.pending_reply = Some(reply(&first, "first parent"));
        select(&ctx, chat_composer_id(None), 1, 5);
        app.activate_channel(second.clone());
        assert!(app.chat_input_buf.is_empty());
        assert!(app.pending_reply.is_none());
        assert_ne!(app.single_pane_identity, identity);
        app.chat_input_buf = "second draft".into();
        app.pending_reply = Some(reply(&second, "second parent"));
        select(&ctx, chat_composer_id(None), 3, 6);
        app.activate_channel(first.clone());
        assert_eq!(app.chat_input_buf, "first draft 🙂");
        assert_eq!(app.single_pane_identity, identity);
        assert_eq!(app.chat_composer_target(None), target);
        assert_eq!(selection(&ctx, chat_composer_id(None)), (1, 5));
        assert_eq!(
            app.pending_reply.as_ref().unwrap().info.parent_msg_id,
            "first parent"
        );
        app.activate_channel(second);
        assert_eq!(app.chat_input_buf, "second draft");
        assert_eq!(selection(&ctx, chat_composer_id(None)), (3, 6));
        assert_eq!(
            app.pending_reply.as_ref().unwrap().info.parent_msg_id,
            "second parent"
        );
    }

    #[test]
    fn delayed_uploads_follow_inactive_classic_and_split_drafts() {
        let (mut app, ctx, _) = fixture();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let target = app.chat_composer_target(None);
        app.activate_channel(second.clone());
        app.chat_input_buf = "other draft".into();
        assert!(app.append_upload_to_composer(
            &first,
            Some(target),
            "https://example.test/first.png"
        ));
        assert_eq!(app.chat_input_buf, "other draft");
        app.activate_channel(first.clone());
        assert_eq!(app.chat_input_buf, "https://example.test/first.png ");
        app.seed_active_split(&ctx);
        app.split_panes
            .split(0, ChannelId::new("third"), SplitAxis::Horizontal);
        app.split_panes.focused = 0;
        app.sync_split_focus(&ctx);
        let target = app.chat_composer_target(Some(0));
        app.activate_channel(second);
        app.split_panes.panes[0].input_buf = "split draft".into();
        assert!(app.append_upload_to_composer(
            &first,
            Some(target),
            "https://example.test/second.png"
        ));
        assert_eq!(app.split_panes.panes[0].input_buf, "split draft");
        app.activate_channel(first);
        assert_eq!(
            app.split_panes.panes[0].input_buf,
            "https://example.test/first.png https://example.test/second.png "
        );
    }

    #[test]
    fn hidden_split_draft_survives_collapse_and_classic_channel_switches() {
        let (mut app, ctx, _) = fixture();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        app.chat_input_buf = "original draft".into();
        app.pending_reply = Some(reply(&first, "original parent"));
        select(&ctx, chat_composer_id(None), 2, 5);
        let identity = app.single_pane_identity;
        app.seed_active_split(&ctx);
        app.split_panes
            .split(0, ChannelId::new("third"), SplitAxis::Vertical);
        app.split_panes.focused = 0;
        app.sync_split_focus(&ctx);
        app.activate_channel(second.clone());
        app.split_panes.panes[0].input_buf = "replacement draft".into();
        app.keep_split_pane(0, &ctx);
        app.activate_channel(first);
        assert_eq!(app.chat_input_buf, "original draft");
        assert_eq!(app.single_pane_identity, identity);
        assert_eq!(selection(&ctx, chat_composer_id(None)), (2, 5));
        assert_eq!(
            app.pending_reply.as_ref().unwrap().info.parent_msg_id,
            "original parent"
        );
        app.activate_channel(second);
        assert_eq!(app.chat_input_buf, "replacement draft");
        assert!(app.pending_reply.is_none());
    }

    #[test]
    fn closing_and_reopening_an_inactive_classic_draft_restores_its_reply() {
        let (mut app, ctx, _) = fixture();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let id = app.single_pane_identity.unwrap().0;
        app.chat_input_buf = "closed draft".into();
        app.pending_reply = Some(reply(&first, "closed parent"));
        select(&ctx, chat_composer_id(None), 0, 4);
        app.activate_channel(second.clone());
        app.chat_input_buf = "visible draft".into();
        app.close_channel_tab(first.clone(), &ctx);
        app.reopen_closed_split(&ctx);
        let pane = app
            .split_panes
            .panes
            .iter()
            .find(|pane| pane.id == id)
            .unwrap();
        assert_eq!(pane.channel, first);
        assert_eq!(pane.input_buf, "closed draft");
        assert_eq!(selection(&ctx, chat_composer_id(Some(id))), (0, 4));
        assert_eq!(
            app.pending_reply.as_ref().unwrap().info.parent_msg_id,
            "closed parent"
        );
        assert_eq!(
            app.split_panes
                .panes
                .iter()
                .find(|pane| pane.channel == second)
                .unwrap()
                .input_buf,
            "visible draft"
        );
    }

    #[test]
    fn reopening_a_hidden_channel_reuses_its_live_pane_without_duplicate_ids() {
        let (mut app, ctx, _) = fixture();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        app.chat_input_buf = "hidden draft".into();
        app.seed_active_split(&ctx);
        app.split_panes
            .split(0, ChannelId::new("third"), SplitAxis::Horizontal);
        app.split_panes.focused = 0;
        app.sync_split_focus(&ctx);
        let id = app.split_panes.panes[0].id;
        app.activate_channel(second.clone());
        app.split_panes.panes[0].input_buf = "current draft".into();
        app.close_channel_tab(first, &ctx);
        app.reopen_closed_split(&ctx);
        assert_eq!(app.split_panes.panes.len(), 2);
        assert_eq!(app.split_panes.panes[0].id, id);
        assert_eq!(app.split_panes.panes[0].input_buf, "hidden draft");
        assert_ne!(app.split_panes.panes[0].id, app.split_panes.panes[1].id);
        app.activate_channel(second);
        assert_eq!(app.split_panes.panes[0].input_buf, "current draft");
    }

    #[test]
    fn redirect_collision_keeps_both_drafts_and_retires_ambiguous_async_targets() {
        let (mut app, ctx, _) = fixture();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        app.seed_active_split(&ctx);
        app.split_panes
            .split(0, ChannelId::new("third"), SplitAxis::Horizontal);
        app.split_panes.focused = 0;
        app.sync_split_focus(&ctx);
        app.activate_channel(second.clone());
        app.split_panes.panes[0].input_buf = "destination draft".into();
        let old_target = app.chat_composer_target(Some(0));
        let old_id = app.split_panes.panes[0].id;
        app.activate_channel(first.clone());
        app.split_panes.panes[0].input_buf = "redirected draft".into();
        app.apply_event(
            AppEvent::ChannelRedirected {
                old_channel: first,
                new_channel: second.clone(),
            },
            &ctx,
        );
        assert_eq!(app.split_panes.panes[0].input_buf, "redirected draft");
        assert_ne!(app.split_panes.panes[0].id, old_id);
        let closed = app.split_panes.closed.back().unwrap();
        assert_eq!(closed.pane.input_buf, "destination draft");
        assert_eq!(closed.pane.channel, second);
        assert_ne!(closed.pane.id, old_id);
        assert_ne!(closed.pane.id, app.split_panes.panes[0].id);
        assert!(!app.append_upload_to_composer(
            &second,
            Some(old_target),
            "https://example.test/late.png"
        ));
        assert_eq!(app.split_panes.panes[0].input_buf, "redirected draft");
        assert!(app
            .split_panes
            .saved_layout()
            .unwrap()
            .validated(2)
            .is_some());
        app.reopen_closed_split(&ctx);
        assert_eq!(app.split_panes.panes.len(), 3);
        assert!(app
            .split_panes
            .panes
            .iter()
            .any(|pane| pane.input_buf == "destination draft"));
    }
}
