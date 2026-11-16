use super::*;
use crust_core::model::{DetachedChatSettings, MAX_DETACHED_CHATS};
use egui::{ViewportBuilder, ViewportClass, ViewportCommand, ViewportId};

pub(super) struct DetachedChat {
    pub pane: Pane,
    pub reply: Option<ReplyInfo>,
    pub account_id: Option<String>,
    settings: DetachedChatSettings,
    picker: EmotePicker,
    focus_input: bool,
}

impl DetachedChat {
    fn new(
        settings: DetachedChatSettings,
        filters: &[crust_core::model::CompiledFilter],
        account_id: Option<String>,
    ) -> Self {
        let settings = settings.normalized();
        let id = SplitPanes::next_id();
        let mut pane = Pane {
            id,
            channel: ChannelId(settings.channel.clone()),
            input_buf: String::new(),
            view_scope: Some(id),
            search: MessageSearchState::for_pane(id),
            filters: PaneFilters::default(),
            frac: 1.0,
        };
        pane.restore_view_settings(&settings.view, filters);
        Self {
            pane,
            reply: None,
            account_id,
            settings,
            picker: EmotePicker::default().with_namespace(("detached_chat", id)),
            focus_input: false,
        }
    }

    fn viewport_id(&self) -> ViewportId {
        ViewportId::from_hash_of(("detached_chat", self.pane.id))
    }

    fn target(&self) -> crust_core::ComposerTarget {
        crust_core::ComposerTarget::Chat {
            account_id: self.account_id.clone(),
            pane_id: self.pane.id,
        }
    }

    fn snapshot(&self) -> DetachedChatSettings {
        DetachedChatSettings {
            channel: self.pane.channel.0.clone(),
            view: self.pane.view_settings(),
            ..self.settings.clone()
        }
    }

    fn title(&self) -> String {
        format!(
            "{} - {} - Crust",
            self.pane.channel.display_name(),
            if self.settings.overlay {
                "Overlay"
            } else {
                "Chat"
            }
        )
    }

    fn viewport_builder(&self, always_on_top: bool) -> ViewportBuilder {
        let builder = ViewportBuilder::default()
            .with_title(self.title())
            .with_inner_size(self.settings.size)
            .with_min_inner_size([240.0, 180.0])
            .with_maximized(self.settings.maximized)
            .with_resizable(true)
            .with_transparent(self.settings.overlay)
            .with_decorations(!self.settings.overlay || !self.settings.click_through)
            .with_mouse_passthrough(self.settings.overlay && self.settings.click_through)
            // This is a creation attribute; changing it recreates the native window.
            .with_active(false)
            .with_window_level(if always_on_top || self.settings.overlay {
                egui::viewport::WindowLevel::AlwaysOnTop
            } else {
                egui::viewport::WindowLevel::Normal
            });
        if let Some(position) = self.settings.position {
            builder.with_position(position)
        } else {
            builder
        }
    }

    fn focus_composer(&mut self, ctx: &Context, native: bool) {
        if !self.focus_input {
            return;
        }
        request_composer_focus(ctx, Some(chat_composer_id(Some(self.pane.id))));
        if native && !ctx.input(|input| input.focused) {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        self.focus_input = false;
    }

    fn sample_geometry(&mut self, ctx: &Context) {
        ctx.input(|input| {
            let viewport = input.viewport();
            self.settings.maximized = viewport.maximized.unwrap_or(false);
            if !self.settings.maximized && !viewport.minimized.unwrap_or(false) {
                if let Some(rect) = viewport
                    .inner_rect
                    .filter(|rect| rect.is_finite() && rect.is_positive())
                {
                    self.settings.size = [rect.width(), rect.height()];
                }
                if let Some(rect) = viewport.outer_rect.filter(|rect| rect.is_finite()) {
                    self.settings.position = Some([rect.min.x, rect.min.y]);
                }
            }
        });
    }
}

#[derive(Default)]
pub(super) struct DetachedChats {
    pub open: Vec<DetachedChat>,
    pub closed: VecDeque<DetachedChat>,
    pub manager_open: bool,
}

impl DetachedChats {
    pub fn restore(
        &mut self,
        settings: &[DetachedChatSettings],
        filters: &[crust_core::model::CompiledFilter],
        account: Option<String>,
    ) {
        self.open = settings
            .iter()
            .take(MAX_DETACHED_CHATS)
            .filter(|settings| !ChannelId(settings.channel.clone()).is_virtual())
            .map(|settings| DetachedChat::new(settings.clone(), filters, account.clone()))
            .collect();
    }

    pub fn settings(&self) -> Vec<DetachedChatSettings> {
        self.open.iter().map(DetachedChat::snapshot).collect()
    }

    pub fn owns_target(&self, target: &crust_core::ComposerTarget) -> bool {
        let crust_core::ComposerTarget::Chat { pane_id, .. } = target else {
            return false;
        };
        self.open
            .iter()
            .chain(&self.closed)
            .any(|window| window.pane.id == *pane_id)
    }

    pub fn invalidate_catalogs(&mut self) {
        for window in self.open.iter_mut().chain(&mut self.closed) {
            window.picker.invalidate_catalog();
        }
    }

    pub fn refresh_filters(&mut self, filters: &[crust_core::model::CompiledFilter]) {
        for window in self.open.iter_mut().chain(&mut self.closed) {
            window.pane.filters.refresh(filters);
        }
    }

    pub fn redirect_channel(&mut self, old: &ChannelId, new: &ChannelId) {
        for window in self.open.iter_mut().chain(&mut self.closed) {
            if &window.pane.channel == old {
                window.pane.channel = new.clone();
                window.settings.channel = new.0.clone();
            }
        }
    }
}

#[derive(Default)]
struct WindowActions {
    close: bool,
    submission: Option<(String, Option<ReplyInfo>)>,
    uploads: Vec<crate::widgets::chat_input::PendingUpload>,
    dictionary_word: Option<String>,
    profile: Option<(String, Vec<crust_core::model::Badge>)>,
    thread: Option<ChatMessage>,
    load_history: Option<i64>,
    picker_preferences: Option<EmotePickerPreferences>,
    edit_filters: bool,
    new_window: Option<bool>,
}

impl CrustApp {
    pub(super) fn open_detached_chat(
        &mut self,
        channel: ChannelId,
        view: PaneViewSettings,
        overlay: bool,
    ) {
        if channel.is_virtual() || self.detached_chats.open.len() >= MAX_DETACHED_CHATS {
            self.detached_chats.manager_open = true;
            self.focus_main_viewport();
            return;
        }
        self.cancel_pending_layout_restore();
        let channel_tab_visible = self.state.channel_order.contains(&channel);
        let settings = DetachedChatSettings {
            channel: channel.0,
            channel_tab_visible,
            view,
            overlay,
            ..Default::default()
        };
        let mut window = DetachedChat::new(
            settings,
            &self.filter_records,
            self.state.auth.user_id.clone(),
        );
        window.focus_input = true;
        self.detached_chats.open.push(window);
    }

    fn keep_detached_draft(&mut self, window: &mut DetachedChat, reason: &str) {
        if !window.pane.input_buf.is_empty() {
            let reply = window.reply.take();
            self.unsent_messages.push(UnsentMessage {
                channel: window.pane.channel.clone(),
                text: std::mem::take(&mut window.pane.input_buf),
                reply_to_msg_id: reply.as_ref().map(|reply| reply.parent_msg_id.clone()),
                reply,
                composer: Some(window.target()),
                reason: reason.to_owned(),
            });
        }
        window.reply = None;
    }

    fn close_detached_chat(&mut self, id: u64, ctx: &Context) {
        let Some(index) = self
            .detached_chats
            .open
            .iter()
            .position(|window| window.pane.id == id)
        else {
            return;
        };
        let mut window = self.detached_chats.open.remove(index);
        let channel = window.pane.channel.clone();
        ctx.send_viewport_cmd_to(window.viewport_id(), ViewportCommand::Close);
        window.picker.open = false;
        window.pane.filters.editor_open = false;
        self.detached_chats.closed.push_back(window);
        if !self.state.channel_order.contains(&channel)
            && !self
                .detached_chats
                .open
                .iter()
                .any(|window| window.pane.channel == channel)
        {
            self.send_cmd(AppCommand::LeaveChannel {
                channel: channel.clone(),
            });
            self.state.leave_channel(&channel);
        }
        while self.detached_chats.closed.len() > MAX_DETACHED_CHATS {
            if let Some(mut oldest) = self.detached_chats.closed.pop_front() {
                self.keep_detached_draft(
                    &mut oldest,
                    "The closed chat window left the reopen history. Your draft has been kept.",
                );
            }
        }
    }

