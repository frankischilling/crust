use super::*;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

/// Shared chat-row rendering for search results and local user history.
#[derive(Clone, Copy)]
pub struct MessagePreviewContext<'a> {
    pub emote_bytes: &'a UiImageCache,
    pub cmd_tx: &'a UiCommandSender,
    pub link_previews: &'a HashMap<String, LinkPreview>,
    pub source_channels: Option<&'a HashMap<ChannelId, crust_core::ChannelState>>,
    pub viewer_login: Option<&'a str>,
    pub show_timestamps: bool,
    pub show_timestamp_seconds: bool,
    pub use_24h_timestamps: bool,
    pub animate_emotes: bool,
    pub hide_link_previews: bool,
    pub highlight_rules: &'a [crust_core::highlight::HighlightMatch],
    pub mod_action_presets: &'a [crust_core::model::mod_actions::ModActionPreset],
}

#[derive(Clone, Debug, Default)]
pub struct MessagePreviewActions {
    pub reply: Option<(ChannelId, ReplyInfo)>,
    pub open_thread: Option<ChatMessage>,
    pub jump_to: Option<(ChannelId, MessageId)>,
    pub profile_request: Option<(ChannelId, String, Vec<Badge>)>,
}

impl MessagePreviewActions {
    pub fn append(&mut self, other: Self) {
        if other.open_thread.is_some() {
            self.open_thread = other.open_thread;
        }
        if other.reply.is_some() {
            self.reply = other.reply;
        }
        if other.jump_to.is_some() {
            self.jump_to = other.jump_to;
        }
        if other.profile_request.is_some() {
            self.profile_request = other.profile_request;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reply.is_none()
            && self.open_thread.is_none()
            && self.jump_to.is_none()
            && self.profile_request.is_none()
    }
}

pub(crate) struct MessagePreviewResult {
    pub response: egui::Response,
    pub actions: MessagePreviewActions,
}

struct EmptyPreviewState {
    messages: VecDeque<ChatMessage>,
    ignored_logins: HashSet<String>,
    ignored_users: crust_core::ignores::CompiledIgnoredUsers,
    low_trust_users: HashMap<String, crust_core::model::LowTrustEntry>,
}

impl EmptyPreviewState {
    fn get() -> &'static Self {
        static EMPTY: OnceLock<EmptyPreviewState> = OnceLock::new();
        EMPTY.get_or_init(|| Self {
            messages: VecDeque::new(),
            ignored_logins: HashSet::new(),
            ignored_users: crust_core::ignores::CompiledIgnoredUsers::new(&[]),
            low_trust_users: HashMap::new(),
        })
    }
}

impl<'a> MessagePreviewContext<'a> {
    pub fn new(
        emote_bytes: &'a UiImageCache,
        cmd_tx: &'a UiCommandSender,
        link_previews: &'a HashMap<String, LinkPreview>,
    ) -> Self {
        Self {
            emote_bytes,
            cmd_tx,
            link_previews,
            source_channels: None,
            viewer_login: None,
            show_timestamps: true,
            show_timestamp_seconds: false,
            use_24h_timestamps: true,
            animate_emotes: true,
            hide_link_previews: false,
            highlight_rules: &[],
            mod_action_presets: &[],
        }
    }

    fn can_moderate(&self, channel: &ChannelId) -> bool {
        self.source_channels
            .and_then(|channels| channels.get(channel))
            .is_some_and(|state| state.is_mod)
            || (channel.is_twitch()
                && self
                    .viewer_login
                    .is_some_and(|login| channel.display_name().eq_ignore_ascii_case(login)))
    }

