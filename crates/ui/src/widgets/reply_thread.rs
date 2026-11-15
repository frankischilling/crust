use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use crust_core::{ChannelId, ChatMessage, ComposerTarget, ReplyInfo};
use egui::{Context, Id, RichText};

use super::chat_input::{request_composer_focus, ChatInput, PendingUpload};
use super::emote_picker::{EmotePicker, EmotePickerPreferences};
use super::message_list::{MessagePreviewActions, MessagePreviewCache, MessagePreviewContext};
use crate::theme as t;

const MAX_THREAD_VIEWS: usize = 64;
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_PARENT_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ThreadKey {
    pub account_id: Option<String>,
    pub channel: ChannelId,
    pub root_id: String,
}

impl ThreadKey {
    pub fn composer_id(&self) -> Id {
        Id::new("reply_thread_composer").with(self)
    }
    fn window_id(&self) -> Id {
        Id::new("reply_thread_window").with(self)
    }
    pub fn target(&self) -> ComposerTarget {
        ComposerTarget::ReplyThread {
            account_id: self.account_id.clone(),
            root_msg_id: self.root_id.clone(),
        }
    }
    pub fn from_target(channel: &ChannelId, target: &ComposerTarget) -> Option<Self> {
        match target {
            ComposerTarget::ReplyThread {
                account_id,
                root_msg_id,
            } => Some(Self {
                account_id: account_id.clone(),
                channel: channel.clone(),
                root_id: root_msg_id.clone(),
            }),
            ComposerTarget::Chat { .. } => None,
        }
    }
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

/// Prefer server thread metadata, then walk retained parents for older history.
fn root_id<'a>(
    message: &'a ChatMessage,
    by_id: &HashMap<&str, &'a ChatMessage>,
) -> Option<&'a str> {
    let mut current = message;
    for _ in 0..MAX_PARENT_DEPTH {
        let Some(reply) = &current.reply else {
            return current.server_id.as_deref().and_then(nonempty);
        };
        if let Some(root) = reply.thread_parent_msg_id.as_deref().and_then(nonempty) {
            return Some(root);
        }
        let parent = nonempty(&reply.parent_msg_id)?;
        if current.server_id.as_deref().and_then(nonempty) == Some(parent) {
            return None;
        }
        let Some(found) = by_id.get(parent) else {
            return Some(parent);
        };
        current = found;
    }
    None
}

fn retained_index(messages: &VecDeque<ChatMessage>) -> HashMap<&str, &ChatMessage> {
    messages
        .iter()
        .filter_map(|message| {
            message
                .server_id
                .as_deref()
                .and_then(nonempty)
                .map(|id| (id, message))
        })
        .collect()
}

fn root_reply(root: &str, seed: Option<&ChatMessage>, source: &VecDeque<ChatMessage>) -> ReplyInfo {
    if let Some(message) = source
        .iter()
        .find(|message| message.server_id.as_deref() == Some(root))
        .or_else(|| seed.filter(|message| message.server_id.as_deref() == Some(root)))
    {
        return ReplyInfo {
            parent_msg_id: root.to_owned(),
            parent_user_login: message.sender.login.clone(),
            parent_display_name: message.sender.display_name.clone(),
            parent_msg_body: message.raw_text.clone(),
            thread_parent_msg_id: Some(root.to_owned()),
            thread_parent_user_login: Some(message.sender.login.clone()),
        };
    }
    let mut reply = seed
        .and_then(|message| message.reply.as_ref())
        .filter(|reply| reply.parent_msg_id == root)
        .cloned()
        .unwrap_or_else(|| ReplyInfo {
            parent_msg_id: root.to_owned(),
            parent_user_login: seed
                .and_then(|message| message.reply.as_ref())
                .and_then(|reply| reply.thread_parent_user_login.clone())
                .unwrap_or_default(),
            ..Default::default()
        });
    if reply.parent_display_name.is_empty() {
        reply
            .parent_display_name
            .clone_from(&reply.parent_user_login);
    }
    reply.thread_parent_msg_id = Some(root.to_owned());
    reply.thread_parent_user_login = nonempty(&reply.parent_user_login).map(str::to_owned);
    reply
}

struct ThreadView {
    key: ThreadKey,
    seed: Option<ChatMessage>,
    reply: ReplyInfo,
    draft: String,
    cache: MessagePreviewCache,
    source_fingerprint: Option<u64>,
    source_indices: Vec<usize>,
    picker: EmotePicker,
    focus_input: bool,
    focused_control: Option<Id>,
}