    fn reopen_detached_chat(&mut self, id: u64) {
        if self.detached_chats.open.len() >= MAX_DETACHED_CHATS {
            return;
        }
        let Some(index) = self
            .detached_chats
            .closed
            .iter()
            .position(|window| window.pane.id == id)
        else {
            return;
        };
        let mut window = self.detached_chats.closed.remove(index).unwrap();
        window.settings.click_through = false;
        window.focus_input = true;
        if !self.state.channels.contains_key(&window.pane.channel) {
            self.send_cmd(AppCommand::JoinChannel {
                channel: window.pane.channel.clone(),
            });
        } else {
            window.settings.channel_tab_visible =
                self.state.channel_order.contains(&window.pane.channel);
        }
        self.detached_chats.open.push(window);
    }

    pub(super) fn focus_detached_composer(&mut self, id: u64, ctx: &Context) -> bool {
        if !self.detached_chats.open.iter().any(|window| window.pane.id == id) {
            self.reopen_detached_chat(id);
        }
        let Some(window) = self.detached_chats.open.iter_mut().find(|window| window.pane.id == id) else {
            return false;
        };
        window.settings.click_through = false;
        window.picker.open = false;
        window.focus_input = true;
        for command in [ViewportCommand::MousePassthrough(false),
            ViewportCommand::Minimized(false), ViewportCommand::Focus] {
            ctx.send_viewport_cmd_to(window.viewport_id(), command);
        }
        ctx.request_repaint();
        true
    }

    pub(super) fn retain_detached_channel(&mut self, channel: &ChannelId) -> bool {
        let retained = self
            .detached_chats
            .open
            .iter()
            .any(|window| &window.pane.channel == channel);
        if retained {
            for window in self
                .detached_chats
                .open
                .iter_mut()
                .chain(&mut self.detached_chats.closed)
            {
                if &window.pane.channel == channel {
                    window.settings.channel_tab_visible = false;
                }
            }
            self.state.channel_order.retain(|item| item != channel);
            if self.state.active_channel.as_ref() == Some(channel) {
                self.state.active_channel = self.state.channel_order.first().cloned();
            }
        }
        retained
    }

    fn restore_detached_channel_tab(&mut self, id: u64) {
        let Some(channel) = self
            .detached_chats
            .open
            .iter()
            .find(|window| window.pane.id == id)
            .map(|window| window.pane.channel.clone())
        else {
            return;
        };
        self.activate_channel(channel);
        self.focus_main_viewport();
    }

    pub(super) fn reveal_detached_channel_tab(&mut self, channel: &ChannelId) {
        let mut retained = false;
        for window in self
            .detached_chats
            .open
            .iter_mut()
            .chain(&mut self.detached_chats.closed)
        {
            if &window.pane.channel == channel {
                window.settings.channel_tab_visible = true;
                retained = true;
            }
        }
        if retained
            && self.state.channels.contains_key(channel)
            && !self.state.channel_order.contains(channel)
        {
            self.state.channel_order.push(channel.clone());
        }
    }

    fn reconcile_detached_channel_tabs(&mut self) {
        for window in &self.detached_chats.open {
            if !window.settings.channel_tab_visible {
                self.state
                    .channel_order
                    .retain(|channel| channel != &window.pane.channel);
                if self.state.active_channel.as_ref() == Some(&window.pane.channel) {
                    self.state.active_channel = self.state.channel_order.first().cloned();
                }
            }
        }
    }