    pub(crate) fn show(
        &self,
        ui: &mut Ui,
        message: &ChatMessage,
        id: Id,
        selected: bool,
        can_moderate: bool,
    ) -> MessagePreviewResult {
        let empty = EmptyPreviewState::get();
        let source = self
            .source_channels
            .and_then(|channels| channels.get(&message.channel));
        let mut list = MessageList::new(
            source.map_or(&empty.messages, |state| &state.messages),
            self.emote_bytes,
            self.cmd_tx,
            &message.channel,
            self.link_previews,
            None,
            false,
            3,
            self.animate_emotes,
            self.show_timestamps,
            self.show_timestamp_seconds,
            self.use_24h_timestamps,
            can_moderate || self.can_moderate(&message.channel),
            &empty.ignored_logins,
            &empty.ignored_users,
            self.highlight_rules,
            &[],
            self.mod_action_presets,
            source.map_or(&empty.low_trust_users, |state| &state.low_trust_users),
        )
        .with_hide_link_previews(self.hide_link_previews);
        list.preview_id = Some(id);
        let rctx = RenderCtx {
            reply_key: list.state_id("reply"),
            scroll_to_key: list.state_id("parent"),
            scroll_hint_key: list.state_id("parent_hint"),
            reply_senders: OnceCell::new(),
            highlight_server_id: None,
            highlight_alpha: 0.0,
            highlight_animating: false,
        };
        let rendered = message_row_scope(ui, id, true, |ui| {
            ui.set_min_width(ui.available_width());
            let mut colors = HashMap::new();
            list.render_message(ui, message, false, &rctx, &mut colors);
        });
        let response = rendered.response;
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                ui.is_enabled(),
                selected,
                format!("{}: {}", message.sender.login, message.plain_text()),
            )
        });
        if selected || response.has_focus() {
            ui.painter().rect_stroke(
                response.rect.shrink(0.5),
                t::RADIUS_SM,
                egui::Stroke::new(1.0_f32, t::border_accent()),
                egui::StrokeKind::Inside,
            );
        }
        response.context_menu(|ui| list.show_message_context_menu(ui, message, rctx.reply_key));
        let (clicked, target, hint) = ui.ctx().data_mut(|data| {
            (
                data.remove_temp::<bool>(list.state_id("preview_clicked"))
                    .unwrap_or(false),
                data.remove_temp::<String>(rctx.scroll_to_key),
                data.remove_temp::<ReplyScrollHint>(rctx.scroll_hint_key),
            )
        });
        let mut actions = MessagePreviewActions {
            reply: list
                .take_reply(ui, rctx.reply_key)
                .map(|reply| (message.channel.clone(), reply)),
            open_thread: list.take_thread_request(ui),
            profile_request: list
                .take_profile_request(ui)
                .map(|(login, badges)| (message.channel.clone(), login, badges)),
            ..Default::default()
        };
        if response.clicked() || clicked {
            response.request_focus();
            actions.jump_to = Some((message.channel.clone(), message.id));
        }
        if target.is_some() || hint.is_some() {
            if let Some(index) = list.find_reply_target_index(
                &VisibleIndices::All(list.messages.len()),
                target.as_deref(),
                hint.as_ref(),
            ) {
                actions.jump_to = Some((message.channel.clone(), list.messages[index].id));
            }
        }
        MessagePreviewResult { response, actions }
    }

    pub fn signature(&self, message: &ChatMessage) -> u64 {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        message.raw_text.hash(&mut hash);
        message.spans.hash(&mut hash);
        message.timestamp.hash(&mut hash);
        message.sender.login.hash(&mut hash);
        message.sender.display_name.hash(&mut hash);
        (
            message.flags.is_action,
            message.flags.is_deleted,
            message.flags.is_first_msg,
            message.flags.is_pinned,
            message.flags.is_highlighted,
            message.flags.is_mention,
            message.flags.is_history,
            message.flags.custom_reward_id.as_deref(),
        )
            .hash(&mut hash);
        for span in &message.spans {
            if let Span::Emote { url, .. } | Span::Emoji { url, .. } = span {
                self.image_dimensions(url).hash(&mut hash);
            }
        }
        let mut badge = |badge: &Badge| {
            (&badge.name, &badge.version, &badge.url).hash(&mut hash);
            badge
                .url
                .as_deref()
                .and_then(|url| self.image_dimensions(url))
                .hash(&mut hash);
        };
        for value in &message.sender.badges {
            badge(value);
        }
        if let Some(shared) = &message.shared {
            for value in &shared.badges {
                badge(value);
            }
            (
                &shared.room_id,
                &shared.login,
                &shared.display_name,
                &shared.profile_url,
            )
                .hash(&mut hash);
        }
        message
            .reply
            .as_ref()
            .map(|reply| {
                (
                    &reply.parent_msg_id,
                    &reply.parent_user_login,
                    &reply.parent_display_name,
                    &reply.parent_msg_body,
                )
            })
            .hash(&mut hash);
        std::mem::discriminant(&message.msg_kind).hash(&mut hash);
        match &message.msg_kind {
            MsgKind::Announcement { color } => color.hash(&mut hash),
            MsgKind::Sub {
                display_name,
                months,
                plan,
                is_gift,
                sub_msg,
            } => {
                (display_name, months, plan, is_gift, sub_msg).hash(&mut hash);
            }
            MsgKind::Raid {
                display_name,
                viewer_count,
                source_login,
            } => {
                (display_name, viewer_count, source_login).hash(&mut hash);
            }
            MsgKind::Timeout { login, seconds } => (login, seconds).hash(&mut hash),
            MsgKind::Ban { login } => login.hash(&mut hash),
            MsgKind::ChannelPointsReward {
                user_login,
                reward_title,
                cost,
                reward_id,
                redemption_id,
                user_input,
                status,
            } => {
                (
                    user_login,
                    reward_title,
                    cost,
                    reward_id,
                    redemption_id,
                    user_input,
                    status,
                )
                    .hash(&mut hash);
            }
            _ => {}
        }
        self.can_moderate(&message.channel).hash(&mut hash);
        hash.finish()
    }

    fn image_dimensions(&self, url: &str) -> Option<(u32, u32)> {
        self.emote_bytes
            .peek(url)
            .map(|(width, height, _)| (*width, *height))
    }
}