impl ThreadView {
    fn new(key: ThreadKey, seed: Option<ChatMessage>, reply: ReplyInfo) -> Self {
        Self {
            picker: EmotePicker::default().with_namespace(("reply_thread", &key)),
            key,
            seed,
            reply,
            draft: String::new(),
            cache: MessagePreviewCache::default(),
            source_fingerprint: None,
            source_indices: Vec::new(),
            focus_input: true,
            focused_control: None,
        }
    }

    fn refresh(&mut self, source: &VecDeque<ChatMessage>) {
        let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
        for message in source {
            (&message.id, &message.server_id).hash(&mut fingerprint);
            message
                .reply
                .as_ref()
                .map(|reply| (&reply.parent_msg_id, &reply.thread_parent_msg_id))
                .hash(&mut fingerprint);
        }
        let fingerprint = fingerprint.finish();
        if self.source_fingerprint != Some(fingerprint) {
            let by_id = retained_index(source);
            self.source_indices = source
                .iter()
                .enumerate()
                .filter_map(|(index, message)| {
                    (root_id(message, &by_id) == Some(self.key.root_id.as_str())).then_some(index)
                })
                .collect();
            self.source_fingerprint = Some(fingerprint);
            if let Some(root) = source.iter().find(|message| message.server_id.as_deref() == Some(self.key.root_id.as_str())) {
                self.seed = Some(root.clone());
            }
        }
        let reply = root_reply(&self.key.root_id, self.seed.as_ref(), source);
        let authoritative = source
            .iter()
            .chain(self.seed.iter())
            .any(|message| message.server_id.as_deref() == Some(self.key.root_id.as_str()));
        if authoritative || self.reply.parent_msg_body.is_empty() {
            self.reply = reply;
        }
    }

    fn rows<'a>(&'a self, source: &'a VecDeque<ChatMessage>) -> Vec<&'a ChatMessage> {
        let mut rows: Vec<_> = self
            .source_indices
            .iter()
            .filter_map(|index| source.get(*index))
            .collect();
        if let Some(seed) = self.seed.as_ref().filter(|seed| {
            !rows.iter().any(|row| {
                row.id == seed.id
                    || seed
                        .server_id
                        .as_ref()
                        .is_some_and(|id| row.server_id.as_ref() == Some(id))
            })
        }) {
            let position = rows.partition_point(|row| row.timestamp <= seed.timestamp);
            rows.insert(position, seed);
        }
        if let Some(position) = rows
            .iter()
            .position(|row| row.server_id.as_deref() == Some(self.key.root_id.as_str()))
        {
            let root = rows.remove(position);
            rows.insert(0, root);
        }
        rows
    }
}

pub(crate) struct ThreadSubmission {
    pub key: ThreadKey,
    pub draft: String,
    pub reply: ReplyInfo,
}

#[derive(Default)]
pub(crate) struct ReplyThreadOutput {
    pub actions: MessagePreviewActions,
    pub submission: Option<ThreadSubmission>,
    pub uploads: Vec<(ThreadKey, PendingUpload)>,
    pub add_to_dictionary: Option<String>,
    pub picker_preferences: Option<EmotePickerPreferences>,
}

#[derive(Default)]
pub(crate) struct ReplyThreads {
    views: VecDeque<ThreadView>,
    active: Option<ThreadKey>,
    subscriptions: VecDeque<(ThreadKey, bool)>,
}

impl ReplyThreads {
    pub fn active_key(&self, account_id: Option<&str>) -> Option<&ThreadKey> {
        self.active
            .as_ref()
            .filter(|key| key.account_id.as_deref() == account_id)
    }

    #[cfg(test)]
    pub(crate) fn draft(&self, key: &ThreadKey) -> Option<&str> {
        self.views
            .iter()
            .find(|view| &view.key == key)
            .map(|view| view.draft.as_str())
    }

    fn reserve_view(&mut self) -> bool {
        if self.views.len() < MAX_THREAD_VIEWS {
            return true;
        }
        if let Some(index) = self
            .views
            .iter()
            .position(|view| view.draft.is_empty() && self.active.as_ref() != Some(&view.key))
        {
            self.views.remove(index);
            true
        } else {
            false
        }
    }