    fn show_chat_window_manager(&mut self, ctx: &Context) {
        if !self.detached_chats.manager_open {
            return;
        }
        let mut open = true;
        let mut close = Vec::new();
        let mut reopen = None;
        let mut restore_tab = None;
        let bounds = ctx.screen_rect().shrink(8.0);
        let style = ctx.style();
        let frame = Frame::window(&style)
            .fill(t::bg_dialog())
            .stroke(t::stroke_subtle());
        let title_font = egui::TextStyle::Heading.resolve(&style);
        let title_height = ctx
            .fonts(|fonts| fonts.row_height(&title_font))
            .max(style.spacing.interact_size.y)
            + frame.inner_margin.sum().y
            + frame.stroke.width;
        let maximum = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
            .max(egui::Vec2::splat(1.0));
        egui::Window::new("Chat windows")
            .id(egui::Id::new("chat_window_manager"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(440.0, 360.0).min(maximum))
            .min_size(egui::vec2(140.0, 60.0).min(maximum))
            .max_size(maximum)
            .constrain_to(bounds)
            .frame(frame)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height().max(0.0))
                    .min_scrolled_height(0.0)
                    .show(ui, |ui| {
                        ui.label("Use Interact to regain control of a click-through overlay.");
                        for window in &mut self.detached_chats.open {
                            ui.push_id(window.pane.id, |ui| {
                                ui.separator();
                                ui.label(
                                    RichText::new(window.title())
                                        .font(t::small())
                                        .color(t::text_primary()),
                                );
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .button(if window.settings.click_through {
                                            "Interact"
                                        } else {
                                            "Show"
                                        })
                                        .clicked()
                                    {
                                        window.settings.click_through = false;
                                        window.focus_input = true;
                                        ctx.send_viewport_cmd_to(
                                            window.viewport_id(),
                                            ViewportCommand::MousePassthrough(false),
                                        );
                                        ctx.send_viewport_cmd_to(
                                            window.viewport_id(),
                                            ViewportCommand::Minimized(false),
                                        );
                                        ctx.send_viewport_cmd_to(
                                            window.viewport_id(),
                                            ViewportCommand::Focus,
                                        );
                                    }
                                    if window.settings.overlay {
                                        ui.checkbox(
                                            &mut window.settings.click_through,
                                            "Click-through",
                                        );
                                    }
                                    if !window.settings.channel_tab_visible
                                        && ui.button("Show tab").clicked()
                                    {
                                        restore_tab = Some(window.pane.id);
                                    }
                                    if ui.button("Reset position").clicked() {
                                        let origin = ctx
                                            .input(|input| {
                                                input.viewport().outer_rect.map(|rect| rect.min)
                                            })
                                            .unwrap_or_default();
                                        let position = origin + egui::vec2(32.0, 32.0);
                                        window.settings.position = Some([position.x, position.y]);
                                        window.settings.size = [480.0, 600.0];
                                        window.settings.maximized = false;
                                        window.settings.click_through = false;
                                        window.focus_input = true;
                                        for command in [
                                            ViewportCommand::MousePassthrough(false),
                                            ViewportCommand::Maximized(false),
                                            ViewportCommand::Minimized(false),
                                            ViewportCommand::OuterPosition(position),
                                            ViewportCommand::InnerSize(egui::vec2(480.0, 600.0)),
                                            ViewportCommand::Focus,
                                        ] {
                                            ctx.send_viewport_cmd_to(window.viewport_id(), command);
                                        }
                                    }
                                    if ui.button("Close window").clicked() {
                                        close.push(window.pane.id);
                                    }
                                });
                                if window.settings.overlay {
                                    let label = ui.label("Background");
                                    ui.spacing_mut().slider_width = ui.available_width().max(1.0);
                                    ui.add(
                                        egui::Slider::new(
                                            &mut window.settings.background_opacity,
                                            0.0..=1.0,
                                        )
                                        .show_value(false),
                                    )
                                    .labelled_by(label.id);
                                }
                            });
                        }
                        if self.detached_chats.open.is_empty() {
                            ui.label("No open chat windows.");
                        }
                        if self.detached_chats.open.len() >= MAX_DETACHED_CHATS {
                            ui.label("Close a chat window before opening another.");
                        }
                        if !self.detached_chats.closed.is_empty() {
                            ui.separator();
                            ui.label("Recently closed");
                            for window in self.detached_chats.closed.iter().rev() {
                                ui.push_id(window.pane.id, |ui| {
                                    if ui
                                        .add_enabled(
                                            self.detached_chats.open.len() < MAX_DETACHED_CHATS,
                                            egui::Button::new(format!("Reopen {}", window.title())),
                                        )
                                        .clicked()
                                    {
                                        reopen = Some(window.pane.id);
                                    }
                                });
                            }
                        }
                    });
            });
        self.detached_chats.manager_open = open;
        for id in close {
            self.close_detached_chat(id, ctx);
        }
        if let Some(id) = reopen {
            self.reopen_detached_chat(id);
        }
        if let Some(id) = restore_tab {
            self.restore_detached_channel_tab(id);
        }
    }

    pub(super) fn show_detached_chats(&mut self, ctx: &Context, stats: &mut ChatPerfStats) {
        self.reconcile_detached_channel_tabs();
        self.show_chat_window_manager(ctx);
        let mut windows = std::mem::take(&mut self.detached_chats.open);
        let mut outputs = Vec::new();
        for window in &mut windows {
            if window.pane.channel.is_twitch() && window.account_id != self.state.auth.user_id {
                self.keep_detached_draft(
                    window,
                    "The account changed while this chat window had an unsent draft.",
                );
                window.account_id = self.state.auth.user_id.clone();
                window.picker.open = false;
            }
            let mut output = WindowActions::default();
            self.ensure_input_emote_catalog(&window.pane.channel);
            window
                .picker
                .apply_preferences(&self.emote_picker.preferences());
            let builder = window.viewport_builder(self.always_on_top);
            ctx.show_viewport_immediate(window.viewport_id(), builder, |child, class| {
                if child.input(|input| input.viewport().close_requested()) {
                    output.close = true;
                    return;
                }
                let embedded = matches!(class, ViewportClass::Embedded);
                if !embedded {
                    window.sample_geometry(child);
                }
                let animate = !self.animations_when_focused || child.input(|input| input.focused);
                let passthrough =
                    window.settings.overlay && window.settings.click_through && !embedded;
                if !passthrough {
                    window.focus_composer(child, !embedded);
                    self.handle_chat_window_shortcuts(child, window, &mut output);
                    if output.close {
                        return;
                    }
                }
                if embedded {
                    let mut open = true;
                    let bounds = child.screen_rect().shrink(8.0);
                    let style = child.style();
                    let frame = Frame::window(&style);
                    let title_font = egui::TextStyle::Heading.resolve(&style);
                    let title_height = child
                        .fonts(|fonts| fonts.row_height(&title_font))
                        .max(style.spacing.interact_size.y)
                        + frame.inner_margin.sum().y
                        + frame.stroke.width;
                    let maximum = (bounds.size()
                        - frame.total_margin().sum()
                        - egui::vec2(0.0, title_height))
                    .max(egui::Vec2::splat(1.0));
                    egui::Window::new(window.title())
                        .id(egui::Id::new("detached_chat_fallback").with(window.pane.id))
                        .open(&mut open)
                        .default_size(egui::Vec2::from(window.settings.size).min(maximum))
                        .min_size(egui::vec2(140.0, 100.0).min(maximum))
                        .max_size(maximum)
                        .constrain_to(bounds)
                        .frame(frame)
                        .resizable(true)
                        .collapsible(false)
                        .show(child, |ui| {
                            self.render_detached_chat(
                                ui,
                                window,
                                animate,
                                false,
                                stats,
                                &mut output,
                            )
                        });
                    output.close |= !open;
                } else {
                    let background = if window.settings.overlay {
                        t::alpha(
                            t::bg_base(),
                            (window.settings.background_opacity * 255.0).round() as u8,
                        )
                    } else {
                        t::bg_base()
                    };
                    CentralPanel::default()
                        .frame(Frame::new().fill(background).inner_margin(Margin::ZERO))
                        .show(child, |ui| {
                            self.render_detached_chat(
                                ui,
                                window,
                                animate,
                                passthrough,
                                stats,
                                &mut output,
                            )
                        });
                }
                if !passthrough {
                    output.edit_filters = window.pane.filters.show(
                        child,
                        &window.pane.channel,
                        Some(window.pane.id),
                        &self.settings_filter_records,
                        &self.filter_records,
                    );
                    let before = window.picker.preferences();
                    let picker_open = window.picker.open;
                    if let Some(code) = window.picker.show(
                        child,
                        self.input_emote_catalogs
                            .get(&window.pane.channel)
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                        &self.emote_bytes,
                        &self.cmd_tx,
                        animate,
                    ) {
                        insert_picker_emote(
                            child,
                            chat_composer_id(Some(window.pane.id)),
                            &mut window.pane.input_buf,
                            &code,
                        );
                        window.focus_input = true;
                    } else if picker_open && !window.picker.open {
                        window.focus_input = true;
                    }
                    let after = window.picker.preferences();
                    if before != after {
                        output.picker_preferences = Some(after);
                    }
                }
            });
            outputs.push((window.pane.id, output));
        }
        self.detached_chats.open = windows;
        for (id, output) in outputs {
            self.apply_chat_window_actions(id, output, ctx);
        }
    }

    fn handle_chat_window_shortcuts(
        &mut self,
        ctx: &Context,
        window: &mut DetachedChat,
        output: &mut WindowActions,
    ) {
        let id = chat_composer_id(Some(window.pane.id));
        let owner = crate::widgets::chat_input::composer_owns_keyboard(ctx, id)
            .then_some((id, &window.pane.channel));
        crate::widgets::chat_input::prepare_composer_events(
            ctx,
            &self.hotkey_bindings,
            owner,
            self.state.auth.user_id.as_deref(),
        );
        self.handle_font_zoom_shortcuts(ctx);
        let close = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::CloseChannel);
        if ctx.input_mut(|input| consume_binding(input, &close)) {
            output.close = true;
            return;
        }
        if self.handle_settings_shortcut(ctx) {
            self.focus_main_viewport();
        }
        let quick_switch = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::OpenQuickSwitcher);
        if ctx.input_mut(|input| consume_binding(input, &quick_switch)) {
            self.open_channel_quick_switch();
            self.focus_main_viewport();
        }
        let search = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::OpenMessageSearch);
        if ctx.input_mut(|input| consume_binding(input, &search)) {
            window.pane.search.request_open();
        }
        let global_search = self
            .hotkey_bindings
            .get(crust_core::HotkeyAction::ToggleGlobalSearch);
        if ctx.input_mut(|input| consume_binding(input, &global_search)) {
            if self.global_search.open {
                self.global_search.close();
            } else {
                self.global_search.request_open();
            }
            self.focus_main_viewport();
        }
        let picker_was_open = window.picker.open;
        window.picker.handle_shortcuts(ctx);
        if picker_was_open && !window.picker.open {
            window.focus_input = true;
        }
        if !picker_was_open
            && window.pane.search.open
            && !ctx.is_context_menu_open()
            && ctx.input_mut(|input| {
                consume_exact_key(input, egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            window.pane.search.close();
            window.focus_input = true;
        }
    }

    fn render_detached_chat(
        &mut self,
        ui: &mut egui::Ui,
        window: &mut DetachedChat,
        animate: bool,
        passthrough: bool,
        stats: &mut ChatPerfStats,
        output: &mut WindowActions,
    ) {
        let ctx = ui.ctx().clone();
        let rect = ui.available_rect_before_wrap();
        let channel = window.pane.channel.clone();
        let mut header_bottom = rect.top();
        if !passthrough {
            let metadata = split_header_meta_text(
                self.stream_statuses
                    .get(&channel.display_name().to_ascii_lowercase()),
                self.split_header_show_viewer_count
                    && !(self.streamer_mode_active && self.streamer_hide_viewer_counts),
                self.split_header_show_game,
                self.split_header_show_title,
                self.channel_points.get(&channel).copied(),
            );
            let header = show_split_header(
                ui,
                rect,
                &channel,
                ctx.input(|input| input.focused),
                0,
                0,
                window.pane.search.open,
                metadata.as_deref(),
                SplitHeaderOptions {
                    can_split: false,
                    can_reopen: false,
                    can_close_others: false,
                },
            );
            output.close |= header.close_clicked;
            if header.focus_clicked {
                window.focus_input = true;
            }
            if header.toggle_search_clicked {
                if window.pane.search.open {
                    window.pane.search.close();
                } else {
                    window.pane.search.request_open();
                }
            }
            if header.filters_clicked {
                window.pane.filters.editor_open = true;
            }
            if header.manage_windows_clicked {
                self.detached_chats.manager_open = true;
                self.focus_main_viewport();
            }
            if header.open_window_clicked || header.open_overlay_clicked {
                output.new_window = Some(header.open_overlay_clicked);
            }
            header_bottom += split_header_height();
            if window.settings.overlay {
                let controls =
                    egui::Rect::from_min_max(egui::pos2(rect.left(), header_bottom), rect.max);
                let mut controls_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("overlay_controls")
                        .max_rect(controls)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                controls_ui.set_clip_rect(rect);
                controls_ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Click-through")
                        .on_hover_text("Restore interaction from Chat windows in the main window.")
                        .clicked()
                    {
                        window.settings.click_through = true;
                        window.picker.open = false;
                        ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(true));
                        self.focus_main_viewport();
                    }
                    if ui.button("Chat windows").clicked() {
                        self.detached_chats.manager_open = true;
                        self.focus_main_viewport();
                    }
                });
                header_bottom = controls_ui.min_rect().bottom() + 3.0;
            }
        }
        let mut content_bottom = rect.bottom();
        if !passthrough {
            let height = (t::chat_font_size() + 16.0).max(t::bar_h())
                + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom) as f32
                + 2.0
                + crate::widgets::chat_input::chat_input_counter_height(
                    &ctx,
                    rect.width() - 4.0,
                    &channel,
                    &window.pane.input_buf,
                )
                + if window.reply.is_some() { 36.0 } else { 0.0 };
            let top = (rect.bottom() - height).max(header_bottom.min(rect.bottom()));
            let input_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 2.0, top),
                rect.max - egui::vec2(2.0, 2.0),
            );
            content_bottom = input_rect.top() - 2.0;
            ui.painter().rect_filled(input_rect, 0.0, t::bg_surface());
            ui.painter()
                .hline(input_rect.x_range(), input_rect.top(), t::stroke_subtle());
            let mut input_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("detached_input", window.pane.id))
                    .max_rect(input_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            input_ui.set_clip_rect(input_rect.intersect(rect));
            let input = ChatInput {
                id: Some(chat_composer_id(Some(window.pane.id))),
                hotkey_bindings: &self.hotkey_bindings,
                channel: &channel,
                logged_in: self.state.auth.logged_in,
                username: self.state.auth.username.as_deref(),
                emote_catalog: self
                    .input_emote_catalogs
                    .get(&channel)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                emote_bytes: &self.emote_bytes,
                emote_picker_open: window.picker.open,
                pending_reply: window.reply.as_ref(),
                message_history: &self.message_history,
                slash_usage_counts: &self.slash_usage_counts,
                known_channels: &self.state.channel_order,
                chatters: self
                    .sorted_chatters
                    .get(&channel)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                prevent_overlong_twitch_messages: self.prevent_overlong_twitch_messages,
                animate_emotes: animate,
                accept_dropped_files: ctx.pointer_latest_pos().map_or_else(
                    || {
                        crate::widgets::chat_input::composer_owns_keyboard(
                            &ctx,
                            chat_composer_id(Some(window.pane.id)),
                        )
                    },
                    |position| {
                        rect.contains(position) && ctx.layer_id_at(position) == Some(ui.layer_id())
                    },
                ),
            }
            .show(&mut input_ui, &mut window.pane.input_buf);
            output.uploads = input.uploads;
            output.dictionary_word = input.add_to_dictionary;
            if input.dismiss_reply {
                window.reply = None;
            }
            if let Some(text) = input.send {
                output.submission = Some((text, window.reply.clone()));
            }
            if input.toggle_emote_picker {
                window.picker.toggle();
            }
        }
        if !passthrough && window.pane.search.open {
            if let Some(state) = self.state.channels.get(&channel) {
                window.pane.search.local_history_exhausted = state.local_history_exhausted;
                if should_use_search_window(rect.width()) {
                    show_message_search_window(
                        &ctx,
                        &channel,
                        &state.messages,
                        &mut window.pane.search,
                        self.always_on_top || window.settings.overlay,
                    );
                } else {
                    let search_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.left(), header_bottom),
                        egui::pos2(rect.right(), content_bottom.max(header_bottom)),
                    );
                    let mut search_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt("detached_search")
                            .max_rect(search_rect)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    search_ui.set_clip_rect(search_rect);
                    header_bottom += show_message_search_inline(
                        &mut search_ui,
                        &channel,
                        &state.messages,
                        &mut window.pane.search,
                    ) + 2.0;
                }
                if window.pane.search.take_load_more_local_request() {
                    output.load_history = Some(
                        state
                            .messages
                            .front()
                            .map(|message| message.timestamp.timestamp_millis())
                            .unwrap_or(i64::MAX),
                    );
                }
            }
        }
        let message_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left(), header_bottom.min(content_bottom)),
            egui::pos2(rect.right(), content_bottom),
        );
        if message_rect.height() <= 8.0 {
            return;
        }
        let mut messages_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("detached_messages", window.pane.id))
                .max_rect(message_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        messages_ui.set_clip_rect(message_rect);
        if let Some(session) = self.state.shared_chat_sessions.get(&channel) {
            show_shared_chat_banner(&mut messages_ui, session, &self.emote_bytes);
        }
        if let Some(train) = self.state.hype_train_for(&channel) {
            show_hype_train_banner(&mut messages_ui, train);
        }
        if let Some(raid) = self.state.raid_banner_for(&channel).cloned() {
            if show_raid_banner(&mut messages_ui, &raid) {
                self.state.dismiss_raid_banner(&channel);
            }
        }
        if !passthrough && ctx.input(|input| input.focused) {
            if let Some(state) = self.state.channels.get_mut(&channel) {
                state.mark_read();
            }
        }
        if let Some(state) = self.state.channels.get(&channel) {
            let moderator = state.is_mod
                || self
                    .state
                    .auth
                    .username
                    .as_deref()
                    .is_some_and(|login| login.eq_ignore_ascii_case(channel.display_name()));
            let result = MessageList::new(
                &state.messages,
                &self.emote_bytes,
                &self.cmd_tx,
                &channel,
                &self.link_previews,
                Some(&window.pane.search),
                self.collapse_long_messages,
                self.collapse_long_message_lines,
                animate,
                self.show_timestamps,
                self.show_timestamp_seconds,
                self.use_24h_timestamps,
                moderator,
                &self.ignores_set,
                &self.compiled_ignored_users,
                &self.highlight_rules,
                window.pane.filters.records(&self.filter_records),
                &self.mod_action_presets,
                &state.low_trust_users,
            )
            // Main-tab activation must not reset another window's reading position.
            .with_optional_pane(Some(window.pane.id), false)
            .with_hide_link_previews(self.streamer_mode_active && self.streamer_hide_link_previews)
            .with_channel_status(
                self.live_map_cache.get(channel.display_name()).copied(),
                true,
            )
            .show(&mut messages_ui);
            stats.accumulate(&result.perf_stats);
            if let Some(reply) = result.reply {
                window.reply = Some(reply);
                window.focus_input = true;
            }
            output.profile = result.profile_request;
            output.thread = result.open_thread;
        } else {
            messages_ui.label("Waiting for this channel to join.");
        }
    }

    fn apply_chat_window_actions(&mut self, id: u64, output: WindowActions, ctx: &Context) {
        let Some(window) = self
            .detached_chats
            .open
            .iter()
            .find(|window| window.pane.id == id)
        else {
            return;
        };
        let channel = window.pane.channel.clone();
        let target = window.target();
        let view = window.pane.view_settings();
        if let Some(overlay) = output.new_window {
            self.open_detached_chat(channel.clone(), view, overlay);
        }
        for upload in output.uploads {
            self.send_cmd(AppCommand::UploadImage {
                channel: channel.clone(),
                bytes: upload.bytes,
                format: upload.format,
                source_path: upload.source_path.map(|path| path.display().to_string()),
                composer: Some(target.clone()),
            });
        }
        if let Some(word) = output.dictionary_word {
            self.send_cmd(AppCommand::AddWordToDictionary { word });
        }
        if let Some((text, reply)) = output.submission {
            if self.submit_composer_input(&channel, text, reply, target) {
                if let Some(window) = self
                    .detached_chats
                    .open
                    .iter_mut()
                    .find(|window| window.pane.id == id)
                {
                    window.reply = None;
                }
            }
        }
        if let Some(oldest) = output.load_history {
            self.request_older_local_history(&channel, oldest);
        }
        if let Some(preferences) = output.picker_preferences {
            self.emote_picker.apply_preferences(&preferences);
        }
        if let Some((login, badges)) = output.profile {
            let moderator = self
                .state
                .channels
                .get(&channel)
                .is_some_and(|state| state.is_mod)
                || self
                    .state
                    .auth
                    .username
                    .as_deref()
                    .is_some_and(|login| login.eq_ignore_ascii_case(channel.display_name()));
            self.user_profile_popup
                .set_loading(&login, badges, Some(channel), moderator);
            self.focus_main_viewport();
        }
        if let Some(message) = output.thread {
            self.open_reply_thread(message);
        }
        if output.edit_filters {
            self.settings_open = true;
            self.settings_section = SettingsSection::Filters;
            self.focus_main_viewport();
        }
        if output.close {
            self.close_detached_chat(id, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (CrustApp, mpsc::Receiver<AppCommand>) {
        let (commands, receiver) = mpsc::channel(64);
        let (_, events) = mpsc::channel(64);
        let mut app = CrustApp::with_event_channels(commands, events);
        app.chat_font_size = t::DEFAULT_CHAT_FONT_SIZE;
        app.ui_font_size = t::DEFAULT_UI_FONT_SIZE;
        app.state.auth.logged_in = true;
        app.state.auth.user_id = Some("alice-id".into());
        app.state.auth.username = Some("alice".into());
        app.state.join_channel(ChannelId::new("room"));
        (app, receiver)
    }

    fn open(app: &mut CrustApp, overlay: bool) -> u64 {
        app.open_detached_chat(ChannelId::new("room"), PaneViewSettings::default(), overlay);
        app.detached_chats.open.last().unwrap().pane.id
    }

    #[test]
    fn explicit_restore_reveals_the_original_detached_composer() {
        for closed in [false, true] {
            let (mut app, _commands) = app();
            let ctx = Context::default();
            app.egui_ctx = Some(ctx.clone());
            let id = open(&mut app, true);
            let window = &mut app.detached_chats.open[0];
            window.focus_input = false;
            window.settings.click_through = true;
            let message = UnsentMessage {
                channel: window.pane.channel.clone(), text: "recovered window draft".into(),
                reply_to_msg_id: None, reply: None, composer: Some(window.target()),
                reason: "The message was not sent.".into(),
            };
            if closed { app.close_detached_chat(id, &ctx); }
            assert!(app.restore_targeted_unsent(&message, true));
            let window = app.detached_chats.open.iter().find(|window| window.pane.id == id)
                .expect("the recovered window must be visible");
            assert_eq!(window.pane.input_buf, message.text);
            assert!(window.focus_input);
            assert!(!window.settings.click_through);
        }
    }

    #[test]
    fn explicit_restore_keeps_a_closed_window_recoverable_at_the_window_limit() {
        let (mut app, _commands) = app();
        let ctx = Context::default();
        app.egui_ctx = Some(ctx.clone());
        let id = open(&mut app, false);
        let window = &app.detached_chats.open[0];
        let message = UnsentMessage {
            channel: window.pane.channel.clone(), text: "recovered window draft".into(),
            reply_to_msg_id: None, reply: None, composer: Some(window.target()),
            reason: "The message was not sent.".into(),
        };
        app.close_detached_chat(id, &ctx);
        for _ in 0..MAX_DETACHED_CHATS { open(&mut app, false); }
        assert!(!app.can_restore_unsent(&message, true));
        assert!(!app.restore_targeted_unsent(&message, true));
        assert!(app.detached_chats.closed[0].pane.input_buf.is_empty());
    }

    #[test]
    fn automatic_restore_does_not_reopen_a_closed_chat_window() {
        let (mut app, _commands) = app();
        let ctx = Context::default();
        let id = open(&mut app, false);
        let window = &mut app.detached_chats.open[0];
        window.focus_input = false;
        let message = UnsentMessage {
            channel: window.pane.channel.clone(), text: "recovered window draft".into(),
            reply_to_msg_id: None, reply: None, composer: Some(window.target()),
            reason: "The message was not sent.".into(),
        };
        app.close_detached_chat(id, &ctx);
        assert!(app.restore_targeted_unsent(&message, false));
        assert!(app.detached_chats.open.is_empty());
        assert_eq!(app.detached_chats.closed[0].pane.input_buf, message.text);
        assert!(!app.detached_chats.closed[0].focus_input);
    }

    #[test]
    fn closing_picker_and_changing_overlay_interaction_keep_the_native_window() {
        for overlay in [false, true] {
            let (mut app, _) = app();
            open(&mut app, overlay);
            let ctx = Context::default();
            ctx.enable_accesskit();
            let run = |app: &mut CrustApp, events| {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(800.0, 600.0),
                        )),
                        events,
                        focused: true,
                        ..Default::default()
                    },
                    |ctx| app.show_detached_chats(ctx, &mut ChatPerfStats::default()),
                )
            };
            for _ in 0..3 {
                run(&mut app, Vec::new());
            }
            let mut native_builder = app.detached_chats.open[0].viewport_builder(false);
            app.detached_chats.open[0].picker.toggle();
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = run(&mut app, Vec::new());
            }
            let picker_state = |output: &egui::FullOutput| {
                output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some("Emote picker"))
                    .expect("the detached composer exposes its picker button")
                    .1
                    .toggled()
            };
            assert_eq!(picker_state(&output), Some(egui::accesskit::Toggled::True));
            assert!(app.detached_chats.open[0].picker.owns_keyboard(&ctx));
            run(&mut app, shortcut(egui::Key::Escape, egui::Modifiers::NONE));
            let window = &app.detached_chats.open[0];
            assert!(!window.picker.open);
            assert!(window.focus_input);
            assert!(
                !native_builder.patch(window.viewport_builder(false)).1,
                "closing the picker must not recreate its chat window"
            );
            let output = run(&mut app, Vec::new());
            assert_eq!(picker_state(&output), Some(egui::accesskit::Toggled::False));
            let window = &app.detached_chats.open[0];
            assert!(!window.focus_input);
            assert!(
                !native_builder.patch(window.viewport_builder(false)).1,
                "consuming composer focus must not recreate the window on the next frame"
            );
            if overlay {
                for click_through in [true, false] {
                    let window = &mut app.detached_chats.open[0];
                    window.settings.click_through = click_through;
                    window.focus_input = !click_through;
                    let (commands, recreate) = native_builder.patch(window.viewport_builder(false));
                    assert!(
                        !recreate,
                        "changing overlay interaction must keep its window"
                    );
                    assert!(commands.iter().any(|command| matches!(command,
                        ViewportCommand::MousePassthrough(enabled) if *enabled == click_through)));
                }
            }
        }
    }

    #[test]
    fn composer_focus_requests_activate_only_an_unfocused_native_window() {
        let mut window = DetachedChat::new(
            DetachedChatSettings {
                channel: "room".into(),
                ..Default::default()
            },
            &[],
            None,
        );
        let ctx = Context::default();
        for (native, focused, requested, activates) in [
            (true, false, true, true),
            (true, true, true, false),
            (true, false, false, false),
            (false, false, true, false),
        ] {
            window.focus_input = requested;
            let output = ctx.run(
                egui::RawInput {
                    focused,
                    ..Default::default()
                },
                |ctx| window.focus_composer(ctx, native),
            );
            let focus_count = output
                .viewport_output
                .values()
                .flat_map(|viewport| &viewport.commands)
                .filter(|command| matches!(command, ViewportCommand::Focus))
                .count();
            assert_eq!(focus_count, usize::from(activates));
            assert!(!window.focus_input);
        }
    }

    fn surface_frame(
        app: &mut CrustApp,
        ctx: &Context,
        id: u64,
        events: Vec<egui::Event>,
        size: egui::Vec2,
    ) -> egui::FullOutput {
        surface_frame_with_focus(app, ctx, id, events, size, true)
    }

    fn surface_frame_with_focus(
        app: &mut CrustApp,
        ctx: &Context,
        id: u64,
        events: Vec<egui::Event>,
        size: egui::Vec2,
        focused: bool,
    ) -> egui::FullOutput {
        let index = app
            .detached_chats
            .open
            .iter()
            .position(|window| window.pane.id == id)
            .unwrap();
        let mut window = app.detached_chats.open.remove(index);
        app.ensure_input_emote_catalog(&window.pane.channel);
        let mut actions = WindowActions::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                focused,
                ..Default::default()
            },
            |ctx| {
                let composer = chat_composer_id(Some(id));
                if window.focus_input {
                    request_composer_focus(ctx, Some(composer));
                    window.focus_input = false;
                }
                app.handle_chat_window_shortcuts(ctx, &mut window, &mut actions);
                if actions.close {
                    return;
                }
                CentralPanel::default().frame(Frame::NONE).show(ctx, |ui| {
                    app.render_detached_chat(
                        ui,
                        &mut window,
                        false,
                        false,
                        &mut ChatPerfStats::default(),
                        &mut actions,
                    );
                });
            },
        );
        app.detached_chats.open.insert(index, window);
        app.apply_chat_window_actions(id, actions, ctx);
        output
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn shortcut(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers,
            })
            .collect()
    }

    #[test]
    fn detached_close_shortcut_closes_only_its_window_and_obeys_rebinding() {
        for rebound in [false, true] {
            let (mut app, mut commands) = app();
            let first = open(&mut app, false);
            let second = open(&mut app, false);
            app.chat_input_buf = "main draft".into();
            app.detached_chats.open[0].pane.input_buf = "window draft".into();
            if rebound {
                app.hotkey_bindings.set(
                    crust_core::HotkeyAction::CloseChannel,
                    crust_core::KeyBinding::new("F9"),
                );
            }
            let ctx = Context::default();
            for _ in 0..3 {
                surface_frame(&mut app, &ctx, first, Vec::new(), egui::vec2(480.0, 400.0));
            }
            if rebound {
                surface_frame(
                    &mut app,
                    &ctx,
                    first,
                    shortcut(egui::Key::W, egui::Modifiers::CTRL),
                    egui::vec2(480.0, 400.0),
                );
                assert_eq!(app.detached_chats.open.len(), 2);
                // Once rebound, Ctrl+W returns to the editor's delete-word action.
                assert_eq!(app.detached_chats.open[0].pane.input_buf, "window ");
            }
            let events = if rebound {
                shortcut(egui::Key::F9, egui::Modifiers::NONE)
            } else {
                shortcut(egui::Key::W, egui::Modifiers::CTRL)
            };
            surface_frame(&mut app, &ctx, first, events, egui::vec2(480.0, 400.0));
            assert_eq!(app.detached_chats.open.len(), 1);
            assert_eq!(app.detached_chats.open[0].pane.id, second);
            assert_eq!(app.detached_chats.closed[0].pane.id, first);
            assert_eq!(
                app.detached_chats.closed[0].pane.input_buf,
                if rebound { "window " } else { "window draft" }
            );
            assert_eq!(app.chat_input_buf, "main draft");
            assert!(app.state.channel_order.contains(&ChannelId::new("room")));
            assert!(!std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, AppCommand::LeaveChannel { .. })));
        }
    }

    #[test]
    fn detached_shortcuts_keep_local_search_and_focus_main_dialogs() {
        let (mut app, _) = app();
        let id = open(&mut app, false);
        app.state.join_channel(ChannelId::new("main"));
        app.state.active_channel = Some(ChannelId::new("main"));
        let ctx = Context::default();
        app.egui_ctx = Some(ctx.clone());
        for _ in 0..3 {
            surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
        }
        surface_frame(
            &mut app,
            &ctx,
            id,
            shortcut(egui::Key::F, egui::Modifiers::CTRL),
            egui::vec2(480.0, 400.0),
        );
        assert!(app.detached_chats.open[0].pane.search.open);
        assert!(!app
            .message_search
            .get(&ChannelId::new("main"))
            .is_some_and(|search| search.open));
        for (key, modifiers) in [
            (egui::Key::F, egui::Modifiers::CTRL | egui::Modifiers::SHIFT),
            (egui::Key::K, egui::Modifiers::CTRL),
            (egui::Key::Comma, egui::Modifiers::CTRL),
        ] {
            let output = surface_frame(
                &mut app,
                &ctx,
                id,
                shortcut(key, modifiers),
                egui::vec2(480.0, 400.0),
            );
            assert!(output.viewport_output[&ViewportId::ROOT]
                .commands
                .iter()
                .any(|command| matches!(command, ViewportCommand::Focus)));
        }
        assert!(app.global_search.open);
        assert!(app.quick_switch.open);
        assert!(app.settings_open);
        assert_eq!(app.state.active_channel, Some(ChannelId::new("main")));
        app.settings_open = false;
        app.hotkey_bindings.set(
            crust_core::HotkeyAction::OpenSettings,
            crust_core::KeyBinding::new("F10"),
        );
        surface_frame(
            &mut app,
            &ctx,
            id,
            shortcut(egui::Key::Comma, egui::Modifiers::CTRL),
            egui::vec2(480.0, 400.0),
        );
        assert!(!app.settings_open);
        surface_frame(
            &mut app,
            &ctx,
            id,
            shortcut(egui::Key::F10, egui::Modifiers::NONE),
            egui::vec2(480.0, 400.0),
        );
        assert!(app.settings_open);
    }

    #[test]
    fn detached_close_keeps_text_typed_before_the_shortcut_in_the_same_input_batch() {
        let (mut app, _) = app();
        let id = open(&mut app, false);
        let ctx = Context::default();
        for _ in 0..3 {
            surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
        }
        let mut events = vec![egui::Event::Text("draft before close".into())];
        events.extend(shortcut(egui::Key::W, egui::Modifiers::CTRL));
        surface_frame(&mut app, &ctx, id, events, egui::vec2(480.0, 400.0));
        assert_eq!(
            app.detached_chats.open[0].pane.input_buf,
            "draft before close"
        );
        surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
        assert!(app.detached_chats.open.is_empty());
        assert_eq!(
            app.detached_chats.closed[0].pane.input_buf,
            "draft before close"
        );
    }

    #[test]
    fn main_close_shortcut_closes_focused_split_before_its_channel() {
        let (mut app, mut commands) = app();
        let ctx = Context::default();
        app.split_active_pane(SplitAxis::Horizontal, &ctx);
        assert_eq!(app.split_panes.panes.len(), 2);
        app.split_panes.panes[app.split_panes.focused].input_buf = "saved split draft".into();
        let _ = ctx.run(
            egui::RawInput {
                events: shortcut(egui::Key::W, egui::Modifiers::CTRL),
                ..Default::default()
            },
            |ctx| app.handle_channel_shortcuts(ctx),
        );
        assert!(app.split_panes.panes.is_empty());
        assert_eq!(
            app.split_panes.closed.back().unwrap().pane.input_buf,
            "saved split draft"
        );
        assert!(app.state.channel_order.contains(&ChannelId::new("room")));
        assert!(!std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, AppCommand::LeaveChannel { .. })));
        let _ = ctx.run(
            egui::RawInput {
                events: shortcut(egui::Key::W, egui::Modifiers::CTRL),
                ..Default::default()
            },
            |ctx| app.handle_channel_shortcuts(ctx),
        );
        assert!(!app.state.channel_order.contains(&ChannelId::new("room")));
        assert!(std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, AppCommand::LeaveChannel { .. })));
    }

    #[test]
    fn detached_composer_typing_and_send_preserve_other_views_and_reply_targets() {
        let (mut app, mut commands) = app();
        let first = open(&mut app, false);
        let second = open(&mut app, false);
        let channel = ChannelId::new("room");
        app.chat_input_buf = "main draft".into();
        app.pending_reply = Some(PendingReply {
            channel: channel.clone(),
            info: ReplyInfo {
                parent_msg_id: "main-parent".into(),
                ..Default::default()
            },
        });
        app.detached_chats.open[0].pane.input_buf = "first window draft".into();
        app.detached_chats.open[1].reply = Some(ReplyInfo {
            parent_msg_id: "second-parent".into(),
            ..Default::default()
        });
        let target = app.detached_chats.open[1].target();
        let ctx = Context::default();
        for _ in 0..3 {
            surface_frame(&mut app, &ctx, second, Vec::new(), egui::vec2(480.0, 400.0));
        }
        surface_frame(
            &mut app,
            &ctx,
            second,
            vec![egui::Event::Text("second reply".into())],
            egui::vec2(480.0, 400.0),
        );
        surface_frame(
            &mut app,
            &ctx,
            second,
            vec![
                key(egui::Key::Enter),
                egui::Event::Text("next draft".into()),
            ],
            egui::vec2(480.0, 400.0),
        );
        surface_frame(&mut app, &ctx, second, Vec::new(), egui::vec2(480.0, 400.0));
        let sends: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok())
            .filter_map(|command| {
                if let AppCommand::SendMessage {
                    text,
                    reply_to_msg_id,
                    composer,
                    ..
                } = command
                {
                    Some((text, reply_to_msg_id, composer))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            sends,
            vec![(
                "second reply".into(),
                Some("second-parent".into()),
                Some(target)
            )]
        );
        assert_eq!(app.detached_chats.open[0].pane.id, first);
        assert_eq!(
            app.detached_chats.open[0].pane.input_buf,
            "first window draft"
        );
        assert_eq!(app.detached_chats.open[1].pane.input_buf, "next draft");
        assert!(app.detached_chats.open[1].reply.is_none());
        assert_eq!(app.chat_input_buf, "main draft");
        assert_eq!(
            app.pending_reply.as_ref().unwrap().info.parent_msg_id,
            "main-parent"
        );
    }

    #[test]
    fn detached_uploads_and_rejections_follow_closed_and_reopened_composers() {
        let (mut app, _) = app();
        let id = open(&mut app, false);
        open(&mut app, false);
        let channel = ChannelId::new("room");
        let target = app.detached_chats.open[0].target();
        app.detached_chats.open[0].pane.search.keyword = "first search".into();
        app.detached_chats.open[0].settings.position = Some([-1200.0, 80.0]);
        app.detached_chats.open[1].pane.input_buf = "second window".into();
        app.close_detached_chat(id, &Context::default());
        assert!(app.append_upload_to_composer(
            &channel,
            Some(target.clone()),
            "https://example.test/image.png"
        ));
        assert_eq!(app.detached_chats.open[0].pane.input_buf, "second window");
        app.reopen_detached_chat(id);
        let window = app.detached_chats.open.last_mut().unwrap();
        assert_eq!(window.pane.id, id);
        assert_eq!(window.pane.input_buf, "https://example.test/image.png ");
        assert_eq!(window.pane.search.keyword, "first search");
        assert_eq!(window.settings.position, Some([-1200.0, 80.0]));
        window.pane.input_buf.clear();
        let rejection = UnsentMessage {
            channel,
            text: "restore reply".into(),
            reply_to_msg_id: Some("original".into()),
            reply: None,
            composer: Some(target),
            reason: "Test rejection".into(),
        };
        assert!(app.restore_targeted_unsent(&rejection, false));
        assert_eq!(
            app.detached_chats
                .open
                .last()
                .unwrap()
                .reply
                .as_ref()
                .unwrap()
                .parent_msg_id,
            "original"
        );
        assert_eq!(app.detached_chats.open[0].pane.input_buf, "second window");
    }

    #[test]
    fn account_change_preserves_old_window_draft_without_delivering_old_uploads() {
        let (mut app, _) = app();
        open(&mut app, false);
        app.detached_chats.open[0].pane.input_buf = "alice draft".into();
        let target = app.detached_chats.open[0].target();
        app.state.auth.user_id = Some("bob-id".into());
        let ctx = Context::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |ctx| {
                app.show_detached_chats(ctx, &mut ChatPerfStats::default());
            },
        );
        assert!(app.detached_chats.open[0].pane.input_buf.is_empty());
        assert_eq!(
            app.unsent_messages
                .front_for_account(Some("alice-id"))
                .unwrap()
                .text,
            "alice draft"
        );
        assert!(app
            .unsent_messages
            .front_for_account(Some("bob-id"))
            .is_none());
        assert!(!app.append_upload_to_composer(
            &ChannelId::new("room"),
            Some(target),
            "https://example.test/old.png"
        ));
        assert!(app.detached_chats.open[0].pane.input_buf.is_empty());
    }

    #[test]
    fn late_upload_cannot_reach_a_closed_window_owned_by_another_account() {
        let (mut app, _) = app();
        let id = open(&mut app, false);
        let alice = app.detached_chats.open[0].target();
        app.detached_chats.open[0].account_id = Some("bob-id".into());
        app.detached_chats.open[0].pane.input_buf = "bob draft".into();
        app.state.auth.user_id = Some("bob-id".into());
        app.close_detached_chat(id, &Context::default());
        app.state.auth.user_id = Some("alice-id".into());
        assert!(!app.append_upload_to_composer(
            &ChannelId::new("room"),
            Some(alice),
            "https://example.test/alice.png"
        ));
        assert_eq!(app.detached_chats.closed[0].pane.input_buf, "bob draft");
        let recovered = app
            .unsent_messages
            .front_for_account(Some("alice-id"))
            .unwrap();
        assert_eq!(recovered.text, "https://example.test/alice.png ");
    }

    #[test]
    fn reopening_a_closed_sibling_keeps_a_restored_channel_tab_visible() {
        let (mut app, _) = app();
        let first = open(&mut app, false);
        let second = open(&mut app, false);
        let channel = ChannelId::new("room");
        app.close_channel_tab(channel.clone(), &Context::default());
        app.close_detached_chat(first, &Context::default());
        app.restore_detached_channel_tab(second);
        app.reopen_detached_chat(first);
        app.reconcile_detached_channel_tabs();
        assert!(app.state.channel_order.contains(&channel));
        assert!(app
            .detached_chats
            .settings()
            .iter()
            .all(|window| window.channel_tab_visible));
    }

    fn emote(code: &str) -> EmoteCatalogEntry {
        EmoteCatalogEntry {
            id: code.into(),
            code: code.into(),
            provider: "7tv".into(),
            url: format!("https://example.test/{code}.png"),
            url_hd: None,
            zero_width: false,
            scope: "channel".into(),
            channel: Some(ChannelId::new("room")),
            user_id: None,
        }
    }

    #[test]
    fn catalog_changes_invalidate_completions_in_open_and_closed_chat_windows() {
        for close in [false, true] {
            let (mut app, _) = app();
            let id = open(&mut app, false);
            let ctx = Context::default();
            app.apply_event(
                AppEvent::EmoteCatalogUpdated {
                    emotes: vec![emote("SharedAlpha"), emote("SharedBravo")],
                },
                &ctx,
            );
            for _ in 0..3 {
                surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
            }
            surface_frame(
                &mut app,
                &ctx,
                id,
                vec![egui::Event::Text("Sha".into())],
                egui::vec2(480.0, 400.0),
            );
            let release = egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: Some(egui::Key::Tab),
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            surface_frame(
                &mut app,
                &ctx,
                id,
                vec![key(egui::Key::Tab), release.clone()],
                egui::vec2(480.0, 400.0),
            );
            assert!(
                app.detached_chats.open[0]
                    .pane
                    .input_buf
                    .starts_with("SharedAlpha"),
                "completion={:?}",
                app.detached_chats.open[0].pane.input_buf
            );
            if close {
                app.close_detached_chat(id, &ctx);
            }
            app.apply_event(
                AppEvent::EmoteCatalogUpdated {
                    emotes: vec![emote("SharedDelta"), emote("SharedGamma")],
                },
                &ctx,
            );
            if close {
                app.reopen_detached_chat(id);
            }
            surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
            surface_frame(
                &mut app,
                &ctx,
                id,
                vec![key(egui::Key::Tab), release],
                egui::vec2(480.0, 400.0),
            );
            assert!(
                !app.detached_chats.open[0]
                    .pane
                    .input_buf
                    .contains("SharedBravo"),
                "removed emote survived catalog invalidation, closed={close}"
            );
        }
    }

    #[test]
    fn detached_only_channel_history_receives_catalog_replacements() {
        let (mut app, _) = app();
        open(&mut app, false);
        let channel = ChannelId::new("room");
        app.state
            .channels
            .get_mut(&channel)
            .unwrap()
            .push_message(ChatMessage {
                id: MessageId(11),
                server_id: Some("detached-history".into()),
                timestamp: Utc::now(),
                channel: channel.clone(),
                sender: crust_core::model::Sender {
                    user_id: crust_core::UserId("viewer".into()),
                    login: "viewer".into(),
                    display_name: "Viewer".into(),
                    original_display_name: None,
                    color: None,
                    name_paint: None,
                    badges: Vec::new(),
                },
                original_text: None,
                raw_text: "SharedAlpha".into(),
                spans: Default::default(),
                twitch_emotes: Vec::new(),
                flags: Default::default(),
                reply: None,
                msg_kind: MsgKind::Chat,
                shared: None,
            });
        let ctx = Context::default();
        app.close_channel_tab(channel.clone(), &ctx);
        app.apply_event(
            AppEvent::EmoteCatalogUpdated {
                emotes: vec![emote("SharedAlpha")],
            },
            &ctx,
        );
        app.refresh_emote_messages(&ctx);
        assert!(app.state.channels[&channel].messages[0]
            .spans
            .iter()
            .any(|span| matches!(span, Span::Emote { .. })));
        app.apply_event(AppEvent::EmoteCatalogUpdated { emotes: Vec::new() }, &ctx);
        app.refresh_emote_messages(&ctx);
        assert!(!app.state.channels[&channel].messages[0]
            .spans
            .iter()
            .any(|span| matches!(span, Span::Emote { .. })));
    }

    #[test]
    fn detached_view_keeps_main_navigation_and_background_unread_state_independent() {
        let (mut app, _) = app();
        let id = open(&mut app, false);
        let channel = ChannelId::new("room");
        let message = ChatMessage {
            id: MessageId(42),
            server_id: Some("window-message".into()),
            timestamp: Utc::now(),
            channel: channel.clone(),
            sender: crust_core::model::Sender {
                user_id: crust_core::UserId("viewer".into()),
                login: "viewer".into(),
                display_name: "Viewer".into(),
                original_display_name: None,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            original_text: None,
            raw_text: "Retained message".into(),
            spans: Default::default(),
            twitch_emotes: Vec::new(),
            flags: Default::default(),
            reply: None,
            msg_kind: MsgKind::Chat,
            shared: None,
        };
        app.state
            .channels
            .get_mut(&channel)
            .unwrap()
            .push_message(message);
        let ctx = Context::default();
        app.egui_ctx = Some(ctx.clone());
        for _ in 0..3 {
            surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
        }
        let state = app.state.channels.get_mut(&channel).unwrap();
        state.unread_count = 5;
        state.unread_mentions = 2;
        let snap = egui::Id::new("ml_force_snap").with(channel.as_str());
        ctx.data_mut(|data| data.insert_temp(snap, true));
        surface_frame_with_focus(
            &mut app,
            &ctx,
            id,
            Vec::new(),
            egui::vec2(480.0, 400.0),
            false,
        );
        assert_eq!(app.state.channels[&channel].unread_count, 5);
        assert_eq!(app.state.channels[&channel].unread_mentions, 2);
        assert_eq!(ctx.data(|data| data.get_temp::<bool>(snap)), Some(true));
        surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(480.0, 400.0));
        assert_eq!(app.state.channels[&channel].unread_count, 0);
        assert_eq!(app.state.channels[&channel].unread_mentions, 0);
        app.close_channel_tab(channel.clone(), &ctx);
        assert!(!app.state.channel_order.contains(&channel));
        assert!(app.jump_to_message(channel.clone(), MessageId(42)));
        app.reconcile_detached_channel_tabs();
        assert!(app.state.channel_order.contains(&channel));
        assert_eq!(app.state.active_channel, Some(channel.clone()));
        surface_frame_with_focus(
            &mut app,
            &ctx,
            id,
            Vec::new(),
            egui::vec2(480.0, 400.0),
            false,
        );
        assert_eq!(
            app.pending_scroll_to_message.get(&channel),
            Some(&MessageId(42))
        );
        assert_eq!(ctx.data(|data| data.get_temp::<bool>(snap)), Some(true));
    }

    #[test]
    fn detached_channel_survives_closing_its_main_tab_until_the_last_window_closes() {
        let (mut app, mut commands) = app();
        let first = open(&mut app, false);
        let second = open(&mut app, true);
        let channel = ChannelId::new("room");
        let ctx = Context::default();
        app.close_channel_tab(channel.clone(), &ctx);
        assert!(app.state.channels.contains_key(&channel));
        assert!(!app.state.channel_order.contains(&channel));
        assert!(app
            .detached_chats
            .settings()
            .iter()
            .all(|window| !window.channel_tab_visible));
        assert!(!std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, AppCommand::LeaveChannel { .. })));
        app.close_detached_chat(first, &ctx);
        assert!(app.state.channels.contains_key(&channel));
        app.close_detached_chat(second, &ctx);
        assert!(!app.state.channels.contains_key(&channel));
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command, AppCommand::LeaveChannel { channel: left } if left == channel)));
        app.reopen_detached_chat(second);
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command, AppCommand::JoinChannel { channel: joined } if joined == channel)));
    }

    #[test]
    fn detached_tabs_and_window_settings_restore_without_restoring_private_drafts() {
        let (mut app, _) = app();
        let id = open(&mut app, true);
        app.detached_chats.open[0].pane.input_buf = "session-only draft".into();
        app.detached_chats.open[0].pane.search.keyword = "saved query".into();
        app.detached_chats.open[0].pane.filters.selected = Some(Vec::new());
        app.detached_chats.open[0].settings.size = [640.0, 480.0];
        app.detached_chats.open[0].settings.click_through = true;
        app.retain_detached_channel(&ChannelId::new("room"));
        let saved = app.detached_chats.settings();
        let (mut restored, _) = self::app();
        restored
            .detached_chats
            .restore(&saved, &[], Some("alice-id".into()));
        restored.reconcile_detached_channel_tabs();
        assert_eq!(restored.detached_chats.settings(), saved);
        assert!(restored.detached_chats.open[0].pane.input_buf.is_empty());
        assert_ne!(restored.detached_chats.open[0].pane.id, id);
        assert!(!restored
            .state
            .channel_order
            .contains(&ChannelId::new("room")));
        let restored_id = restored.detached_chats.open[0].pane.id;
        restored.restore_detached_channel_tab(restored_id);
        assert_eq!(restored.state.active_channel, Some(ChannelId::new("room")));
        assert!(restored
            .state
            .channel_order
            .contains(&ChannelId::new("room")));
        assert!(restored.detached_chats.open[0].settings.channel_tab_visible);
    }

    #[test]
    fn shutdown_saves_window_edits_without_replacing_a_main_layout_that_is_still_joining() {
        let (mut app, _) = app();
        let window = DetachedChatSettings {
            channel: "room".into(),
            ..Default::default()
        };
        app.detached_chats
            .restore(&[window.clone()], &[], Some("alice-id".into()));
        app.last_saved_detached_chats = vec![window];
        app.layout_restore_received = true;
        app.pending_restore_channel = Some("delayed".into());
        app.last_saved_split_channel = "delayed".into();
        app.last_saved_split_panes = vec![("delayed".into(), 0.4), ("room".into(), 0.6)];
        app.last_saved_split_focused = 1;
        app.detached_chats.open[0].pane.search.keyword = "last edit".into();
        app.detached_chats.open[0].settings.size = [720.0, 540.0];
        let state = Arc::new(Mutex::new(None));
        app.set_layout_shutdown_state(Arc::clone(&state));
        drop(app);
        let snapshot = state.lock().unwrap().take().unwrap();
        assert_eq!(snapshot.active_channel, "delayed");
        assert_eq!(
            snapshot.panes,
            [("delayed".into(), 0.4), ("room".into(), 0.6)]
        );
        assert_eq!(snapshot.detached_chats[0].view.search.keyword, "last edit");
        assert_eq!(snapshot.detached_chats[0].size, [720.0, 540.0]);
    }

    #[test]
    fn detached_search_and_composer_stay_usable_in_small_views() {
        for overlay in [false, true] {
            for width in [240.0, 320.0, 640.0] {
                let (mut app, _) = app();
                let id = open(&mut app, overlay);
                let ctx = Context::default();
                for _ in 0..3 {
                    surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(width, 220.0));
                }
                surface_frame(
                    &mut app,
                    &ctx,
                    id,
                    vec![egui::Event::Text("visible draft".into())],
                    egui::vec2(width, 220.0),
                );
                assert_eq!(
                    app.detached_chats.open[0].pane.input_buf, "visible draft",
                    "overlay={overlay}, width={width}"
                );
                let output =
                    surface_frame(&mut app, &ctx, id, Vec::new(), egui::vec2(width, 220.0));
                let visible = output.shapes.iter().any(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        text.galley.text() == "visible draft"
                            && shape.clip_rect.contains_rect(egui::Rect::from_min_size(
                                text.pos,
                                text.galley.size(),
                            ))
                    } else {
                        false
                    }
                });
                assert!(
                    visible,
                    "composer clipped: overlay={overlay}, width={width}"
                );
            }
        }
    }

    fn manager_frame(
        app: &mut CrustApp,
        ctx: &Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.set_embed_viewports(false);
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                for window in &app.detached_chats.open {
                    ctx.show_viewport_deferred(
                        window.viewport_id(),
                        ViewportBuilder::default(),
                        |_, _| {},
                    );
                }
                app.show_chat_window_manager(ctx);
            },
        )
    }

    fn visible_text(output: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            let egui::Shape::Text(text) = &shape.shape else {
                return None;
            };
            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
            (text.galley.text() == label && shape.clip_rect.contains_rect(rect)).then_some(rect)
        })
    }

    #[test]
    fn chat_window_manager_keeps_recovery_controls_reachable_at_minimum_size() {
        for scale in [1.0, 1.5] {
            let (mut app, _) = app();
            open(&mut app, true);
            app.detached_chats.open[0].settings.click_through = true;
            app.detached_chats.manager_open = true;
            let ctx = Context::default();
            ctx.style_mut(|style| {
                for font in style.text_styles.values_mut() {
                    font.size *= scale;
                }
            });
            let size = egui::vec2(220.0, 200.0);
            let mut seen = HashSet::new();
            let wanted = ["Interact", "Reset position", "Close window", "Background"];
            for frame in 0..80 {
                let events = if frame < 3 {
                    Vec::new()
                } else {
                    vec![
                        egui::Event::PointerMoved(egui::pos2(110.0, 120.0)),
                        egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(0.0, -12.0),
                            modifiers: egui::Modifiers::NONE,
                        },
                    ]
                };
                let output = manager_frame(&mut app, &ctx, size, events);
                for label in wanted {
                    if let Some(rect) = visible_text(&output, label) {
                        assert!(
                            egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(rect),
                            "{label} escaped viewport at scale {scale}"
                        );
                        seen.insert(label);
                    }
                }
                if seen.len() == wanted.len() {
                    break;
                }
            }
            assert_eq!(
                seen.len(),
                wanted.len(),
                "controls missing at scale {scale}: {:?}",
                wanted
                    .into_iter()
                    .filter(|label| !seen.contains(label))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn reset_position_recovers_an_offscreen_click_through_overlay() {
        let (mut app, _) = app();
        open(&mut app, true);
        app.detached_chats.open[0].settings.position = Some([-999_000.0, 500_000.0]);
        app.detached_chats.open[0].settings.click_through = true;
        app.detached_chats.open[0].settings.maximized = true;
        app.detached_chats.manager_open = true;
        let ctx = Context::default();
        let size = egui::vec2(800.0, 600.0);
        let mut output = manager_frame(&mut app, &ctx, size, Vec::new());
        for _ in 0..3 {
            output = manager_frame(&mut app, &ctx, size, Vec::new());
        }
        let position = visible_text(&output, "Reset position").unwrap().center();
        manager_frame(
            &mut app,
            &ctx,
            size,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let output = manager_frame(
            &mut app,
            &ctx,
            size,
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let window = &app.detached_chats.open[0];
        assert_eq!(window.settings.position, Some([32.0, 32.0]));
        assert_eq!(window.settings.size, [480.0, 600.0]);
        assert!(!window.settings.click_through);
        assert!(!window.settings.maximized);
        let commands = &output.viewport_output[&window.viewport_id()].commands;
        assert!(commands
            .iter()
            .any(|command| matches!(command, ViewportCommand::MousePassthrough(false))));
        assert!(commands.iter().any(|command| matches!(command, ViewportCommand::OuterPosition(position) if *position == egui::pos2(32.0, 32.0))));
    }
}