#[derive(PartialEq)]
struct PreviewLayout {
    width: f32,
    pixels_per_point: f32,
    body_font: egui::FontId,
    timestamp_font: egui::FontId,
    chip_font: egui::FontId,
    style: Arc<egui::Style>,
    timestamp_settings: (bool, bool, bool),
}

pub(crate) struct PreviewRowHeight {
    pub signature: u64,
    pub stride: f32,
    pub generation: u64,
}

#[derive(Default)]
pub(crate) struct MessagePreviewCache {
    layout: Option<PreviewLayout>,
    fonts: Option<egui::FontDefinitions>,
    highlight_rules: Vec<crust_core::highlight::HighlightRule>,
    pub rows: HashMap<Id, PreviewRowHeight>,
    pub generation: u64,
    pub rendered_rows: usize,
    pub measured_rows: usize,
    pub content_height: f32,
}

impl MessagePreviewCache {
    pub fn prepare(&mut self, ui: &Ui, context: &MessagePreviewContext<'_>) {
        let layout = PreviewLayout {
            width: ui.available_width(),
            pixels_per_point: ui.ctx().pixels_per_point(),
            body_font: ui
                .style()
                .override_font_id
                .clone()
                .unwrap_or_else(|| egui::TextStyle::Body.resolve(ui.style())),
            timestamp_font: t::timestamps_font(),
            chip_font: t::small(),
            style: ui.style().clone(),
            timestamp_settings: (
                context.show_timestamps,
                context.show_timestamp_seconds,
                context.use_24h_timestamps,
            ),
        };
        let fonts_changed = ui.ctx().fonts(|fonts| {
            let font_cache = fonts.lock();
            let definitions = font_cache.fonts.definitions();
            let unchanged = self.fonts.as_ref().is_some_and(|cached| {
                cached.families == definitions.families
                    && cached.font_data.len() == definitions.font_data.len()
                    && cached.font_data.iter().zip(&definitions.font_data).all(
                        |((left_name, left_font), (right_name, right_font))| {
                            left_name == right_name && Arc::ptr_eq(left_font, right_font)
                        },
                    )
            });
            if !unchanged {
                self.fonts = Some(definitions.clone());
            }
            !unchanged
        });
        let rules_changed = !self.highlight_rules.iter().eq(context
            .highlight_rules
            .iter()
            .map(|compiled| &compiled.rule));
        if rules_changed {
            self.highlight_rules = context
                .highlight_rules
                .iter()
                .map(|compiled| compiled.rule.clone())
                .collect();
        }
        if fonts_changed || rules_changed || self.layout.as_ref() != Some(&layout) {
            self.rows.clear();
            self.layout = Some(layout);
        }
        self.generation = self.generation.wrapping_add(1);
        self.rendered_rows = 0;
        self.measured_rows = 0;
    }