    pub fn open(
        &mut self,
        account_id: Option<&str>,
        seed: ChatMessage,
        source: &VecDeque<ChatMessage>,
    ) -> bool {
        if !seed.channel.is_twitch() {
            return false;
        }
        let by_id = retained_index(source);
        let Some(root) = root_id(&seed, &by_id) else {
            return false;
        };
        let key = ThreadKey {
            account_id: account_id.map(str::to_owned),
            channel: seed.channel.clone(),
            root_id: root.to_owned(),
        };
        let seed = source.iter().find(|message| message.server_id.as_deref() == Some(key.root_id.as_str()))
            .cloned().unwrap_or(seed);
        if let Some(view) = self.views.iter_mut().find(|view| view.key == key) {
            view.focus_input = true;
            if seed.server_id.as_deref() == Some(key.root_id.as_str()) || view.seed.is_none() {
                view.seed = Some(seed);
            }
        } else {
            if !self.reserve_view() {
                return false;
            }
            let reply = root_reply(&key.root_id, Some(&seed), source);
            self.views
                .push_back(ThreadView::new(key.clone(), Some(seed), reply));
        }
        self.active = Some(key);
        true
    }

    pub fn subscribed(&self, key: &ThreadKey) -> bool {
        self.subscriptions
            .iter()
            .any(|(entry, subscribed)| entry == key && *subscribed)
    }

    pub fn set_subscribed(&mut self, key: ThreadKey, subscribed: bool) {
        self.subscriptions.retain(|(entry, _)| entry != &key);
        self.subscriptions.push_back((key, subscribed));
        while self.subscriptions.len() > MAX_SUBSCRIPTIONS {
            self.subscriptions.pop_front();
        }
    }

    pub fn observe_participation(
        &mut self,
        account_id: Option<&str>,
        viewer_login: Option<&str>,
        message: &ChatMessage,
        source: &VecDeque<ChatMessage>,
    ) {
        let (Some(account), Some(viewer), Some(reply)) =
            (account_id, viewer_login, message.reply.as_ref())
        else {
            return;
        };
        if message.flags.is_history
            || message.flags.is_deleted
            || message.flags.suppress_notification
        {
            return;
        }
        let participated = viewer.eq_ignore_ascii_case(&message.sender.login)
            || viewer.eq_ignore_ascii_case(&reply.parent_user_login)
            || reply
                .thread_parent_user_login
                .as_deref()
                .is_some_and(|login| viewer.eq_ignore_ascii_case(login));
        if !participated {
            return;
        }
        let root = reply
            .thread_parent_msg_id
            .as_deref()
            .and_then(nonempty)
            .map(str::to_owned)
            .or_else(|| root_id(message, &retained_index(source)).map(str::to_owned));
        let Some(root_id) = root else {
            return;
        };
        let key = ThreadKey {
            account_id: Some(account.to_owned()),
            channel: message.channel.clone(),
            root_id,
        };
        if !self.subscriptions.iter().any(|(entry, _)| entry == &key) {
            self.set_subscribed(key, true);
        }
    }

    pub fn should_notify(
        &self,
        account_id: Option<&str>,
        viewer_login: Option<&str>,
        message: &ChatMessage,
        source: &VecDeque<ChatMessage>,
    ) -> bool {
        if message.flags.is_history
            || message.flags.is_self
            || message.flags.is_deleted
            || message.flags.suppress_notification
            || message.reply.is_none()
            || viewer_login.is_some_and(|login| login.eq_ignore_ascii_case(&message.sender.login))
        {
            return false;
        }
        let relevant = |(key, subscribed): &(ThreadKey, bool)| {
            *subscribed && key.account_id.as_deref() == account_id && key.channel == message.channel
        };
        if !self.subscriptions.iter().any(relevant) {
            return false;
        }
        let mut current = message;
        for _ in 0..MAX_PARENT_DEPTH {
            let Some(reply) = current.reply.as_ref() else {
                return false;
            };
            let candidate = reply
                .thread_parent_msg_id
                .as_deref()
                .and_then(nonempty)
                .or_else(|| nonempty(&reply.parent_msg_id));
            let Some(candidate) = candidate else {
                return false;
            };
            if self
                .subscriptions
                .iter()
                .any(|entry| relevant(entry) && entry.0.root_id == candidate)
            {
                return true;
            }
            if reply
                .thread_parent_msg_id
                .as_deref()
                .and_then(nonempty)
                .is_some()
            {
                return false;
            }
            let Some(parent) = source
                .iter()
                .rev()
                .find(|parent| parent.server_id.as_deref() == Some(candidate))
            else {
                return false;
            };
            current = parent;
        }
        false
    }

    pub fn can_restore(&self, key: &ThreadKey, activate: bool) -> bool {
        if !activate && self.active.as_ref() != Some(key) {
            return false;
        }
        self.views
            .iter()
            .find(|view| &view.key == key)
            .map_or(activate, |view| view.draft.is_empty())
    }