    pub fn stride(&mut self, id: Id, signature: u64) -> Option<f32> {
        self.rows
            .get_mut(&id)
            .filter(|row| row.signature == signature)
            .map(|row| {
                row.generation = self.generation;
                row.stride
            })
    }

    pub fn record(&mut self, id: Id, signature: u64, stride: f32, measured: bool) {
        self.rendered_rows += 1;
        self.measured_rows += usize::from(measured);
        self.rows.insert(
            id,
            PreviewRowHeight {
                signature,
                stride,
                generation: self.generation,
            },
        );
    }

    pub fn finish(&mut self, content_height: f32) {
        self.rows.retain(|_, row| row.generation == self.generation);
        self.content_height = content_height;
    }

    pub fn show_history(
        &mut self,
        ui: &mut Ui,
        namespace: Id,
        messages: &[ChatMessage],
        context: &MessagePreviewContext<'_>,
        can_moderate: bool,
    ) -> MessagePreviewActions {
        self.show_rows(
            ui,
            namespace,
            messages.iter(),
            context,
            can_moderate,
            320.0,
            false,
        )
    }

    pub fn show_rows<'a>(
        &mut self,
        ui: &mut Ui,
        namespace: Id,
        messages: impl IntoIterator<Item = &'a ChatMessage>,
        context: &MessagePreviewContext<'_>,
        can_moderate: bool,
        max_height: f32,
        stick_to_bottom: bool,
    ) -> MessagePreviewActions {
        let mut actions = MessagePreviewActions::default();
        let focused = ui.ctx().memory(|memory| memory.focused());
        let scroll = ScrollArea::vertical()
            .id_salt(namespace)
            .max_height(max_height)
            .min_scrolled_height(0.0)
            .stick_to_bottom(stick_to_bottom)
            .auto_shrink([false, true])
            .show_viewport(ui, |ui, viewport| {
                self.prepare(ui, context);
                let origin_y = ui.next_widget_position().y;
                let mut skipped = 0.0;
                for message in messages {
                    let id = namespace.with((&message.channel, message.id));
                    let signature = context.signature(message);
                    let stride = self.stride(id, signature);
                    let top = ui.next_widget_position().y - origin_y + skipped;
                    if focused != Some(id)
                        && stride.is_some_and(|height| {
                            top + height < viewport.min.y || top > viewport.max.y
                        })
                    {
                        skipped += stride.unwrap_or_default();
                        continue;
                    }
                    skip_rows(ui, skipped);
                    skipped = 0.0;
                    let top = ui.next_widget_position().y;
                    let result = context.show(ui, message, id, false, can_moderate);
                    actions.append(result.actions);
                    self.record(
                        id,
                        signature,
                        ui.next_widget_position().y - top,
                        stride.is_none(),
                    );
                }
                skip_rows(ui, skipped);
            });
        self.finish(scroll.content_size.y);
        actions
    }
}

fn skip_rows(ui: &mut Ui, stride: f32) {
    if stride > 0.0 {
        ui.allocate_space(egui::vec2(
            0.0,
            (stride - ui.spacing().item_spacing.y).max(0.0),
        ));
    }
}

#[cfg(test)]
pub(crate) struct PreviewTestState {
    pub images: UiImageCache,
    pub sender: UiCommandSender,
    pub receiver: tokio::sync::mpsc::Receiver<AppCommand>,
    previews: HashMap<String, LinkPreview>,
}

#[cfg(test)]
impl Default for PreviewTestState {
    fn default() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        Self {
            images: UiImageCache::default(),
            sender: UiCommandSender::new(sender),
            receiver,
            previews: HashMap::new(),
        }
    }
}

#[cfg(test)]
impl PreviewTestState {
    pub fn context(&self) -> MessagePreviewContext<'_> {
        MessagePreviewContext::new(&self.images, &self.sender, &self.previews)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u64, text: &str) -> ChatMessage {
        ChatMessage {
            id: MessageId(id),
            server_id: Some(format!("server-{id}")),
            timestamp: chrono::Utc::now(),
            channel: ChannelId::new("room"),
            sender: crust_core::model::Sender {
                user_id: crust_core::model::UserId("1".to_owned()),
                login: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: text.to_owned(),
            spans: smallvec::smallvec![Span::Text {
                text: text.to_owned(),
                is_action: false
            }],
            twitch_emotes: Vec::new(),
            flags: MessageFlags::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        }
    }

    fn emote(code: &str) -> Span {
        Span::Emote {
            id: code.to_owned(),
            code: code.to_owned(),
            url: format!("https://example.test/{code}.png"),
            url_hd: None,
            provider: "twitch".to_owned(),
            zero_width: false,
        }
    }

    fn image(state: &mut PreviewTestState, ctx: &egui::Context, code: &str, width: u32) {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, 20, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        assert!(state.images.complete(
            ctx,
            format!("https://example.test/{code}.png"),
            width,
            20,
            bytes.into_inner()
        ));
    }

    fn frame(
        ctx: &egui::Context,
        state: &PreviewTestState,
        messages: &[ChatMessage],
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, MessagePreviewActions) {
        frame_with_skipped_siblings(ctx, state, messages, events, 0)
    }

    fn frame_with_skipped_siblings(
        ctx: &egui::Context,
        state: &PreviewTestState,
        messages: &[ChatMessage],
        events: Vec<egui::Event>,
        skipped: usize,
    ) -> (egui::FullOutput, MessagePreviewActions) {
        let mut actions = MessagePreviewActions::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 500.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.skip_ahead_auto_ids(skipped);
                    for message in messages {
                        actions.append(
                            state
                                .context()
                                .show(
                                    ui,
                                    message,
                                    Id::new("preview_test").with(message.id),
                                    false,
                                    false,
                                )
                                .actions,
                        );
                    }
                });
            },
        );
        (output, actions)
    }

    fn text_center(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        text_bounds(output, label).center()
    }

    fn text_bounds(output: &egui::FullOutput, label: &str) -> egui::Rect {
        output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => Some(
                    text.galley
                        .rows
                        .iter()
                        .flat_map(|row| &row.glyphs)
                        .fold(egui::Rect::NOTHING, |bounds, glyph| {
                            bounds.union(glyph.logical_rect())
                        })
                        .translate(text.pos.to_vec2()),
                ),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing visible text {label:?}"))
    }

    fn click(
        ctx: &egui::Context,
        state: &PreviewTestState,
        messages: &[ChatMessage],
        position: egui::Pos2,
        button: egui::PointerButton,
    ) -> (egui::FullOutput, MessagePreviewActions) {
        let mut result = None;
        for pressed in [true, false] {
            result = Some(frame(
                ctx,
                state,
                messages,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ));
        }
        result.unwrap()
    }

    fn commands(state: &mut PreviewTestState) -> Vec<AppCommand> {
        std::iter::from_fn(|| state.receiver.try_recv().ok())
            .filter(|command| !matches!(command, AppCommand::FetchLinkPreview { .. }))
            .collect()
    }

    #[test]
    fn rich_rows_keep_emotes_badges_reply_and_subscription_content() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let mut state = PreviewTestState::default();
        image(&mut state, &ctx, "Smile", 40);
        image(&mut state, &ctx, "moderator", 20);
        let mut chat = message(1, "Smile");
        chat.spans = smallvec::smallvec![emote("Smile")];
        chat.sender.badges.push(Badge {
            name: "moderator".to_owned(),
            version: "1".to_owned(),
            url: Some("https://example.test/moderator.png".to_owned()),
        });
        chat.reply = Some(ReplyInfo {
            parent_msg_id: "parent".to_owned(),
            parent_user_login: "bob".to_owned(),
            parent_display_name: "Bob".to_owned(),
            parent_msg_body: "quoted question".to_owned(),
            ..Default::default()
        });
        let mut sub = message(2, "Smile");
        sub.spans = chat.spans.clone();
        sub.msg_kind = MsgKind::Sub {
            display_name: "Alice".to_owned(),
            months: 12,
            plan: "Tier 1".to_owned(),
            is_gift: false,
            sub_msg: "Smile".to_owned(),
        };
        let messages = [chat, sub];
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = frame(&ctx, &state, &messages, Vec::new()).0;
        }
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text().contains("quoted question"))));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text().contains("resubscribed with Tier 1"))));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "Smile")));
        let images: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) if rect.brush.is_some() => Some(rect.rect.size()),
                egui::Shape::Mesh(mesh) if mesh.texture_id != egui::TextureId::default() => {
                    Some(mesh.calc_bounds().size())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            images.len(),
            3,
            "one badge and two emotes must be painted: {images:?}"
        );
        assert_eq!(images.iter().filter(|size| size.x > size.y).count(), 2);
    }

    #[test]
    fn message_links_names_and_body_clicks_keep_distinct_actions() {
        let ctx = egui::Context::default();
        let mut state = PreviewTestState::default();
        let mut chat = message(1, "read this example link");
        chat.spans = smallvec::smallvec![
            Span::Text {
                text: "read this ".to_owned(),
                is_action: false
            },
            Span::Url {
                text: "example link".to_owned(),
                url: "https://example.test/".to_owned()
            },
        ];
        let messages = [chat];
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = frame(&ctx, &state, &messages, Vec::new()).0;
        }
        let link = text_center(&output, "example link");
        let name = text_center(&output, "Alice");
        let body = text_center(&output, "read this ");
        let (_, actions) = click(&ctx, &state, &messages, link, egui::PointerButton::Primary);
        assert!(
            actions.is_empty(),
            "link={link:?} name={name:?} body={body:?}; actions={actions:?}"
        );
        let sent = commands(&mut state);
        assert!(
            matches!(sent.as_slice(), [AppCommand::OpenUrl { url }]
            if url == "https://example.test/"),
            "link commands: {sent:?}"
        );
        let (_, actions) = click(&ctx, &state, &messages, name, egui::PointerButton::Primary);
        assert!(actions.jump_to.is_none());
        assert!(matches!(actions.profile_request, Some((channel, login, _))
            if channel == ChannelId::new("room") && login == "alice"));
        assert!(matches!(
            commands(&mut state).as_slice(),
            [AppCommand::ShowUserCard { .. }]
        ));
        let (_, actions) = click(&ctx, &state, &messages, body, egui::PointerButton::Primary);
        assert_eq!(
            actions.jump_to,
            Some((ChannelId::new("room"), MessageId(1)))
        );
        assert!(commands(&mut state).is_empty());
    }

    #[test]
    fn message_link_press_survives_virtualized_siblings() {
        let ctx = egui::Context::default();
        let mut state = PreviewTestState::default();
        let mut chat = message(1, "example link");
        chat.spans = smallvec::smallvec![Span::Url {
            text: "example link".to_owned(),
            url: "https://example.test/".to_owned(),
        }];
        let messages = [chat];
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = frame(&ctx, &state, &messages, Vec::new()).0;
        }
        let point = text_center(&output, "example link");
        for (pressed, skipped) in [(true, 0), (false, 200)] {
            let (_, actions) = frame_with_skipped_siblings(
                &ctx,
                &state,
                &messages,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                skipped,
            );
            assert!(actions.is_empty());
        }
        let sent = commands(&mut state);
        assert!(
            matches!(sent.as_slice(), [AppCommand::OpenUrl { url }]
            if url == "https://example.test/"),
            "link commands: {sent:?}"
        );
    }

    #[test]
    fn dragging_rich_row_text_copies_the_selection_without_navigating() {
        let ctx = egui::Context::default();
        let state = PreviewTestState::default();
        let body = "copy this selection";
        let messages = [message(1, body)];
        let mut output = egui::FullOutput::default();
        for _ in 0..3 { output = frame(&ctx, &state, &messages, Vec::new()).0; }
        let bounds = text_bounds(&output, body);
        let start = egui::pos2(bounds.left() + 0.1, bounds.center().y);
        let end = egui::pos2(bounds.right() - 0.1, bounds.center().y);
        for events in [
            vec![egui::Event::PointerMoved(start), egui::Event::PointerButton {
                pos: start, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE,
            }],
            vec![egui::Event::PointerMoved(end)],
            vec![egui::Event::PointerButton {
                pos: end, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE,
            }],
        ] {
            let (_, actions) = frame(&ctx, &state, &messages, events);
            assert!(actions.jump_to.is_none(), "selecting text must not navigate away from it");
        }
        let (output, _) = frame(&ctx, &state, &messages, vec![egui::Event::Copy]);
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == body)
        }), "copied commands: {:?}", output.platform_output.commands);
    }

    #[test]
    fn preview_context_menu_replies_to_the_original_message() {
        let ctx = egui::Context::default();
        let mut state = PreviewTestState::default();
        let messages = [message(7, "original message")];
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = frame(&ctx, &state, &messages, Vec::new()).0;
        }
        let body = text_center(&output, "original message");
        click(
            &ctx,
            &state,
            &messages,
            body,
            egui::PointerButton::Secondary,
        );
        let output = frame(&ctx, &state, &messages, Vec::new()).0;
        let reply = text_center(&output, "Reply");
        let (_, actions) = click(&ctx, &state, &messages, reply, egui::PointerButton::Primary);
        let (channel, reply) = actions
            .reply
            .expect("Reply opens the composer without sending");
        assert_eq!(channel, ChannelId::new("room"));
        assert_eq!(reply.parent_msg_id, "server-7");
        assert_eq!(reply.parent_msg_body, "original message");
        assert!(state.receiver.try_recv().is_err());
    }

    #[test]
    fn preview_context_menu_opens_the_original_thread_without_sending() {
        let ctx = egui::Context::default();
        let mut state = PreviewTestState::default();
        let messages = [message(7, "thread seed")];
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = frame(&ctx, &state, &messages, Vec::new()).0;
        }
        click(
            &ctx,
            &state,
            &messages,
            text_center(&output, "thread seed"),
            egui::PointerButton::Secondary,
        );
        let output = frame(&ctx, &state, &messages, Vec::new()).0;
        let (_, actions) = click(
            &ctx,
            &state,
            &messages,
            text_center(&output, "Open thread"),
            egui::PointerButton::Primary,
        );
        let seed = actions
            .open_thread
            .expect("the full row opens a conversation");
        assert_eq!(seed.server_id.as_deref(), Some("server-7"));
        assert_eq!(seed.raw_text, "thread seed");
        assert!(actions.reply.is_none());
        assert!(commands(&mut state).is_empty());
    }

    #[test]
    fn highlight_rule_edits_remeasure_offscreen_history_rows() {
        let ctx = egui::Context::default();
        let state = PreviewTestState::default();
        let messages: Vec<_> = (0..80)
            .map(|id| {
                message(
                    id,
                    if id == 70 {
                        "special keyword"
                    } else {
                        "ordinary history row"
                    },
                )
            })
            .collect();
        let mut cache = MessagePreviewCache::default();
        let render = |cache: &mut MessagePreviewCache,
                      rules: &[crust_core::highlight::HighlightMatch]| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360.0, 340.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let context = MessagePreviewContext {
                            highlight_rules: rules,
                            ..state.context()
                        };
                        cache.show_history(
                            ui,
                            Id::new("highlight_history"),
                            &messages,
                            &context,
                            false,
                        );
                    });
                },
            );
        };
        for _ in 0..3 {
            render(&mut cache, &[]);
        }
        let plain_height = cache.content_height;
        let rules =
            crust_core::highlight::compile_rules(&[crust_core::highlight::HighlightRule::new(
                "special",
            )]);
        render(&mut cache, &rules);
        assert_eq!(cache.measured_rows, messages.len());
        assert!(cache.content_height > plain_height);
        render(&mut cache, &rules);
        assert_eq!(cache.measured_rows, 0);
        render(&mut cache, &[]);
        assert!((cache.content_height - plain_height).abs() < 0.5);
    }

    #[test]
    fn history_measurements_follow_rich_content_and_image_sizes() {
        let ctx = egui::Context::default();
        let mut state = PreviewTestState::default();
        let mut messages: Vec<_> = (0..80).map(|id| message(id, "short history row")).collect();
        let mut cache = MessagePreviewCache::default();
        let render = |cache: &mut MessagePreviewCache,
                      state: &PreviewTestState,
                      messages: &[ChatMessage]| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360.0, 340.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        cache.show_history(
                            ui,
                            Id::new("history_test"),
                            messages,
                            &state.context(),
                            false,
                        );
                    });
                },
            );
        };
        for _ in 0..3 {
            render(&mut cache, &state, &messages);
        }
        assert_eq!(cache.measured_rows, 0);
        assert!(cache.rendered_rows < 20);
        let old_height = cache.content_height;
        messages[70].reply = Some(ReplyInfo {
            parent_msg_id: "parent".to_owned(),
            parent_user_login: "bob".to_owned(),
            parent_display_name: "Bob".to_owned(),
            parent_msg_body: "question".to_owned(),
            ..Default::default()
        });
        render(&mut cache, &state, &messages);
        assert_eq!(cache.measured_rows, 1);
        assert!(cache.content_height > old_height);
        messages[70].spans = smallvec::smallvec![emote("Smile")];
        render(&mut cache, &state, &messages);
        assert_eq!(cache.measured_rows, 1);
        image(&mut state, &ctx, "Smile", 80);
        render(&mut cache, &state, &messages);
        assert_eq!(cache.measured_rows, 1);
        messages[70].flags.is_deleted = true;
        render(&mut cache, &state, &messages);
        assert_eq!(cache.measured_rows, 1);
        messages[70].msg_kind = MsgKind::SystemInfo;
        render(&mut cache, &state, &messages);
        let old_height = cache.content_height;
        messages[70].raw_text =
            "Updated event details that wrap across the history panel. ".repeat(12);
        render(&mut cache, &state, &messages);
        assert_eq!(cache.measured_rows, 1);
        assert!(cache.content_height > old_height);
        messages.truncate(20);
        render(&mut cache, &state, &messages);
        assert_eq!(cache.rows.len(), 20);
    }
}