    pub fn restore_draft(
        &mut self,
        key: ThreadKey,
        text: &str,
        reply: Option<&ReplyInfo>,
        activate: bool,
    ) -> bool {
        if !self.can_restore(&key, activate) {
            return false;
        }
        if !self.views.iter().any(|view| view.key == key) {
            if !self.reserve_view() {
                return false;
            }
            let reply = reply.cloned().unwrap_or_else(|| ReplyInfo {
                parent_msg_id: key.root_id.clone(),
                thread_parent_msg_id: Some(key.root_id.clone()),
                ..Default::default()
            });
            self.views
                .push_back(ThreadView::new(key.clone(), None, reply));
        }
        let view = self.views.iter_mut().find(|view| view.key == key).unwrap();
        view.draft = text.to_owned();
        if activate {
            view.focus_input = true;
            self.active = Some(key);
        }
        true
    }

    pub fn append_upload(&mut self, key: ThreadKey, url: &str) -> bool {
        if !self.views.iter().any(|view| view.key == key) {
            if !self.reserve_view() {
                return false;
            }
            let reply = ReplyInfo {
                parent_msg_id: key.root_id.clone(),
                thread_parent_msg_id: Some(key.root_id.clone()),
                ..Default::default()
            };
            let mut view = ThreadView::new(key.clone(), None, reply);
            view.focus_input = false;
            self.views.push_back(view);
        }
        let view = self.views.iter_mut().find(|view| view.key == key).unwrap();
        if !view.draft.is_empty() && !view.draft.ends_with(' ') {
            view.draft.push(' ');
        }
        view.draft.push_str(url);
        view.draft.push(' ');
        true
    }

    pub fn owns_keyboard(&self, ctx: &Context, account_id: Option<&str>) -> bool {
        let Some(key) = self.active_key(account_id) else {
            return false;
        };
        let Some(view) = self.views.iter().find(|view| &view.key == key) else {
            return false;
        };
        let focused = ctx.memory(|memory| {
            memory.focused().or_else(|| {
                view.focused_control
                    .filter(|id| memory.had_focus_last_frame(*id))
            })
        });
        view.picker.owns_keyboard(ctx)
            || super::chat_input::composer_owns_keyboard(ctx, key.composer_id())
            || (focused.is_some() && focused == view.focused_control)
    }

    pub fn handle_shortcuts(&mut self, ctx: &Context, account_id: Option<&str>) -> bool {
        let owns = self.owns_keyboard(ctx, account_id);
        let Some(key) = self.active_key(account_id).cloned() else {
            return false;
        };
        let Some(view) = self.views.iter_mut().find(|view| view.key == key) else {
            return false;
        };
        let picker_was_open = view.picker.open;
        view.picker.handle_shortcuts(ctx);
        if picker_was_open && !view.picker.open {
            request_composer_focus(ctx, Some(key.composer_id()));
            return true;
        }
        owns
    }

    pub fn invalidate_catalogs(&mut self) {
        for view in &mut self.views {
            view.picker.invalidate_catalog();
        }
    }

    pub fn apply_moderation(&mut self, action: &crust_core::model::ModerationAction) {
        for view in &mut self.views {
            if let Some(seed) = view.seed.as_mut().filter(|seed| action.matches(seed)) {
                seed.flags.is_deleted = true;
            }
        }
    }

    pub fn update_seed(&mut self, message: &ChatMessage) {
        for view in &mut self.views {
            if let Some(seed) = view.seed.as_mut().filter(|seed| {
                seed.channel == message.channel
                    && (seed.id == message.id
                        || message
                            .server_id
                            .as_ref()
                            .is_some_and(|id| seed.server_id.as_ref() == Some(id)))
            }) {
                *seed = message.clone();
            }
        }
    }

    pub fn show(
        &mut self,
        ctx: &Context,
        account_id: Option<&str>,
        source: &VecDeque<ChatMessage>,
        preview: MessagePreviewContext<'_>,
        mut input: ChatInput<'_>,
        preferences: &EmotePickerPreferences,
    ) -> ReplyThreadOutput {
        let mut output = ReplyThreadOutput::default();
        let Some(key) = self.active_key(account_id).cloned() else {
            return output;
        };
        let owns_keyboard = self.owns_keyboard(ctx, account_id);
        let mut subscribed = self.subscribed(&key);
        let old_subscription = subscribed;
        let Some(view) = self.views.iter_mut().find(|view| view.key == key) else {
            return output;
        };
        view.refresh(source);
        input.id = Some(key.composer_id());
        input.pending_reply = None;
        input.emote_picker_open = view.picker.open;
        let mut open = true;
        let screen = ctx.screen_rect();
        let window_size = egui::vec2(
            (screen.width() - 48.0).max(100.0),
            (screen.height() - 64.0).max(80.0),
        );
        egui::Window::new(format!("Thread in #{}", key.channel.display_name()))
            .id(key.window_id())
            .open(&mut open)
            .default_size(egui::vec2(520.0, 480.0).min(window_size))
            .min_size(egui::vec2(100.0, 60.0))
            .max_size(window_size)
            .constrain_to(screen)
            .resizable(true)
            .collapsible(false)
            .frame(egui::Frame::window(&ctx.style()).fill(t::bg_dialog()))
            .show(ctx, |ui| {
                egui::TopBottomPanel::bottom(key.window_id().with("composer"))
                    .frame(egui::Frame::NONE)
                    .min_height(0.0)
                    .resizable(false)
                    .show_inside(ui, |ui| {
                        ui.separator();
                        if view.focus_input && ui.is_visible() && !ui.is_sizing_pass() {
                            view.focus_input = false;
                            request_composer_focus(ctx, Some(key.composer_id()));
                        }
                        input.accept_dropped_files = ctx
                            .pointer_latest_pos()
                            .is_some_and(|pos| ctx.layer_id_at(pos) == Some(ui.layer_id()));
                        let result = input.show(ui, &mut view.draft);
                        if let Some(draft) = result.send {
                            output.submission = Some(ThreadSubmission {
                                key: key.clone(),
                                draft,
                                reply: view.reply.clone(),
                            });
                        }
                        output.uploads.extend(
                            result
                                .uploads
                                .into_iter()
                                .map(|upload| (key.clone(), upload)),
                        );
                        output.add_to_dictionary = result.add_to_dictionary;
                        if result.toggle_emote_picker {
                            view.picker.toggle();
                        }
                    });
                ui.checkbox(&mut subscribed, "Subscribe to thread")
                    .on_hover_text("Highlight new replies for this account while Crust is open.");
                let mut cache = std::mem::take(&mut view.cache);
                let rows = view.rows(source);
                let root_present = rows
                    .iter()
                    .any(|row| row.server_id.as_deref() == Some(key.root_id.as_str()));
                if !root_present {
                    ui.add(
                        egui::Label::new(
                            RichText::new("Original message not loaded")
                                .font(t::small())
                                .color(t::text_muted()),
                        )
                        .truncate(),
                    )
                    .on_hover_text(format!(
                        "{}: {}",
                        view.reply.parent_display_name, view.reply.parent_msg_body
                    ));
                }
                let height = ui.available_height().max(0.0);
                output.actions = cache.show_rows(
                    ui,
                    key.window_id().with("messages"),
                    rows,
                    &preview,
                    false,
                    height,
                    true,
                );
                view.cache = cache;
                view.focused_control = ctx.memory(|memory| memory.focused()).filter(|id| {
                    ctx.read_response(*id).is_some_and(|response| {
                        response.layer_id == ui.layer_id()
                            && ui.min_rect().contains_rect(response.rect)
                    })
                });
            });
        view.picker.apply_preferences(preferences);
        let picker_was_open = view.picker.open;
        if let Some(code) = view.picker.show(
            ctx,
            input.emote_catalog,
            input.emote_bytes,
            preview.cmd_tx,
            input.animate_emotes,
        ) {
            super::chat_input::insert_picker_emote(ctx, key.composer_id(), &mut view.draft, &code);
            request_composer_focus(ctx, Some(key.composer_id()));
        } else if picker_was_open && !view.picker.open {
            request_composer_focus(ctx, Some(key.composer_id()));
        }
        let updated = view.picker.preferences();
        if &updated != preferences {
            output.picker_preferences = Some(updated);
        }
        if owns_keyboard
            && !ctx.is_context_menu_open()
            && !view.picker.open
            && ctx.input_mut(|input| {
                crate::app::consume_exact_key(input, egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            open = false;
        }
        if !open {
            self.active = None;
        }
        if subscribed != old_subscription {
            self.set_subscribed(key, subscribed);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::model::{MessageFlags, MessageId, MsgKind, Sender, UserId};

    fn message(id: u64, parent: Option<&str>, root: Option<&str>) -> ChatMessage {
        ChatMessage {
            id: MessageId(id),
            server_id: Some(id.to_string()),
            channel: ChannelId::new("room"),
            timestamp: chrono::Utc::now(),
            raw_text: format!("message {id}"),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            msg_kind: MsgKind::Chat,
            shared: None,
            sender: Sender {
                user_id: UserId("sender-id".into()),
                login: "sender".into(),
                display_name: "Sender".into(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            reply: parent.map(|parent| ReplyInfo {
                parent_msg_id: parent.into(),
                parent_user_login: "parent".into(),
                parent_display_name: "Parent".into(),
                parent_msg_body: "quoted parent".into(),
                thread_parent_msg_id: root.map(str::to_owned),
                thread_parent_user_login: root.map(|_| "author".into()),
            }),
        }
    }

    #[test]
    fn nested_replies_share_a_root_and_missing_roots_keep_server_metadata() {
        let source = VecDeque::from([
            message(1, None, None),
            message(2, Some("1"), None),
            message(3, Some("2"), None),
        ]);
        assert_eq!(root_id(&source[2], &retained_index(&source)), Some("1"));
        let nested = message(4, Some("missing-parent"), Some("missing-root"));
        assert_eq!(
            root_id(&nested, &retained_index(&source)),
            Some("missing-root")
        );
        let reply = root_reply("missing-root", Some(&nested), &source);
        assert_eq!(reply.parent_msg_id, "missing-root");
        assert_eq!(reply.parent_user_login, "author");
        assert!(
            reply.parent_msg_body.is_empty(),
            "an intermediate parent's text is not the root text"
        );
        let cycles = VecDeque::from([message(1, Some("2"), None), message(2, Some("1"), None)]);
        assert_eq!(root_id(&cycles[0], &retained_index(&cycles)), None);
    }

    #[test]
    fn subscriptions_are_bounded_account_scoped_and_skip_history_and_self() {
        let mut threads = ReplyThreads::default();
        let key = ThreadKey {
            account_id: Some("alice-id".into()),
            channel: ChannelId::new("room"),
            root_id: "1".into(),
        };
        threads.set_subscribed(key.clone(), true);
        let source = VecDeque::from([message(1, None, None)]);
        let mut reply = message(2, Some("1"), Some("1"));
        assert!(threads.should_notify(Some("alice-id"), Some("alice"), &reply, &source));
        assert!(!threads.should_notify(Some("bob-id"), Some("bob"), &reply, &source));
        reply.flags.is_history = true;
        assert!(!threads.should_notify(Some("alice-id"), Some("alice"), &reply, &source));
        reply.flags.is_history = false;
        reply.flags.is_self = true;
        assert!(!threads.should_notify(Some("alice-id"), Some("alice"), &reply, &source));
        reply.flags.is_self = false;
        reply.sender.login = "ALICE".into();
        assert!(!threads.should_notify(Some("alice-id"), Some("alice"), &reply, &source));
        for index in 0..MAX_SUBSCRIPTIONS + 20 {
            threads.set_subscribed(
                ThreadKey {
                    root_id: index.to_string(),
                    ..key.clone()
                },
                true,
            );
        }
        assert_eq!(threads.subscriptions.len(), MAX_SUBSCRIPTIONS);
    }

    #[test]
    fn rejected_drafts_and_uploads_stay_with_their_thread() {
        let mut threads = ReplyThreads::default();
        let source = VecDeque::from([message(1, None, None), message(2, None, None)]);
        assert!(threads.open(Some("alice-id"), source[0].clone(), &source));
        let first = threads.active.clone().unwrap();
        assert!(threads.restore_draft(first.clone(), "keep first draft", None, false));
        assert!(!threads.restore_draft(first.clone(), "rejected replacement", None, false));
        assert!(threads.open(Some("alice-id"), source[1].clone(), &source));
        let second = threads.active.clone().unwrap();
        assert!(!threads.restore_draft(first.clone(), "rejected replacement", None, false));
        assert!(threads.append_upload(first.clone(), "https://example.test/image.png"));
        assert_eq!(threads.active.as_ref(), Some(&second));
        assert!(threads
            .views
            .iter()
            .find(|view| view.key == second)
            .unwrap()
            .draft
            .is_empty());
        assert_eq!(
            threads
                .views
                .iter()
                .find(|view| view.key == first)
                .unwrap()
                .draft,
            "keep first draft https://example.test/image.png "
        );
        assert!(threads.active_key(Some("bob-id")).is_none());
    }

    #[test]
    fn missing_roots_keep_restored_context_and_seed_rows_precede_replies() {
        let mut threads = ReplyThreads::default();
        let root = message(1, None, None);
        let source = VecDeque::from([message(2, Some("1"), Some("1"))]);
        assert!(threads.open(Some("alice-id"), root.clone(), &source));
        let view = threads.views.back_mut().unwrap();
        view.refresh(&source);
        assert_eq!(
            view.rows(&source)
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [MessageId(1), MessageId(2)]
        );
        let mut with_root = source.clone();
        let mut duplicate = root.clone();
        duplicate.id = MessageId(99);
        with_root.push_front(duplicate);
        view.refresh(&with_root);
        assert_eq!(
            view.rows(&with_root).len(),
            2,
            "a server ID deduplicates a historical snapshot"
        );

        let key = ThreadKey {
            account_id: Some("alice-id".into()),
            channel: root.channel.clone(),
            root_id: "absent".into(),
        };
        let reply = ReplyInfo {
            parent_msg_id: "absent".into(),
            parent_user_login: "author".into(),
            parent_display_name: "Author".into(),
            parent_msg_body: "Keep this quoted root".into(),
            ..Default::default()
        };
        assert!(threads.restore_draft(key.clone(), "rejected", Some(&reply), true));
        let view = threads
            .views
            .iter_mut()
            .find(|view| view.key == key)
            .unwrap();
        view.refresh(&source);
        assert_eq!(view.reply.parent_msg_body, reply.parent_msg_body);
        assert_eq!(view.reply.parent_user_login, reply.parent_user_login);

        threads.apply_moderation(&crust_core::model::ModerationAction::message(
            root.channel.clone(),
            "1".into(),
        ));
        assert!(
            threads
                .views
                .front()
                .unwrap()
                .seed
                .as_ref()
                .unwrap()
                .flags
                .is_deleted
        );
    }

    #[test]
    fn late_upload_recreates_an_inactive_view_without_stealing_focus() {
        let mut threads = ReplyThreads::default();
        let source = VecDeque::from([message(1, None, None)]);
        assert!(threads.open(Some("alice-id"), source[0].clone(), &source));
        let active = threads.active.clone();
        let missing = ThreadKey {
            account_id: Some("old-account".into()),
            channel: ChannelId::new("other"),
            root_id: "old-root".into(),
        };
        assert!(threads.append_upload(missing.clone(), "https://example.test/upload.png"));
        assert_eq!(threads.active, active);
        let view = threads
            .views
            .iter()
            .find(|view| view.key == missing)
            .unwrap();
        assert!(!view.focus_input);
        assert_eq!(view.draft, "https://example.test/upload.png ");
    }

    #[test]
    fn participation_subscribes_live_threads_and_respects_explicit_unsubscribe() {
        let mut threads = ReplyThreads::default();
        let source = VecDeque::from([message(1, None, None)]);
        let mut own = message(2, Some("1"), Some("1"));
        own.sender.login = "alice".into();
        own.flags.is_self = true;
        own.flags.is_history = true;
        threads.observe_participation(Some("alice-id"), Some("alice"), &own, &source);
        assert!(threads.subscriptions.is_empty());
        own.flags.is_history = false;
        threads.observe_participation(Some("alice-id"), Some("alice"), &own, &source);
        let key = ThreadKey {
            account_id: Some("alice-id".into()),
            channel: own.channel.clone(),
            root_id: "1".into(),
        };
        assert!(threads.subscribed(&key));
        assert!(!threads.should_notify(Some("alice-id"), Some("alice"), &own, &source));
        threads.set_subscribed(key.clone(), false);
        threads.observe_participation(Some("alice-id"), Some("alice"), &own, &source);
        assert!(!threads.subscribed(&key));
        threads.observe_participation(Some("bob-id"), Some("bob"), &own, &source);
        assert!(!threads.subscribed(&ThreadKey {
            account_id: Some("bob-id".into()),
            ..key.clone()
        }));
        own.sender.login = "bob".into();
        threads.observe_participation(Some("bob-id"), Some("bob"), &own, &source);
        assert!(threads.subscribed(&ThreadKey {
            account_id: Some("bob-id".into()),
            ..key
        }));
    }

    #[test]
    fn opening_a_reply_retains_the_full_root_after_live_history_rotates() {
        let mut threads = ReplyThreads::default();
        let source = VecDeque::from([message(1, None, None), message(2, Some("1"), Some("1"))]);
        assert!(threads.open(Some("alice-id"), source[1].clone(), &source));
        let view = threads.views.front_mut().unwrap();
        view.refresh(&source);
        let rotated = VecDeque::from([source[1].clone()]);
        view.refresh(&rotated);
        assert_eq!(view.rows(&rotated).iter().map(|row| row.id).collect::<Vec<_>>(), [MessageId(1), MessageId(2)]);
        assert_eq!(view.reply.parent_msg_body, "message 1");
    }

    fn render_frame(
        ctx: &Context,
        threads: &mut ReplyThreads,
        source: &VecDeque<ChatMessage>,
        preview: &super::super::message_list::PreviewTestState,
        size: egui::Vec2,
        events: Vec<egui::Event>,
        main_draft: &mut String,
    ) -> egui::FullOutput {
        let bindings = crust_core::hotkeys::HotkeyBindings::default();
        let channel = ChannelId::new("room");
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                ..Default::default()
            },
            |ctx| {
                let owner = threads
                    .active_key(Some("alice-id"))
                    .filter(|key| {
                        super::super::chat_input::composer_owns_keyboard(ctx, key.composer_id())
                    })
                    .map(|key| (key.composer_id(), &channel));
                super::super::chat_input::prepare_composer_events(ctx, &bindings, owner, Some("alice-id"));
                threads.handle_shortcuts(ctx, Some("alice-id"));
                threads.show(
                    ctx,
                    Some("alice-id"),
                    source,
                    preview.context(),
                    ChatInput {
                        id: None,
                        channel: &channel,
                        logged_in: true,
                        username: Some("alice"),
                        hotkey_bindings: &bindings,
                        emote_catalog: &[],
                        emote_bytes: &preview.images,
                        emote_picker_open: false,
                        pending_reply: None,
                        message_history: &[],
                        slash_usage_counts: &HashMap::new(),
                        known_channels: &[],
                        chatters: &[],
                        prevent_overlong_twitch_messages: true,
                        animate_emotes: false,
                        accept_dropped_files: false,
                    },
                    &EmotePickerPreferences::default(),
                );
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(main_draft).id(Id::new("other_composer")));
                });
            },
        )
    }

    #[test]
    fn compact_thread_keeps_its_composer_visible_with_a_long_missing_root_quote() {
        let ctx = Context::default();
        let preview = super::super::message_list::PreviewTestState::default();
        let source = VecDeque::new();
        let mut threads = ReplyThreads::default();
        let mut seed = message(2, Some("1"), Some("1"));
        seed.reply.as_mut().unwrap().parent_msg_body = "Long quoted message 🙂\n".repeat(100);
        threads.open(Some("alice-id"), seed, &source);
        let key = threads.active.clone().unwrap();
        let size = egui::vec2(220.0, 200.0);
        let mut main = String::new();
        for _ in 0..4 {
            render_frame(
                &ctx,
                &mut threads,
                &source,
                &preview,
                size,
                Vec::new(),
                &mut main,
            );
        }
        let composer = ctx
            .read_response(key.composer_id())
            .expect("thread composer is rendered");
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(
            screen.expand(1.0).contains_rect(composer.rect),
            "composer {:?} is outside {screen:?}",
            composer.rect
        );
        assert!(composer.rect.height() > 10.0);
    }

    #[test]
    fn unfocused_thread_picker_keeps_main_shortcuts_and_escape_with_main_chat() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let preview = super::super::message_list::PreviewTestState::default();
        let source = VecDeque::from([message(1, None, None)]);
        let mut threads = ReplyThreads::default();
        threads.open(Some("alice-id"), source[0].clone(), &source);
        let mut main = String::new();
        let size = egui::vec2(1100.0, 800.0);
        for _ in 0..3 {
            render_frame(
                &ctx,
                &mut threads,
                &source,
                &preview,
                size,
                Vec::new(),
                &mut main,
            );
        }
        threads.views.front_mut().unwrap().picker.toggle();
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = render_frame(
                &ctx,
                &mut threads,
                &source,
                &preview,
                size,
                Vec::new(),
                &mut main,
            );
        }
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        let picker = nodes.iter().find(|(_, node)| node.label() == Some("Emote picker"))
            .expect("the thread composer exposes its picker button");
        assert_eq!(picker.1.toggled(), Some(egui::accesskit::Toggled::True));
        assert!(threads.owns_keyboard(&ctx, Some("alice-id")));
        ctx.memory_mut(|memory| memory.request_focus(Id::new("other_composer")));
        render_frame(
            &ctx,
            &mut threads,
            &source,
            &preview,
            size,
            Vec::new(),
            &mut main,
        );
        assert!(!threads.owns_keyboard(&ctx, Some("alice-id")));
        render_frame(
            &ctx,
            &mut threads,
            &source,
            &preview,
            size,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut main,
        );
        assert!(threads.active.is_some());
        assert!(threads.views.front().unwrap().picker.open);
    }
}
