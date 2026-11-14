use crate::image_cache::UiImageCache;
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;

use egui::{Color32, Id, Key, LayerId, Order, Pos2, RichText, Stroke, Ui, Vec2};

use crate::commands::{extract_slash_query, slash_command_matches_ranked, SlashCommandSuggestion};
use crate::theme as t;
use crust_core::model::{ChannelId, EmoteCatalogEntry, ReplyInfo, IRC_SERVER_CONTROL_CHANNEL};

const AUTOCOMPLETE_MAX: usize = 10;
const AUTOCOMPLETE_EMOTE_SIZE: f32 = 20.0;
const SLASH_AUTOCOMPLETE_MAX: usize = 10;
const SLASH_AUTOCOMPLETE_MATCH_LIMIT: usize = usize::MAX;
const USERNAME_AUTOCOMPLETE_MAX: usize = 10;
/// Maximum number of Tab-completion matches to cycle through.
const TAB_COMPLETE_MAX: usize = 50;

// Persistent state stored in egui temp data

/// Spell-check context menu state preserved across frames while the menu is
/// open.
#[derive(Clone)]
struct SpellCtx {
    word: String,
    start: usize,
    end: usize,
    suggestions: Vec<String>,
}

/// Tab-completion state for bare-word emote completion (no `:` prefix).
#[derive(Clone, Default)]
struct TabState {
    /// Text before the word being completed.
    prefix: String,
    /// Text after the completed word, including its original whitespace.
    suffix: String,
    /// Matching emote codes.
    matches: Vec<String>,
    /// Current index in `matches`.
    index: usize,
    /// The buffer content we last set (to detect external changes).
    expected_buf: String,
    expected_cursor: usize,
}

impl TabState {
    fn apply(&mut self, buf: &mut String) -> usize {
        let word = &self.matches[self.index];
        let separator = if self.suffix.is_empty() { " " } else { "" };
        *buf = format!("{}{}{}{}", self.prefix, word, separator, self.suffix);
        let following_space = self
            .suffix
            .chars()
            .next()
            .filter(|ch| ch.is_whitespace())
            .map_or(separator.len(), char::len_utf8);
        let cursor = self.prefix.len() + word.len() + following_space;
        self.expected_buf.clone_from(buf);
        self.expected_cursor = buf[..cursor].chars().count();
        self.expected_cursor
    }
}

/// Message-history recall state (Up / Down arrows).
#[derive(Clone)]
struct HistState {
    /// -1 = normal input, 0 = most recent sent msg, 1 = second-most-recent, ...
    idx: i32,
    /// The original unsent input saved when history navigation started.
    saved_input: String,
    /// What we set the buffer to (to detect user edits -> reset).
    expected_buf: String,
}

impl Default for HistState {
    fn default() -> Self {
        Self {
            idx: -1,
            saved_input: String::new(),
            expected_buf: String::new(),
        }
    }
}

/// Chat input bar shown at the bottom of the message area.
pub struct ChatInput<'a> {
    /// Explicit identity keeps keyboard focus independent of the pane's layout.
    pub id: Option<Id>,
    /// The active channel to send messages to.
    pub channel: &'a ChannelId,
    /// Whether the user is authenticated (can send).
    pub logged_in: bool,
    /// The current username (for display).
    pub username: Option<&'a str>,
    /// Emotes available in this channel to the current account.
    pub emote_catalog: &'a [EmoteCatalogEntry],
    /// Loaded emote image bytes for rendering previews.
    pub emote_bytes: &'a UiImageCache,
    /// If set, show a dismissable "Replying to @name" banner above the input.
    pub pending_reply: Option<&'a ReplyInfo>,
    /// Previously-sent messages for Up/Down recall.
    pub message_history: &'a [String],
    /// Slash command usage counts used to improve autocomplete ranking.
    pub slash_usage_counts: &'a HashMap<String, u32>,
    /// All open channel tabs (used for `/join` channel suggestions on IRC).
    pub known_channels: &'a [ChannelId],
    /// Chatters currently in the channel (sorted, for `@` username autocomplete).
    pub chatters: &'a [String],
    /// Overflow handling mode for Twitch messages:
    /// `true` = Prevent, `false` = Highlight.
    pub prevent_overlong_twitch_messages: bool,
    /// Whether animated emotes should animate this frame.
    pub animate_emotes: bool,
    /// Whether dropped files belong to this pane.
    pub accept_dropped_files: bool,
}

/// A single image the user wants to upload (clipboard paste or file drop).
#[derive(Debug, Clone)]
pub struct PendingUpload {
    /// Encoded image bytes ready to POST.
    pub bytes: Vec<u8>,
    /// Extension without the dot: `"png"`, `"gif"`, `"jpeg"`.
    pub format: String,
    /// Original file path if the source was a dropped file.
    pub source_path: Option<std::path::PathBuf>,
}

/// Result from showing the chat input.
pub struct ChatInputResult {
    /// The original draft to submit, retained verbatim if dispatch fails.
    pub send: Option<String>,
    /// Whether the emote picker button was clicked.
    pub toggle_emote_picker: bool,
    /// User clicked ✕ to dismiss the pending reply.
    pub dismiss_reply: bool,
    /// Images queued for upload (clipboard or drag-drop).
    pub uploads: Vec<PendingUpload>,
    /// Word the user asked to add to their custom spellcheck dictionary via
    /// the chat-input context menu. Caller should forward to
    /// `AppCommand::AddWordToDictionary` so the change is persisted.
    pub add_to_dictionary: Option<String>,
}

impl<'a> ChatInput<'a> {
    /// Show the chat input. The `buf` is stored externally so it persists across frames.
    pub fn show(&self, ui: &mut Ui, buf: &mut String) -> ChatInputResult {
        let mut result = ChatInputResult {
            send: None,
            toggle_emote_picker: false,
            dismiss_reply: false,
            uploads: Vec::new(),
            add_to_dictionary: None,
        };

        let text_edit_id = self.id.unwrap_or_else(|| ui.id().with("chat_input"));
        let scope_id = text_edit_id.with("completion_channel");
        let channel_scope = Id::new(self.channel);
        let channel_changed = ui.data_mut(|data| {
            let changed = data.get_temp::<Id>(scope_id) != Some(channel_scope);
            if changed {
                data.insert_temp(scope_id, channel_scope);
            }
            changed
        });
        if channel_changed {
            invalidate_emote_completions(ui.ctx(), text_edit_id);
            ui.data_mut(|data| data.remove::<HistState>(text_edit_id.with("msg_history_state")));
        }
        let input_had_focus = ui.memory(|m| m.has_focus(text_edit_id));
        let mut retain_keyboard_focus = false;

        // Clipboard image paste (Ctrl/Cmd+V)
        // egui-winit 0.31's keyboard handler early-returns from a Ctrl+V
        // *press* when its internal `clipboard.get()` fails (image-only
        // clipboards trigger this path), so no `Event::Key` for V is ever
        // pushed on press AND no `Event::Paste` is pushed either. The V
        // RELEASE event, however, still goes through the normal push path
        // with `modifiers.ctrl` attachedwe use that as our paste trigger.
        // Also match `Event::Paste(_)` for the successful text-paste path
        // (where clipboard had both text and image).
        let paste_signal = if input_had_focus {
            let paste_state_id = Id::new("chat_image_paste_release");
            let mut skip_release = ui
                .ctx()
                .data(|d| d.get_temp(paste_state_id).unwrap_or(false));
            let requested = ui.input(|i| {
                image_paste_requested(
                    &i.events,
                    i.modifiers.command || i.modifiers.ctrl,
                    &mut skip_release,
                )
            });
            ui.ctx()
                .data_mut(|d| d.insert_temp(paste_state_id, skip_release));
            requested
        } else {
            false
        };
        if paste_signal {
            match try_clipboard_image_as_png() {
                Some(upload) => {
                    ui.input_mut(|i| {
                        i.events.retain(|e| {
                            !matches!(e, egui::Event::Paste(_))
                                && !matches!(
                                    e,
                                    egui::Event::Key {
                                        key: Key::V,
                                        modifiers,
                                        ..
                                    } if modifiers.command || modifiers.ctrl
                                )
                        });
                    });
                    tracing::info!("uploader: clipboard image captured via Ctrl+V");
                    result.uploads.push(upload);
                }
                None => {
                    tracing::debug!(
                        "uploader: paste signal saw no clipboard image, falling through"
                    );
                }
            }
        }

        // Drag-drop image file(s)
        let dropped = if self.accept_dropped_files {
            ui.input_mut(|i| std::mem::take(&mut i.raw.dropped_files))
        } else {
            Vec::new()
        };
        for f in &dropped {
            if let Some(upload) = dropped_file_to_upload(f) {
                result.uploads.push(upload);
            }
        }

        // Reply banner
        if let Some(rep) = self.pending_reply {
            egui::Frame::new()
                .fill(t::bg_raised())
                .inner_margin(egui::Margin::symmetric(12, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if ui
                                .small_button("✕")
                                .on_hover_text("Dismiss reply")
                                .clicked()
                            {
                                result.dismiss_reply = true;
                            }
                            let preview = format!(
                                "↩ Replying to @{}: {}",
                                rep.parent_display_name, rep.parent_msg_body
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&preview).font(t::small()).color(t::accent()),
                                )
                                .truncate(),
                            )
                            .on_hover_text(preview);
                        });
                    });
                });
        }

        egui::Frame::new()
            .fill(t::bg_surface())
            .inner_margin(t::INPUT_MARGIN)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;
                    let input_width = ui.available_width();
                    // Username label - hide at narrow widths
                    if input_width > 300.0 {
                        if let Some(name) = self.username {
                            ui.label(
                                RichText::new(format!("{name}:"))
                                    .color(t::accent())
                                    .strong()
                                    .font(t::small()),
                            );
                        }
                    }

                    // Queries stop at the caret, so editing an earlier word leaves the suffix intact.
                    let pre_cursor = cursor_byte_index(ui.ctx(), text_edit_id, buf);
                    let pre_query = &buf[..pre_cursor];
                    // Pre-check autocompletes before TextEdit consumes keys.
                    let pre_emote_matches = if input_had_focus {
                        find_autocomplete_matches(pre_query, self.emote_catalog)
                    } else {
                        Vec::new()
                    };
                    let pre_username_matches = if input_had_focus && pre_emote_matches.is_empty() {
                        find_username_matches(pre_query, self.chatters)
                    } else {
                        Vec::new()
                    };
                    let pre_slash_matches = if input_had_focus
                        && pre_emote_matches.is_empty()
                        && pre_username_matches.is_empty()
                    {
                        find_slash_matches(pre_query, self.slash_usage_counts)
                    } else {
                        Vec::new()
                    };
                    let pre_join_matches = if input_had_focus
                        && pre_emote_matches.is_empty()
                        && pre_username_matches.is_empty()
                        && pre_slash_matches.is_empty()
                    {
                        find_join_channel_matches(pre_query, self.channel, self.known_channels)
                    } else {
                        Vec::new()
                    };
                    let autocomplete_active = !pre_emote_matches.is_empty()
                        || !pre_username_matches.is_empty()
                        || !pre_slash_matches.is_empty()
                        || !pre_join_matches.is_empty();

                    // Consume shortcuts only for the focused composer, before
                    // TextEdit handles cursor movement or focus navigation.
                    let mut consumed_tab = false;
                    let mut reverse_tab = false;
                    let mut consumed_enter = false;
                    let mut consumed_up = false;
                    let mut consumed_down = false;

                    if input_had_focus {
                        ui.input_mut(|i| {
                            reverse_tab =
                                crate::app::consume_exact_key(i, egui::Modifiers::SHIFT, Key::Tab);
                            consumed_tab = reverse_tab
                                || crate::app::consume_exact_key(
                                    i,
                                    egui::Modifiers::NONE,
                                    Key::Tab,
                                );
                            consumed_up = crate::app::consume_exact_key(
                                i,
                                egui::Modifiers::NONE,
                                Key::ArrowUp,
                            );
                            consumed_down = crate::app::consume_exact_key(
                                i,
                                egui::Modifiers::NONE,
                                Key::ArrowDown,
                            );
                            if autocomplete_active {
                                consumed_enter = crate::app::consume_exact_key(
                                    i,
                                    egui::Modifiers::NONE,
                                    Key::Enter,
                                );
                            }
                        });
                    }
                    retain_keyboard_focus |=
                        consumed_tab || consumed_enter || consumed_up || consumed_down;

                    // Text input - reserve space for emote button + Send button + gaps
                    // At narrow widths, hide Send button and emote picker to maximise input
                    let show_send_btn = input_width > 250.0;
                    let show_emote_btn = input_width > 200.0;
                    let is_twitch_channel = !self.channel.is_irc() && !self.channel.is_kick();
                    let show_counter = is_twitch_channel && !buf.is_empty();
                    let counter_reserve = if show_counter { 70.0 } else { 0.0 };
                    let btn_square = (t::chat_font_size() + 16.0).max(t::bar_h());
                    let send_btn_w = (58.0 * t::font_scale()).max(58.0);
                    let reserve = if show_send_btn && show_emote_btn {
                        btn_square + send_btn_w + t::TOOLBAR_SPACING.x * 2.0 + counter_reserve
                    } else if show_emote_btn {
                        btn_square + t::TOOLBAR_SPACING.x + counter_reserve
                    } else {
                        counter_reserve
                    };
                    let hint_text = if self.logged_in {
                        if let Some(name) = self.username.filter(|n| !n.trim().is_empty()) {
                            format!("Send messages as {name}...")
                        } else {
                            "Send a message...".to_owned()
                        }
                    } else if let Some(name) = self.username.filter(|n| !n.trim().is_empty()) {
                        format!("Send messages as {name} after login (/help for local commands)")
                    } else {
                        "Log in to send messages (/help for local commands)".to_owned()
                    };
                    let text_width = (ui.available_width() - reserve).max(40.0);
                    // Input box grows with chat font so text never clips.
                    let input_h = (t::chat_font_size() + 16.0).max(t::bar_h());
                    let te_output = ui
                        .allocate_ui(egui::vec2(text_width, input_h), |ui| {
                            egui::TextEdit::singleline(buf)
                                .id(text_edit_id)
                                .hint_text(hint_text)
                                .text_color(t::text_primary())
                                .font(t::body())
                                .margin(egui::Margin::symmetric(6, 6))
                                .frame(true)
                                .min_size(egui::vec2(text_width, input_h))
                                // Prevent egui from cycling keyboard focus away on Tab;
                                // we handle Tab ourselves for autocomplete.
                                .lock_focus(true)
                                .show(ui)
                        })
                        .inner;
                    let resp = te_output.response;
                    let ac_id = text_edit_id.with("autocomplete_selection");
                    let tab_id = text_edit_id.with("tab_complete_state");
                    let hist_id = text_edit_id.with("msg_history_state");

                    // Paint red wavy underlines under misspelled words
                    if crate::spellcheck::is_enabled() {
                        let galley = &te_output.galley;
                        let galley_pos = te_output.galley_pos;
                        let clip = te_output.text_clip_rect;
                        let painter = ui.painter().with_clip_rect(clip);

                        for (word, char_start, _byte_start, char_end) in iter_words(buf) {
                            if crate::spellcheck::is_correct(word) {
                                continue;
                            }
                            // Get screen-space positions of word start and end
                            let r_start =
                                galley.pos_from_ccursor(egui::text::CCursor::new(char_start));
                            let r_end = galley.pos_from_ccursor(egui::text::CCursor::new(char_end));
                            let x0 = galley_pos.x + r_start.min.x;
                            let x1 = galley_pos.x + r_end.min.x;
                            let y = galley_pos.y + r_start.max.y + 1.0;

                            paint_wavy_underline(&painter, x0, x1, y, t::red());
                        }
                    }

                    // Read autocomplete selection index
                    let mut ac_sel: i32 = ui.ctx().data_mut(|d| d.get_temp(ac_id).unwrap_or(0i32));

                    // Recompute matches after TextEdit may have changed buf.
                    // Emote autocomplete takes priority, then @username, then slash, then /join.
                    let input_active = resp.has_focus() || input_had_focus;
                    let cursor_byte = cursor_byte_index(ui.ctx(), text_edit_id, buf);
                    let query = &buf[..cursor_byte];
                    let reuse_matches =
                        input_had_focus && !resp.changed() && cursor_byte == pre_cursor;
                    let matches = if !input_active {
                        Vec::new()
                    } else if reuse_matches {
                        pre_emote_matches
                    } else {
                        find_autocomplete_matches(query, self.emote_catalog)
                    };
                    let username_matches = if input_active && matches.is_empty() {
                        if reuse_matches {
                            pre_username_matches
                        } else {
                            find_username_matches(query, self.chatters)
                        }
                    } else {
                        Vec::new()
                    };
                    let slash_matches =
                        if input_active && matches.is_empty() && username_matches.is_empty() {
                            if reuse_matches {
                                pre_slash_matches
                            } else {
                                find_slash_matches(query, self.slash_usage_counts)
                            }
                        } else {
                            Vec::new()
                        };
                    let join_matches = if input_active
                        && matches.is_empty()
                        && username_matches.is_empty()
                        && slash_matches.is_empty()
                    {
                        if reuse_matches {
                            pre_join_matches
                        } else {
                            find_join_channel_matches(query, self.channel, self.known_channels)
                        }
                    } else {
                        Vec::new()
                    };

                    let mut accepted_emote: Option<String> = None;
                    let mut accepted_username: Option<String> = None;
                    let mut accepted_slash_cmd: Option<String> = None;
                    let mut accepted_join_channel: Option<String> = None;

                    if !matches.is_empty() {
                        // Colon-autocomplete active
                        let n = matches.len() as i32;
                        ac_sel = ac_sel.clamp(0, n - 1);

                        if consumed_up {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_down {
                            ac_sel = (ac_sel + 1).rem_euclid(n);
                        }
                        if consumed_tab && reverse_tab {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_tab || consumed_enter {
                            accepted_emote = Some(matches[ac_sel as usize].code.clone());
                        }
                        // Keep focus on the text field while cycling through the AC list.
                        if consumed_tab || consumed_up || consumed_down {
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    } else if !username_matches.is_empty() {
                        // @username autocomplete active
                        let n = username_matches.len() as i32;
                        ac_sel = ac_sel.clamp(0, n - 1);

                        if consumed_up {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_down {
                            ac_sel = (ac_sel + 1).rem_euclid(n);
                        }
                        if consumed_tab && reverse_tab {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_tab || consumed_enter {
                            accepted_username = Some(username_matches[ac_sel as usize].clone());
                        }
                        if consumed_tab || consumed_up || consumed_down {
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    } else if !slash_matches.is_empty() {
                        // Slash-command autocomplete active
                        let n = slash_matches.len() as i32;
                        ac_sel = ac_sel.clamp(0, n - 1);

                        if consumed_up {
                            ac_sel = (ac_sel - 1).max(0);
                        }
                        if consumed_down {
                            ac_sel = (ac_sel + 1).min(n - 1);
                        }
                        if consumed_tab && reverse_tab {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_tab || consumed_enter {
                            accepted_slash_cmd =
                                Some(slash_matches[ac_sel as usize].name.to_owned());
                        }
                        if consumed_tab || consumed_up || consumed_down {
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    } else if !join_matches.is_empty() {
                        // `/join` channel autocomplete active
                        let n = join_matches.len() as i32;
                        ac_sel = ac_sel.clamp(0, n - 1);

                        if consumed_up {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_down {
                            ac_sel = (ac_sel + 1).rem_euclid(n);
                        }
                        if consumed_tab && reverse_tab {
                            ac_sel = (ac_sel - 1).rem_euclid(n);
                        }
                        if consumed_tab || consumed_enter {
                            accepted_join_channel = Some(join_matches[ac_sel as usize].clone());
                        }
                        if consumed_tab || consumed_up || consumed_down {
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    } else {
                        ac_sel = 0;

                        // Bare-word Tab completion (emotes + usernames)
                        if consumed_tab {
                            let mut session: TabState = ui
                                .ctx()
                                .data(|data| data.get_temp(tab_id).unwrap_or_default());
                            let cursor = buf[..cursor_byte].chars().count();
                            let continuing = !session.matches.is_empty()
                                && session.expected_buf == *buf
                                && session.expected_cursor == cursor;
                            if continuing {
                                session.index = if reverse_tab {
                                    (session.index + session.matches.len() - 1)
                                        % session.matches.len()
                                } else {
                                    (session.index + 1) % session.matches.len()
                                };
                                let cursor = session.apply(buf);
                                move_cursor_to_end(ui.ctx(), text_edit_id, cursor);
                                ui.ctx().data_mut(|data| data.insert_temp(tab_id, session));
                            } else {
                                let range = completion_token_range(buf, cursor_byte);
                                let word = &buf[range.start..cursor_byte];
                                let choices = bare_completion_matches(
                                    word,
                                    self.emote_catalog,
                                    self.chatters,
                                );
                                if choices.is_empty() {
                                    ui.ctx().data_mut(|data| {
                                        data.insert_temp(tab_id, TabState::default())
                                    });
                                } else {
                                    let selected = if reverse_tab { choices.len() - 1 } else { 0 };
                                    apply_completion(
                                        ui.ctx(),
                                        text_edit_id,
                                        buf,
                                        cursor_byte,
                                        choices,
                                        selected,
                                        true,
                                    );
                                }
                            }
                            ui.ctx()
                                .memory_mut(|memory| memory.request_focus(text_edit_id));
                        } else {
                            // Any non-Tab keystroke invalidates the tab session
                            if consumed_up
                                || consumed_down
                                || resp.changed()
                                || cursor_byte != pre_cursor
                            {
                                ui.ctx()
                                    .data_mut(|d| d.insert_temp(tab_id, TabState::default()));
                            }
                        }

                        // Message history (Up / Down)
                        if (consumed_up || consumed_down) && !self.message_history.is_empty() {
                            let mut hs: HistState = ui
                                .ctx()
                                .data_mut(|d| d.get_temp(hist_id).unwrap_or_default());

                            // Detect user edits -> reset history position
                            if hs.idx >= 0 && *buf != hs.expected_buf {
                                hs.idx = -1;
                            }

                            let hlen = self.message_history.len() as i32;

                            if consumed_up {
                                if hs.idx == -1 {
                                    hs.saved_input = buf.clone();
                                    hs.idx = 0;
                                } else if hs.idx < hlen - 1 {
                                    hs.idx += 1;
                                }
                                let i = (hlen - 1 - hs.idx) as usize;
                                *buf = self.message_history[i].clone();
                            }
                            if consumed_down {
                                if hs.idx > 0 {
                                    hs.idx -= 1;
                                    let i = (hlen - 1 - hs.idx) as usize;
                                    *buf = self.message_history[i].clone();
                                } else if hs.idx == 0 {
                                    hs.idx = -1;
                                    *buf = hs.saved_input.clone();
                                }
                            }

                            hs.expected_buf = buf.clone();
                            ui.ctx().data_mut(|d| d.insert_temp(hist_id, hs));
                            move_cursor_to_end(ui.ctx(), text_edit_id, buf.chars().count());
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                    }

                    // Replace the :query token with the accepted emote
                    if let Some(ref code) = accepted_emote {
                        let choices: Vec<String> =
                            matches.iter().map(|entry| entry.code.clone()).collect();
                        let selected = choices
                            .iter()
                            .position(|candidate| candidate == code)
                            .unwrap_or(0);
                        apply_completion(
                            ui.ctx(),
                            text_edit_id,
                            buf,
                            cursor_byte,
                            choices,
                            selected,
                            consumed_tab,
                        );
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        ac_sel = 0;
                    }
                    if let Some(ref username) = accepted_username {
                        let choices: Vec<String> = username_matches
                            .iter()
                            .map(|name| format!("@{name}"))
                            .collect();
                        let selected = username_matches
                            .iter()
                            .position(|candidate| candidate == username)
                            .unwrap_or(0);
                        apply_completion(
                            ui.ctx(),
                            text_edit_id,
                            buf,
                            cursor_byte,
                            choices,
                            selected,
                            consumed_tab,
                        );
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        ac_sel = 0;
                    }
                    if let Some(ref command) = accepted_slash_cmd {
                        let choices: Vec<String> = slash_matches
                            .iter()
                            .map(|entry| format!("/{}", entry.name))
                            .collect();
                        let selected = slash_matches
                            .iter()
                            .position(|candidate| &candidate.name == command)
                            .unwrap_or(0);
                        apply_completion(
                            ui.ctx(),
                            text_edit_id,
                            buf,
                            cursor_byte,
                            choices,
                            selected,
                            consumed_tab,
                        );
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        ac_sel = 0;
                    }
                    if let Some(ref channel_name) = accepted_join_channel {
                        let selected = join_matches
                            .iter()
                            .position(|candidate| candidate == channel_name)
                            .unwrap_or(0);
                        apply_completion(
                            ui.ctx(),
                            text_edit_id,
                            buf,
                            cursor_byte,
                            join_matches.clone(),
                            selected,
                            consumed_tab,
                        );
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        ac_sel = 0;
                    }

                    ui.ctx().data_mut(|d| d.insert_temp(ac_id, ac_sel));

                    const TWITCH_MAX_CHARS: usize = 500;
                    let mut twitch_char_count = if is_twitch_channel {
                        buf.chars().count()
                    } else {
                        0
                    };
                    // Prevent mode: enforce Twitch's hard character cap in the editor.
                    if is_twitch_channel
                        && self.prevent_overlong_twitch_messages
                        && twitch_char_count > TWITCH_MAX_CHARS
                    {
                        truncate_to_max_chars(buf, TWITCH_MAX_CHARS);
                        twitch_char_count = buf.chars().count();
                        move_cursor_to_end(ui.ctx(), text_edit_id, buf.chars().count());
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                    }
                    let twitch_over_limit =
                        is_twitch_channel && twitch_char_count > TWITCH_MAX_CHARS;
                    let is_slash_input = buf.trim_start().starts_with('/');
                    let can_submit = !twitch_over_limit
                        && !buf.trim().is_empty()
                        && (self.logged_in || is_slash_input);

                    // Send on Enter (only fires when we did NOT consume it)
                    let enter_pressed =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

                    if enter_pressed {
                        resp.request_focus();
                        if can_submit {
                            result.send = Some(std::mem::take(buf));
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(hist_id, HistState::default()));
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(tab_id, TabState::default()));
                        }
                    }

                    // Emote picker button - hidden at very narrow widths
                    let btn_h = (t::chat_font_size() + 16.0).max(t::bar_h());
                    if show_emote_btn {
                        if ui
                            .add_sized(
                                [btn_h, btn_h],
                                egui::Button::new(RichText::new(":)").font(t::body())),
                            )
                            .on_hover_text("Emote picker")
                            .clicked()
                        {
                            result.toggle_emote_picker = true;
                        }
                    }

                    // Send button - hidden at very narrow widths
                    if show_send_btn {
                        let send_btn = ui.add_enabled(
                            can_submit,
                            egui::Button::new(RichText::new("Send").font(t::body()))
                                .min_size(egui::vec2((58.0 * t::font_scale()).max(58.0), btn_h)),
                        );
                        let send_btn = if twitch_over_limit && is_twitch_channel {
                            send_btn.on_hover_text("Twitch messages are limited to 500 characters")
                        } else if !self.logged_in && !is_slash_input && !buf.trim().is_empty() {
                            send_btn.on_hover_text("Anonymous mode can run slash commands only")
                        } else {
                            send_btn
                        };
                        if send_btn.clicked() && can_submit {
                            result.send = Some(std::mem::take(buf));
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(hist_id, HistState::default()));
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(tab_id, TabState::default()));
                            resp.request_focus();
                        }
                    }

                    // Character count - Twitch has a 500-char limit.
                    // Show only for Twitch channels when the user has typed something.
                    if !buf.is_empty() && is_twitch_channel {
                        let compact_counter = ui.available_width() < 110.0;
                        let color = if twitch_char_count > TWITCH_MAX_CHARS {
                            t::red()
                        } else if twitch_char_count > 400 {
                            t::yellow()
                        } else {
                            t::text_muted()
                        };
                        ui.label(
                            RichText::new(format!("{twitch_char_count}/{TWITCH_MAX_CHARS}"))
                                .font(t::tiny())
                                .color(color),
                        );
                        if twitch_over_limit
                            && !self.prevent_overlong_twitch_messages
                            && !compact_counter
                        {
                            let over_by = twitch_char_count - TWITCH_MAX_CHARS;
                            ui.label(
                                RichText::new(format!("Over by {over_by} chars"))
                                    .font(t::tiny())
                                    .color(t::red()),
                            );
                        }
                    }

                    if twitch_over_limit && !self.prevent_overlong_twitch_messages {
                        ui.painter().rect_stroke(
                            resp.rect.expand(1.0),
                            4.0,
                            Stroke::new(1.0_f32, t::red()),
                            egui::epaint::StrokeKind::Outside,
                        );
                    }

                    // Draw autocomplete popup above input
                    let show_popup = (!matches.is_empty()
                        || !username_matches.is_empty()
                        || !slash_matches.is_empty()
                        || !join_matches.is_empty())
                        && (resp.has_focus()
                            || accepted_emote.is_some()
                            || accepted_username.is_some()
                            || accepted_slash_cmd.is_some()
                            || accepted_join_channel.is_some());
                    if show_popup {
                        if !matches.is_empty() {
                            if let Some(clicked) =
                                self.show_autocomplete_popup(ui, &resp, &matches, ac_sel)
                            {
                                apply_completion(
                                    ui.ctx(),
                                    text_edit_id,
                                    buf,
                                    cursor_byte,
                                    vec![clicked],
                                    0,
                                    false,
                                );
                                ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                                ui.ctx().data_mut(|d| d.insert_temp(ac_id, 0i32));
                            }
                        } else if !username_matches.is_empty() {
                            if let Some(clicked) = self.show_username_autocomplete_popup(
                                ui,
                                &resp,
                                &username_matches,
                                ac_sel,
                            ) {
                                apply_completion(
                                    ui.ctx(),
                                    text_edit_id,
                                    buf,
                                    cursor_byte,
                                    vec![format!("@{clicked}")],
                                    0,
                                    false,
                                );
                                ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                                ui.ctx().data_mut(|d| d.insert_temp(ac_id, 0i32));
                            }
                        } else if let Some(clicked) =
                            self.show_slash_autocomplete_popup(ui, &resp, &slash_matches, ac_sel)
                        {
                            apply_completion(
                                ui.ctx(),
                                text_edit_id,
                                buf,
                                cursor_byte,
                                vec![format!("/{clicked}")],
                                0,
                                false,
                            );
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                            ui.ctx().data_mut(|d| d.insert_temp(ac_id, 0i32));
                        } else if let Some(clicked) =
                            self.show_join_autocomplete_popup(ui, &resp, &join_matches, ac_sel)
                        {
                            apply_completion(
                                ui.ctx(),
                                text_edit_id,
                                buf,
                                cursor_byte,
                                vec![clicked],
                                0,
                                false,
                            );
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                            ui.ctx().data_mut(|d| d.insert_temp(ac_id, 0i32));
                        }
                    }

                    // Spell-check context menu (right-click)
                    let sc_id = text_edit_id.with("spell_check_ctx");

                    // On right-click, determine the word at the cursor and
                    // compute suggestions if it is misspelled.
                    if resp.secondary_clicked() {
                        let cursor_idx = egui::TextEdit::load_state(ui.ctx(), text_edit_id)
                            .and_then(|s| s.cursor.char_range())
                            .map(|r| r.primary.index)
                            .unwrap_or_else(|| buf.chars().count());

                        let (word, byte_start, byte_end) =
                            crate::spellcheck::word_at_cursor(buf, cursor_idx);

                        if !word.is_empty() && !crate::spellcheck::is_correct(word) {
                            let suggs = crate::spellcheck::suggestions(word, 5);
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(
                                    sc_id,
                                    SpellCtx {
                                        word: word.to_owned(),
                                        start: byte_start,
                                        end: byte_end,
                                        suggestions: suggs,
                                    },
                                )
                            });
                        } else {
                            ui.ctx().data_mut(|d| d.remove::<SpellCtx>(sc_id));
                        }
                    }

                    let mut spell_replace: Option<(usize, usize, String)> = None;
                    let mut spell_add_word: Option<String> = None;
                    let mut context_send_now = false;
                    let mut context_cut_input = false;
                    let mut context_copy_selection = false;
                    let mut context_copy_input = false;
                    let mut context_paste_input = false;
                    let mut context_select_all = false;
                    let mut context_clear_input = false;
                    let mut context_insert_help = false;
                    let has_selection = selected_input_range(ui.ctx(), text_edit_id, buf).is_some();
                    resp.context_menu(|ui| {
                        if ui
                            .add_enabled(
                                has_selection,
                                egui::Button::new(RichText::new("Cut").font(t::small())),
                            )
                            .clicked()
                        {
                            context_cut_input = true;
                            ui.close_menu();
                        }

                        if ui
                            .add_enabled(
                                has_selection,
                                egui::Button::new(RichText::new("Copy").font(t::small())),
                            )
                            .clicked()
                        {
                            context_copy_selection = true;
                            ui.close_menu();
                        }

                        if ui.button(RichText::new("Paste").font(t::small())).clicked() {
                            // Prefer image paste when the clipboard holds one;
                            // fall back to egui's text paste otherwise.
                            match try_clipboard_image_as_png() {
                                Some(up) => {
                                    tracing::info!(
                                        "uploader: clipboard image captured via right-click paste"
                                    );
                                    result.uploads.push(up);
                                }
                                None => {
                                    context_paste_input = true;
                                }
                            }
                            ui.close_menu();
                        }

                        if ui
                            .add_enabled(
                                !buf.is_empty(),
                                egui::Button::new(RichText::new("Select all").font(t::small())),
                            )
                            .clicked()
                        {
                            context_select_all = true;
                            ui.close_menu();
                        }

                        ui.separator();

                        let send_now_resp = ui.add_enabled(
                            can_submit,
                            egui::Button::new(RichText::new("Send now").font(t::small())),
                        );
                        if send_now_resp.clicked() {
                            context_send_now = true;
                            ui.close_menu();
                        }

                        if ui
                            .add_enabled(
                                !buf.is_empty(),
                                egui::Button::new(RichText::new("Copy input").font(t::small())),
                            )
                            .clicked()
                        {
                            context_copy_input = true;
                            ui.close_menu();
                        }

                        if ui
                            .add_enabled(
                                !buf.is_empty(),
                                egui::Button::new(RichText::new("Clear input").font(t::small())),
                            )
                            .clicked()
                        {
                            context_clear_input = true;
                            ui.close_menu();
                        }

                        if ui
                            .button(RichText::new("Insert /help").font(t::small()))
                            .clicked()
                        {
                            context_insert_help = true;
                            ui.close_menu();
                        }

                        ui.separator();

                        let sc: Option<SpellCtx> = ui.ctx().data_mut(|d| d.get_temp(sc_id));
                        if let Some(sc) = sc {
                            let display_word = normalize_spell_menu_word(&sc.word);
                            ui.label(
                                RichText::new(format!("\"{}\" - misspelled", display_word))
                                    .color(t::red())
                                    .strong(),
                            );
                            ui.separator();
                            if sc.suggestions.is_empty() {
                                ui.label(RichText::new("No suggestions").weak().italics());
                            } else {
                                for sug in &sc.suggestions {
                                    if ui.button(sug).clicked() {
                                        spell_replace = Some((sc.start, sc.end, sug.clone()));
                                        ui.close_menu();
                                    }
                                }
                            }
                            ui.separator();
                            if ui
                                .button(
                                    RichText::new(format!("Add \"{display_word}\" to dictionary"))
                                        .font(t::small()),
                                )
                                .clicked()
                            {
                                spell_add_word = Some(sc.word.clone());
                                ui.ctx().data_mut(|d| d.remove::<SpellCtx>(sc_id));
                                ui.close_menu();
                            }
                        } else {
                            ui.label(RichText::new("Spelling OK ").color(t::green()));
                        }
                    });

                    if context_copy_input {
                        ui.ctx().copy_text(buf.clone());
                    }

                    if context_copy_selection || context_cut_input {
                        copy_or_cut_selection(ui.ctx(), text_edit_id, buf, context_cut_input);
                    }

                    if context_paste_input {
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                    }

                    if context_select_all {
                        select_all_text(ui.ctx(), text_edit_id, buf.chars().count());
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                    }

                    if context_send_now && can_submit {
                        result.send = Some(std::mem::take(buf));
                        ui.ctx()
                            .data_mut(|d| d.insert_temp(hist_id, HistState::default()));
                        ui.ctx()
                            .data_mut(|d| d.insert_temp(tab_id, TabState::default()));
                        resp.request_focus();
                    }

                    if context_clear_input {
                        buf.clear();
                        move_cursor_to_end(ui.ctx(), text_edit_id, 0);
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                    }

                    if context_insert_help {
                        *buf = "/help".to_owned();
                        move_cursor_to_end(ui.ctx(), text_edit_id, buf.chars().count());
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                    }

                    if let Some((start, end, replacement)) = spell_replace {
                        let new_len = buf[..start].chars().count() + replacement.chars().count();
                        buf.replace_range(start..end, &replacement);
                        move_cursor_to_end(ui.ctx(), text_edit_id, new_len);
                        ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                    }

                    if let Some(word) = spell_add_word {
                        // Update the in-memory user dictionary immediately so
                        // the red underline disappears this frame; the caller
                        // will also forward the word to the app runtime to
                        // persist it in `AppSettings::custom_spell_dict`.
                        if crate::spellcheck::add_to_user_dict(&word) {
                            tracing::info!(
                                target: "spellcheck",
                                "user added \"{word}\" to custom dictionary"
                            );
                        }
                        result.add_to_dictionary = Some(word);
                    }
                });
            });

        // request_focus resets egui's key filter. Reapply it after every child
        // control has registered so Tab completion cannot move focus to Send.
        ui.ctx().memory_mut(|memory| {
            if retain_keyboard_focus && !memory.has_focus(text_edit_id) {
                memory.request_focus(text_edit_id);
            }
            if memory.has_focus(text_edit_id) {
                memory.set_focus_lock_filter(
                    text_edit_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            }
        });

        result
    }

    fn show_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[&EmoteCatalogEntry],
        selected: i32,
    ) -> Option<String> {
        let input_rect = text_resp.rect;
        let popup_width = input_rect.width().min(350.0);
        let row_h = 28.0;
        let popup_h = (matches.len() as f32 * row_h).min(AUTOCOMPLETE_MAX as f32 * row_h) + 8.0;
        let popup_rect = egui::Rect::from_min_size(
            egui::pos2(input_rect.left(), input_rect.top() - popup_h - 4.0),
            egui::vec2(popup_width, popup_h),
        );

        let layer_id = LayerId::new(
            Order::Foreground,
            text_resp.id.with("emote_autocomplete_popup"),
        );
        let painter = ui.ctx().layer_painter(layer_id);

        // Background + border
        painter.rect_filled(popup_rect, 6.0, t::bg_raised());
        painter.rect_stroke(
            popup_rect,
            6.0,
            egui::Stroke::new(1.0_f32, t::border_subtle()),
            egui::epaint::StrokeKind::Outside,
        );

        // Interactive child UI on the foreground layer - force vertical layout
        let mut popup_ui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer_id)
                .max_rect(popup_rect.shrink(4.0))
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        popup_ui.set_clip_rect(popup_rect);

        let mut clicked_emote: Option<String> = None;
        let mut has_animated_preview = false;

        egui::ScrollArea::vertical()
            .id_salt(text_resp.id.with("emote_ac_scroll"))
            .auto_shrink([false; 2])
            .show(&mut popup_ui, |ui| {
                for (i, entry) in matches.iter().enumerate() {
                    let is_selected = i as i32 == selected;
                    let row_id = text_resp.id.with("ac_row").with(i);

                    let row_bg = if is_selected {
                        t::active_channel_bg()
                    } else {
                        Color32::TRANSPARENT
                    };

                    let frame_resp = egui::Frame::new()
                        .fill(row_bg)
                        .corner_radius(3.0)
                        .inner_margin(egui::Margin::symmetric(4, 2))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;

                                // Emote image preview
                                if let Some(&(w, h, ref raw)) =
                                    self.emote_bytes.get(entry.url.as_str())
                                {
                                    let animated = is_likely_animated_url(&entry.url);
                                    let should_animate = self.animate_emotes && is_selected;
                                    let (slot_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                        ),
                                        egui::Sense::hover(),
                                    );

                                    if animated && !should_animate {
                                        if let Some(tex) =
                                            self.emote_bytes.static_texture(ui.ctx(), &entry.url)
                                        {
                                            let size = fit_size(w, h, AUTOCOMPLETE_EMOTE_SIZE);
                                            let image_rect = egui::Rect::from_center_size(
                                                slot_rect.center(),
                                                size,
                                            );
                                            ui.painter().image(
                                                tex.id(),
                                                image_rect,
                                                egui::Rect::from_min_max(
                                                    egui::pos2(0.0, 0.0),
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                Color32::WHITE,
                                            );
                                        }
                                    } else {
                                        if animated && self.animate_emotes {
                                            has_animated_preview = true;
                                        }
                                        let size = fit_size(w, h, AUTOCOMPLETE_EMOTE_SIZE);
                                        let image_rect =
                                            egui::Rect::from_center_size(slot_rect.center(), size);
                                        let url_key = super::bytes_uri(&entry.url, raw.as_ref());
                                        ui.put(
                                            image_rect,
                                            egui::Image::from_bytes(
                                                url_key,
                                                egui::load::Bytes::Shared(raw.clone()),
                                            )
                                            .fit_to_exact_size(size),
                                        );
                                    }
                                } else {
                                    ui.allocate_exact_size(
                                        egui::vec2(
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                            AUTOCOMPLETE_EMOTE_SIZE,
                                        ),
                                        egui::Sense::hover(),
                                    );
                                }

                                let code_color = if is_selected {
                                    t::text_primary()
                                } else {
                                    t::text_secondary()
                                };
                                ui.label(
                                    RichText::new(&entry.code)
                                        .color(code_color)
                                        .font(t::small()),
                                );

                                // Provider tag (right-aligned)
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(provider_label(&entry.provider))
                                                .font(t::small())
                                                .color(t::text_muted()),
                                        );
                                    },
                                );
                            });
                        });

                    // Make the row clickable on the foreground layer
                    let row_rect = frame_resp.response.rect;
                    if is_selected {
                        ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                    }
                    let click_resp = ui.interact(row_rect, row_id, egui::Sense::click());

                    // If an animated emote row is hovered, render its animated preview.
                    if click_resp.hovered() {
                        if let Some(&(w, h, ref raw)) = self.emote_bytes.get(entry.url.as_str()) {
                            if self.animate_emotes && is_likely_animated_url(&entry.url) {
                                has_animated_preview = true;
                                let size = fit_size(w, h, AUTOCOMPLETE_EMOTE_SIZE);
                                let preview_rect = egui::Rect::from_min_size(
                                    egui::pos2(row_rect.left() + 4.0, row_rect.top() + 2.0),
                                    egui::vec2(AUTOCOMPLETE_EMOTE_SIZE, AUTOCOMPLETE_EMOTE_SIZE),
                                );
                                let image_rect =
                                    egui::Rect::from_center_size(preview_rect.center(), size);
                                ui.put(
                                    image_rect,
                                    egui::Image::from_bytes(
                                        super::bytes_uri(&entry.url, raw.as_ref()),
                                        egui::load::Bytes::Shared(raw.clone()),
                                    )
                                    .fit_to_exact_size(size),
                                );
                            }
                        }
                    }

                    if click_resp.clicked() {
                        clicked_emote = Some(entry.code.clone());
                    }
                }
            });

        if has_animated_preview {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }

        clicked_emote
    }

    fn show_slash_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[SlashCommandSuggestion],
        selected: i32,
    ) -> Option<String> {
        if matches.is_empty() {
            return None;
        }

        let selected = selected.clamp(0, matches.len() as i32 - 1);
        let visible_rows = SLASH_AUTOCOMPLETE_MAX.min(matches.len());
        let window_start = if matches.len() <= visible_rows {
            0
        } else {
            let half = (visible_rows / 2) as i32;
            let max_start = (matches.len() - visible_rows) as i32;
            (selected - half).clamp(0, max_start) as usize
        };
        let window_end = (window_start + visible_rows).min(matches.len());

        let input_rect = text_resp.rect;
        let popup_width = input_rect.width().min(460.0);
        let row_h = 36.0;
        let popup_h = visible_rows as f32 * row_h + 8.0;
        let popup_rect = egui::Rect::from_min_size(
            egui::pos2(input_rect.left(), input_rect.top() - popup_h - 4.0),
            egui::vec2(popup_width, popup_h),
        );

        let layer_id = LayerId::new(
            Order::Foreground,
            text_resp.id.with("slash_autocomplete_popup"),
        );
        let painter = ui.ctx().layer_painter(layer_id);
        painter.rect_filled(popup_rect, 6.0, t::bg_raised());
        painter.rect_stroke(
            popup_rect,
            6.0,
            egui::Stroke::new(1.0_f32, t::border_subtle()),
            egui::epaint::StrokeKind::Outside,
        );

        let mut popup_ui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer_id)
                .max_rect(popup_rect.shrink(4.0))
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        popup_ui.set_clip_rect(popup_rect);

        let mut clicked_cmd: Option<String> = None;
        for (offset, entry) in matches[window_start..window_end].iter().enumerate() {
            let idx = window_start + offset;
            let is_selected = idx as i32 == selected;
            let row_id = text_resp.id.with("slash_ac_row").with(idx);
            let row_bg = if is_selected {
                t::active_channel_bg()
            } else {
                Color32::TRANSPARENT
            };

            let frame_resp = egui::Frame::new()
                .fill(row_bg)
                .corner_radius(3.0)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(&mut popup_ui, |ui| {
                    let cmd_col = if is_selected {
                        t::text_primary()
                    } else {
                        t::accent()
                    };
                    let summary_col = if is_selected {
                        t::text_secondary()
                    } else {
                        t::text_muted()
                    };
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(entry.usage.clone())
                                .font(t::small())
                                .color(cmd_col)
                                .strong(),
                        );
                        ui.add_sized(
                            [ui.available_width(), 14.0],
                            egui::Label::new(
                                RichText::new(entry.summary.clone())
                                    .font(t::small())
                                    .color(summary_col),
                            )
                            .truncate(),
                        );
                    });
                });

            let row_rect = frame_resp.response.rect;
            let click_resp = popup_ui.interact(row_rect, row_id, egui::Sense::click());
            if click_resp.clicked() {
                clicked_cmd = Some(entry.name.to_owned());
            }
        }

        clicked_cmd
    }

    fn show_join_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[String],
        selected: i32,
    ) -> Option<String> {
        let input_rect = text_resp.rect;
        let popup_width = input_rect.width().min(420.0);
        let row_h = 22.0;
        let popup_h =
            (matches.len() as f32 * row_h).min(SLASH_AUTOCOMPLETE_MAX as f32 * row_h) + 8.0;
        let popup_rect = egui::Rect::from_min_size(
            egui::pos2(input_rect.left(), input_rect.top() - popup_h - 4.0),
            egui::vec2(popup_width, popup_h),
        );

        let layer_id = LayerId::new(
            Order::Foreground,
            text_resp.id.with("join_autocomplete_popup"),
        );
        let painter = ui.ctx().layer_painter(layer_id);
        painter.rect_filled(popup_rect, 6.0, t::bg_raised());
        painter.rect_stroke(
            popup_rect,
            6.0,
            egui::Stroke::new(1.0_f32, t::border_subtle()),
            egui::epaint::StrokeKind::Outside,
        );

        let mut popup_ui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer_id)
                .max_rect(popup_rect.shrink(4.0))
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        popup_ui.set_clip_rect(popup_rect);

        let mut clicked: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt(text_resp.id.with("join_ac_scroll"))
            .auto_shrink([false; 2])
            .show(&mut popup_ui, |ui| {
                for (i, channel) in matches.iter().enumerate() {
                    let is_selected = i as i32 == selected;
                    let row_id = text_resp.id.with("join_ac_row").with(i);
                    let row_bg = if is_selected {
                        t::active_channel_bg()
                    } else {
                        Color32::TRANSPARENT
                    };

                    let frame_resp = egui::Frame::new()
                        .fill(row_bg)
                        .corner_radius(3.0)
                        .inner_margin(egui::Margin::symmetric(6, 2))
                        .show(ui, |ui| {
                            let col = if is_selected {
                                t::text_primary()
                            } else {
                                t::accent()
                            };
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(channel).font(t::small()).color(col).strong(),
                                );
                                ui.label(
                                    RichText::new("Known IRC channel on this server")
                                        .font(t::small())
                                        .color(t::text_muted()),
                                );
                            });
                        });

                    let row_rect = frame_resp.response.rect;
                    if is_selected {
                        ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                    }
                    let click_resp = ui.interact(row_rect, row_id, egui::Sense::click());
                    if click_resp.clicked() {
                        clicked = Some(channel.clone());
                    }
                }
            });

        clicked
    }

    fn show_username_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[String],
        selected: i32,
    ) -> Option<String> {
        let input_rect = text_resp.rect;
        let popup_width = input_rect.width().min(350.0);
        let row_h = 24.0;
        let popup_h =
            (matches.len() as f32 * row_h).min(USERNAME_AUTOCOMPLETE_MAX as f32 * row_h) + 8.0;
        let popup_rect = egui::Rect::from_min_size(
            egui::pos2(input_rect.left(), input_rect.top() - popup_h - 4.0),
            egui::vec2(popup_width, popup_h),
        );

        let layer_id = LayerId::new(
            Order::Foreground,
            text_resp.id.with("username_autocomplete_popup"),
        );
        let painter = ui.ctx().layer_painter(layer_id);
        painter.rect_filled(popup_rect, 6.0, t::bg_raised());
        painter.rect_stroke(
            popup_rect,
            6.0,
            egui::Stroke::new(1.0_f32, t::border_subtle()),
            egui::epaint::StrokeKind::Outside,
        );

        let mut popup_ui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer_id)
                .max_rect(popup_rect.shrink(4.0))
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        popup_ui.set_clip_rect(popup_rect);

        let mut clicked_username: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt(text_resp.id.with("username_ac_scroll"))
            .auto_shrink([false; 2])
            .show(&mut popup_ui, |ui| {
                for (i, username) in matches.iter().enumerate() {
                    let is_selected = i as i32 == selected;
                    let row_id = text_resp.id.with("username_ac_row").with(i);
                    let row_bg = if is_selected {
                        t::active_channel_bg()
                    } else {
                        Color32::TRANSPARENT
                    };

                    let frame_resp = egui::Frame::new()
                        .fill(row_bg)
                        .corner_radius(3.0)
                        .inner_margin(egui::Margin::symmetric(6, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // @ icon
                                ui.label(
                                    RichText::new("@").font(t::small()).color(t::text_muted()),
                                );
                                let col = if is_selected {
                                    t::text_primary()
                                } else {
                                    t::accent()
                                };
                                ui.label(
                                    RichText::new(username).font(t::small()).color(col).strong(),
                                );
                            });
                        });

                    let row_rect = frame_resp.response.rect;
                    if is_selected {
                        ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                    }
                    let click_resp = ui.interact(row_rect, row_id, egui::Sense::click());
                    if click_resp.clicked() {
                        clicked_username = Some(username.clone());
                    }
                }
            });

        clicked_username
    }
}

/// Find emotes matching a `:partial` token at the end of the input buffer.
fn find_autocomplete_matches<'a>(
    buf: &str,
    catalog: &'a [EmoteCatalogEntry],
) -> Vec<&'a EmoteCatalogEntry> {
    let query = match extract_colon_query(buf) {
        Some(q) if !q.is_empty() => q,
        _ => return Vec::new(),
    };

    let query_lower = query.to_lowercase();
    let mut matches: Vec<&EmoteCatalogEntry> = catalog
        .iter()
        .filter(|e| e.code.to_lowercase().contains(&query_lower))
        .collect();

    // Prefix matches first, then shorter codes first
    matches.sort_by(|a, b| {
        let a_prefix = a.code.to_lowercase().starts_with(&query_lower);
        let b_prefix = b.code.to_lowercase().starts_with(&query_lower);
        b_prefix
            .cmp(&a_prefix)
            .then_with(|| a.code.len().cmp(&b.code.len()))
            .then_with(|| a.code.cmp(&b.code))
    });

    matches.truncate(AUTOCOMPLETE_MAX);
    matches
}

/// Find slash commands matching an in-progress `/command` token.
fn find_slash_matches(
    buf: &str,
    usage_counts: &HashMap<String, u32>,
) -> Vec<SlashCommandSuggestion> {
    let Some(query) = extract_slash_query(buf) else {
        return Vec::new();
    };
    slash_command_matches_ranked(query, SLASH_AUTOCOMPLETE_MATCH_LIMIT, usage_counts)
}

/// Find usernames matching an `@partial` token at the end of the input buffer.
fn find_username_matches(buf: &str, chatters: &[String]) -> Vec<String> {
    let query = match extract_mention_query(buf) {
        Some(q) if !q.is_empty() => q,
        _ => return Vec::new(),
    };

    let query_lower = query.to_lowercase();
    let mut matches: Vec<&String> = chatters
        .iter()
        .filter(|u| u.to_lowercase().contains(&query_lower))
        .collect();

    // Prefix matches first, then shorter names first
    matches.sort_by(|a, b| {
        let a_prefix = a.to_lowercase().starts_with(&query_lower);
        let b_prefix = b.to_lowercase().starts_with(&query_lower);
        b_prefix
            .cmp(&a_prefix)
            .then_with(|| a.len().cmp(&b.len()))
            .then_with(|| a.cmp(b))
    });

    matches.truncate(USERNAME_AUTOCOMPLETE_MAX);
    matches.into_iter().cloned().collect()
}

/// Find channel suggestions for `/join <channel>` on IRC server tabs.
fn find_join_channel_matches(
    buf: &str,
    current_channel: &ChannelId,
    known_channels: &[ChannelId],
) -> Vec<String> {
    if !current_channel.is_irc() {
        return Vec::new();
    }
    let Some(query) = extract_join_query(buf) else {
        return Vec::new();
    };
    let Some(current_target) = current_channel.irc_target() else {
        return Vec::new();
    };

    let query_lower = query.trim_start_matches('#').to_ascii_lowercase();
    let mut channels: Vec<String> = known_channels
        .iter()
        .filter_map(|ch| {
            let t = ch.irc_target()?;
            if t.host != current_target.host
                || t.port != current_target.port
                || t.tls != current_target.tls
            {
                return None;
            }
            if t.channel == IRC_SERVER_CONTROL_CHANNEL {
                return None;
            }
            Some(t.channel)
        })
        .collect();

    channels.sort();
    channels.dedup();

    if !query_lower.is_empty() {
        channels.retain(|name| name.to_ascii_lowercase().contains(&query_lower));
    }

    channels.sort_by(|a, b| {
        let a_lower = a.to_ascii_lowercase();
        let b_lower = b.to_ascii_lowercase();
        let a_prefix = a_lower.starts_with(&query_lower);
        let b_prefix = b_lower.starts_with(&query_lower);
        b_prefix
            .cmp(&a_prefix)
            .then_with(|| a.len().cmp(&b.len()))
            .then_with(|| a_lower.cmp(&b_lower))
    });

    channels.truncate(SLASH_AUTOCOMPLETE_MAX);
    channels
        .into_iter()
        .map(|name| format!("#{name}"))
        .collect()
}

fn normalize_spell_menu_word(word: &str) -> String {
    let trimmed = word.trim();
    if trimmed.contains(char::is_whitespace) {
        let collapsed: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if !collapsed.is_empty() {
            return collapsed;
        }
    }
    trimmed.to_owned()
}

/// Extract the in-progress `/join` query token (without validation).
fn extract_join_query(buf: &str) -> Option<&str> {
    let trimmed_start = buf.trim_start();
    if !trimmed_start.starts_with('/') {
        return None;
    }
    let without_slash = &trimmed_start[1..];
    let (cmd, rest) = without_slash.split_once(char::is_whitespace)?;
    if !cmd.eq_ignore_ascii_case("join") {
        return None;
    }

    let rest = rest.trim_start();
    if rest.is_empty() {
        return Some("");
    }

    // Stop suggesting after the first argument is complete.
    if buf
        .chars()
        .last()
        .map(|c| c.is_whitespace())
        .unwrap_or(false)
    {
        return None;
    }

    let mut args = rest.split_whitespace();
    let first = args.next()?;
    if args.next().is_some() {
        return None;
    }
    Some(first)
}

/// Extract the partial query after the last `:` in the buffer.
fn extract_colon_query(buf: &str) -> Option<&str> {
    // If the buffer ends with whitespace the user has moved past the
    // colon-token - dismiss autocomplete immediately.
    if buf.is_empty() || buf.as_bytes()[buf.len() - 1].is_ascii_whitespace() {
        return None;
    }

    let bytes = buf.as_bytes();

    for i in (0..bytes.len()).rev() {
        if bytes[i] == b':' {
            if i == 0 || bytes[i - 1] == b' ' {
                let after = &buf[i + 1..];
                if !after.contains(' ') && !after.is_empty() {
                    return Some(after);
                }
            }
        }
    }
    None
}

/// Extract the partial query after the last `@` in the buffer.
fn extract_mention_query(buf: &str) -> Option<&str> {
    // Dismiss if buffer ends with whitespace.
    if buf.is_empty() || buf.as_bytes()[buf.len() - 1].is_ascii_whitespace() {
        return None;
    }

    let bytes = buf.as_bytes();

    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'@' {
            if i == 0 || bytes[i - 1] == b' ' {
                let after = &buf[i + 1..];
                if !after.contains(' ') && !after.is_empty() {
                    return Some(after);
                }
            }
        }
    }
    None
}

fn image_paste_requested(
    events: &[egui::Event],
    shortcut_held: bool,
    skip_release: &mut bool,
) -> bool {
    let mut requested = false;
    for event in events {
        match event {
            egui::Event::Paste(_) => {
                requested = true;
                *skip_release = shortcut_held;
            }
            egui::Event::Key {
                key: Key::V,
                pressed,
                modifiers,
                ..
            } if modifiers.command || modifiers.ctrl => {
                if *pressed {
                    requested = true;
                    *skip_release = true;
                } else {
                    requested |= !*skip_release;
                    *skip_release = false;
                }
            }
            _ => {}
        }
    }
    requested
}

/// Move the TextEdit cursor to a character index.
fn move_cursor_to_end(ctx: &egui::Context, id: egui::Id, char_pos: usize) {
    use egui::text::{CCursor, CCursorRange};
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let cursor = CCursor::new(char_pos);
        state.cursor.set_char_range(Some(CCursorRange::one(cursor)));
        egui::TextEdit::store_state(ctx, id, state);
    }
}

fn select_all_text(ctx: &egui::Context, id: egui::Id, total_chars: usize) {
    use egui::text::{CCursor, CCursorRange};
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        state.cursor.set_char_range(Some(CCursorRange::two(
            CCursor::new(0),
            CCursor::new(total_chars),
        )));
        egui::TextEdit::store_state(ctx, id, state);
    }
}

fn selected_input_range(
    ctx: &egui::Context,
    id: egui::Id,
    text: &str,
) -> Option<std::ops::Range<usize>> {
    let range = egui::TextEdit::load_state(ctx, id)?.cursor.char_range()?;
    let start = range.primary.index.min(range.secondary.index);
    let end = range.primary.index.max(range.secondary.index);
    let byte_index = |index| {
        text.char_indices()
            .nth(index)
            .map_or(text.len(), |(byte, _)| byte)
    };
    let selection = byte_index(start)..byte_index(end);
    (!selection.is_empty()).then_some(selection)
}

fn copy_or_cut_selection(ctx: &egui::Context, id: egui::Id, text: &mut String, cut: bool) {
    let Some(selection) = selected_input_range(ctx, id, text) else {
        return;
    };
    ctx.copy_text(text[selection.clone()].to_owned());
    if cut {
        let cursor = text[..selection.start].chars().count();
        text.replace_range(selection, "");
        move_cursor_to_end(ctx, id, cursor);
    }
    ctx.memory_mut(|memory| memory.request_focus(id));
}

fn cursor_byte_index(ctx: &egui::Context, id: Id, buf: &str) -> usize {
    let cursor = egui::TextEdit::load_state(ctx, id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| range.primary.index);
    cursor
        .and_then(|cursor| buf.char_indices().nth(cursor).map(|(index, _)| index))
        .unwrap_or(buf.len())
}

pub fn invalidate_emote_completions(ctx: &egui::Context, id: Id) {
    ctx.data_mut(|data| {
        data.remove::<TabState>(id.with("tab_complete_state"));
        data.remove::<i32>(id.with("autocomplete_selection"));
    });
}

pub fn insert_picker_emote(ctx: &egui::Context, id: Id, buf: &mut String, code: &str) {
    let cursor = cursor_byte_index(ctx, id, buf);
    let range = selected_input_range(ctx, id, buf).unwrap_or(cursor..cursor);
    let leading_space = buf[..range.start]
        .chars()
        .next_back()
        .is_some_and(|ch| !ch.is_whitespace());
    let next = buf[range.end..].chars().next();
    let trailing_space = next.is_none_or(|ch| !ch.is_whitespace());
    let inserted = format!(
        "{}{}{}",
        if leading_space { " " } else { "" },
        code,
        if trailing_space { " " } else { "" }
    );
    let end = range.start
        + inserted.len()
        + next
            .filter(|ch| ch.is_whitespace())
            .map_or(0, char::len_utf8);
    buf.replace_range(range, &inserted);
    move_cursor_to_end(ctx, id, buf[..end].chars().count());
    ctx.data_mut(|data| {
        data.insert_temp(id.with("tab_complete_state"), TabState::default());
        data.insert_temp(id.with("msg_history_state"), HistState::default());
    });
    ctx.memory_mut(|memory| memory.request_focus(id));
}

/// The word containing the caret; all offsets are valid UTF-8 byte boundaries.
fn completion_token_range(buf: &str, caret: usize) -> std::ops::Range<usize> {
    let start = buf[..caret]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    let end = buf[caret..]
        .find(char::is_whitespace)
        .map_or(buf.len(), |index| caret + index);
    start..end
}

fn apply_completion(
    ctx: &egui::Context,
    id: Id,
    buf: &mut String,
    caret: usize,
    matches: Vec<String>,
    index: usize,
    keep_cycle: bool,
) {
    let range = completion_token_range(buf, caret);
    let mut session = TabState {
        prefix: buf[..range.start].to_owned(),
        suffix: buf[range.end..].to_owned(),
        matches,
        index,
        ..Default::default()
    };
    let cursor = session.apply(buf);
    move_cursor_to_end(ctx, id, cursor);
    ctx.data_mut(|data| {
        data.insert_temp(
            id.with("tab_complete_state"),
            if keep_cycle {
                session
            } else {
                TabState::default()
            },
        )
    });
}

fn bare_completion_matches(
    word: &str,
    catalog: &[EmoteCatalogEntry],
    chatters: &[String],
) -> Vec<String> {
    if word.is_empty() {
        return Vec::new();
    }
    let (mention, query) = word
        .strip_prefix('@')
        .map_or((false, word), |query| (true, query));
    let query = query.to_ascii_lowercase();
    let starts_with = |candidate: &str| {
        candidate
            .as_bytes()
            .get(..query.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query.as_bytes()))
    };
    let mut matches: Vec<String> = if mention {
        Vec::new()
    } else {
        catalog
            .iter()
            .filter(|entry| starts_with(&entry.code))
            .map(|entry| entry.code.clone())
            .collect()
    };
    matches.extend(
        chatters
            .iter()
            .filter(|name| starts_with(name))
            .map(|name| {
                if mention {
                    format!("@{name}")
                } else {
                    name.clone()
                }
            }),
    );
    matches.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    matches.dedup();
    matches.truncate(TAB_COMPLETE_MAX);
    matches
}

fn provider_label(provider: &str) -> &'static str {
    match provider {
        "bttv" => "BTTV",
        "ffz" => "FFZ",
        "7tv" => "7TV",
        "twitch" => "Twitch",
        "kick" => "Kick",
        _ => "Emote",
    }
}

fn fit_size(w: u32, h: u32, target_h: f32) -> Vec2 {
    if w == 0 || h == 0 {
        return Vec2::new(target_h, target_h);
    }
    let scale_x = target_h / w as f32;
    let scale_y = target_h / h as f32;
    let scale = scale_x.min(scale_y);
    Vec2::new(w as f32 * scale, h as f32 * scale)
}

fn is_likely_animated_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains(".gif") || lower.contains(".webp")
}

fn truncate_to_max_chars(buf: &mut String, max_chars: usize) {
    if buf.chars().count() <= max_chars {
        return;
    }
    let cut = buf
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(buf.len());
    buf.truncate(cut);
}

/// Iterate over alphabetic words in `buf`.
/// Yields `(word_slice, char_start, byte_start, char_end)`.
fn iter_words(buf: &str) -> Vec<(&str, usize, usize, usize)> {
    let mut result = Vec::new();
    let mut start = None;
    let mut char_end = 0;
    for (char_idx, (byte_idx, ch)) in buf.char_indices().enumerate() {
        char_end = char_idx + 1;
        if ch.is_ascii_alphabetic() {
            start.get_or_insert((char_idx, byte_idx));
        } else if let Some((char_start, byte_start)) = start.take() {
            result.push((&buf[byte_start..byte_idx], char_start, byte_start, char_idx));
        }
    }
    if let Some((char_start, byte_start)) = start {
        result.push((&buf[byte_start..], char_start, byte_start, char_end));
    }
    result
}

/// Paint a red wavy underline from `x0` to `x1` at screen-space `y`.
fn paint_wavy_underline(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    const WAVE_H: f32 = 1.5; // half amplitude
    const WAVE_LEN: f32 = 4.0; // one full wave = 4px

    let stroke = Stroke::new(1.0_f32, color);
    let mut points: Vec<Pos2> = Vec::new();

    let mut x = x0;
    while x <= x1 {
        // Triangle wave: cycles through 0 -> +H -> 0 -> -H -> 0 over WAVE_LEN
        let phase = (x - x0) % WAVE_LEN;
        let half = WAVE_LEN / 2.0;
        let dy = if phase < half {
            WAVE_H * (1.0 - 2.0 * (phase - half / 2.0).abs() / half)
        } else {
            -WAVE_H * (1.0 - 2.0 * (phase - half - half / 2.0).abs() / half)
        };
        points.push(Pos2::new(x, y + dy));
        x += 1.0;
    }
    // Close to exact x1
    if points.last().map(|p| p.x) != Some(x1) {
        points.push(Pos2::new(x1, y));
    }

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, stroke));
    }
}

/// Read an image from the OS clipboard and re-encode as PNG.
/// Returns `None` if the clipboard is empty, unavailable, or holds no image.
///
/// Retries briefly on Windows because egui-winit's own `arboard` instance may
/// hold the clipboard open for a few milliseconds while it tries its
/// text-paste attempt; a cold retry after a short sleep usually succeeds.
fn try_clipboard_image_as_png() -> Option<PendingUpload> {
    const ATTEMPTS: u32 = 4;
    const SLEEP_MS: u64 = 15;
    let mut last_err: Option<String> = None;
    let img = (0..ATTEMPTS).find_map(|attempt| {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(SLEEP_MS));
        }
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                last_err = Some(format!("Clipboard::new: {e}"));
                return None;
            }
        };
        match cb.get_image() {
            Ok(img) => Some(img),
            Err(e) => {
                last_err = Some(format!("get_image: {e}"));
                None
            }
        }
    });
    let img = match img {
        Some(i) => i,
        None => {
            if let Some(e) = last_err {
                tracing::debug!("uploader: clipboard has no image after retries ({e})");
            }
            return None;
        }
    };
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 {
        return None;
    }
    let rgba = img.bytes.into_owned();
    let mut png = Vec::new();
    {
        use image::codecs::png::PngEncoder;
        use image::ImageEncoder;
        let encoder = PngEncoder::new(&mut png);
        if let Err(e) = encoder.write_image(&rgba, w, h, image::ExtendedColorType::Rgba8) {
            tracing::warn!("uploader: png encode failed: {e}");
            return None;
        }
    }
    Some(PendingUpload {
        bytes: png,
        format: "png".to_owned(),
        source_path: None,
    })
}

/// Convert an `egui::DroppedFile` to a [`PendingUpload`] if it looks like an image.
fn dropped_file_to_upload(f: &egui::DroppedFile) -> Option<PendingUpload> {
    // Web drops give us bytes + name; desktop drops give us a path.
    let (path, raw): (Option<std::path::PathBuf>, Option<Vec<u8>>) = match (&f.path, &f.bytes) {
        (Some(p), _) => (Some(p.clone()), None),
        (None, Some(b)) => (None, Some(b.to_vec())),
        (None, None) => return None,
    };

    let name_lower = path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| {
            let nm = &f.name;
            nm.rsplit('.').next().map(|s| s.to_ascii_lowercase())
        })
        .unwrap_or_default();
    let format = match name_lower.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpeg",
        "gif" => "gif",
        "webp" => "webp",
        _ => return None,
    };

    let bytes = match raw {
        Some(b) => b,
        None => match std::fs::read(path.as_ref()?) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("uploader: read drop file failed: {e}");
                return None;
            }
        },
    };

    // Upload known image formats unchanged to preserve animation and avoid re-encoding.
    Some(PendingUpload {
        bytes,
        format: format.to_owned(),
        source_path: path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        }
    }

    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        }
    }

    fn show_test_input(
        ui: &mut Ui,
        buf: &mut String,
        history: &[String],
        focus: bool,
        accept_dropped_files: bool,
    ) -> ChatInputResult {
        show_test_input_with_completions(ui, buf, history, focus, accept_dropped_files, &[], &[])
    }

    fn show_test_input_with_completions(
        ui: &mut Ui,
        buf: &mut String,
        history: &[String],
        focus: bool,
        accept_dropped_files: bool,
        catalog: &[EmoteCatalogEntry],
        chatters: &[String],
    ) -> ChatInputResult {
        if focus {
            let id = ui.id().with("chat_input");
            ui.memory_mut(|m| m.request_focus(id));
        }
        ChatInput {
            id: None,
            channel: &ChannelId::new("example"),
            logged_in: true,
            username: None,
            emote_catalog: catalog,
            emote_bytes: &UiImageCache::default(),
            pending_reply: None,
            message_history: history,
            slash_usage_counts: &HashMap::new(),
            known_channels: &[],
            chatters,
            prevent_overlong_twitch_messages: false,
            animate_emotes: false,
            accept_dropped_files,
        }
        .show(ui, buf)
    }

    #[test]
    fn long_reply_names_leave_the_dismiss_button_reachable() {
        for width in [150.0, 210.0, 360.0] {
            let ctx = egui::Context::default();
            let reply = ReplyInfo {
                parent_msg_id: "parent".to_owned(),
                parent_user_login: "long_sender".to_owned(),
                parent_display_name: "A very long display name with 日本語".repeat(3),
                parent_msg_body: "A reply preview that must fit beside its dismiss control"
                    .repeat(3),
            };
            let render = |events: Vec<egui::Event>| {
                let mut dismissed = false;
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            Pos2::ZERO,
                            egui::vec2(width, 180.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            dismissed = ChatInput {
                                id: None,
                                channel: &ChannelId::new("example"),
                                logged_in: true,
                                username: None,
                                emote_catalog: &[],
                                emote_bytes: &UiImageCache::default(),
                                pending_reply: Some(&reply),
                                message_history: &[],
                                slash_usage_counts: &HashMap::new(),
                                known_channels: &[],
                                chatters: &[],
                                prevent_overlong_twitch_messages: false,
                                animate_emotes: false,
                                accept_dropped_files: false,
                            }
                            .show(ui, &mut String::new())
                            .dismiss_reply;
                        });
                    },
                );
                let position = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == "✕" => {
                            let position = text.pos + text.galley.size() / 2.0;
                            assert!(shape.clip_rect.contains(position));
                            Some(position)
                        }
                        _ => None,
                    })
                    .expect("reply dismiss control is visible");
                assert!(position.x < width);
                (dismissed, position)
            };
            let (_, pos) = render(Vec::new());
            render(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            let (dismissed, _) = render(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            assert!(dismissed, "reply dismiss failed at width {width}");
        }
    }

    #[test]
    fn picker_emotes_replace_unicode_selections_and_keep_the_following_text() {
        let ctx = egui::Context::default();
        let mut buf = "hé old tail🙂".to_owned();
        let id = completion_frame(&ctx, &mut buf, Vec::new(), Some(3));
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(3),
                egui::text::CCursor::new(6),
            )));
        state.store(&ctx, id);
        insert_picker_emote(&ctx, id, &mut buf, "Kappa");
        assert_eq!(buf, "hé Kappa tail🙂");
        let state = egui::TextEdit::load_state(&ctx, id).unwrap();
        assert_eq!(
            state.cursor.char_range().unwrap().primary.index,
            "hé Kappa ".chars().count()
        );
        assert!(ctx.memory(|memory| memory.has_focus(id)));
    }

    #[test]
    fn picker_emotes_insert_at_the_caret_and_separate_adjacent_words() {
        let ctx = egui::Context::default();
        let mut buf = "hello🙂world".to_owned();
        let id = completion_frame(&ctx, &mut buf, Vec::new(), Some(6));
        insert_picker_emote(&ctx, id, &mut buf, "Kappa");
        assert_eq!(buf, "hello🙂 Kappa world");
    }

    #[test]
    fn completion_cycles_at_the_unicode_caret_and_preserves_the_suffix() {
        let ctx = egui::Context::default();
        let mut buf = "é🙂 kaXYZ  tail🙂".to_owned();
        let id = completion_frame(&ctx, &mut buf, Vec::new(), Some("é🙂 ka".chars().count()));
        for (modifiers, word) in [
            (egui::Modifiers::NONE, "Kappa"),
            (egui::Modifiers::NONE, "KappaHD"),
            (egui::Modifiers::SHIFT, "Kappa"),
            (egui::Modifiers::SHIFT, "KappaHD"),
        ] {
            assert!(ctx.memory(|memory| memory.has_focus(id)), "before {word}: completion must keep the composer focused; expected={id:?}, focused={:?}", ctx.memory(|memory| memory.focused()));
            completion_frame(&ctx, &mut buf, vec![key(Key::Tab, true, modifiers)], None);
            let cursor = egui::TextEdit::load_state(&ctx, id)
                .unwrap()
                .cursor
                .char_range()
                .unwrap();
            let session: TabState = ctx.data(|data| {
                data.get_temp(id.with("tab_complete_state"))
                    .unwrap_or_default()
            });
            assert_eq!(
                buf,
                format!("é🙂 {word}  tail🙂"),
                "modifiers={modifiers:?}, cursor={}, expected_cursor={}, index={}, matches={:?}",
                cursor.primary.index,
                session.expected_cursor,
                session.index,
                session.matches
            );
            assert_eq!(cursor.primary.index, format!("é🙂 {word} ").chars().count());
            assert_eq!(cursor.primary.index, cursor.secondary.index);
        }
    }

    #[test]
    fn shift_tab_starts_with_the_last_completion() {
        let ctx = egui::Context::default();
        let mut buf = "ka tail".to_owned();
        completion_frame(&ctx, &mut buf, Vec::new(), Some(2));
        completion_frame(
            &ctx,
            &mut buf,
            vec![key(Key::Tab, true, egui::Modifiers::SHIFT)],
            None,
        );
        assert_eq!(buf, "KappaHD tail");
    }

    #[test]
    fn catalog_and_channel_changes_end_active_completion_cycles() {
        for change_channel in [false, true] {
            let ctx = egui::Context::default();
            let mut buf = "ka".to_owned();
            let id = Id::new("scoped_composer");
            let first_channel = ChannelId::new("first");
            let second_channel = ChannelId::new("second");
            let catalog: Vec<_> = ["Kappa", "KappaHD"]
                .into_iter()
                .map(|code| EmoteCatalogEntry {
                    id: code.to_owned(),
                    code: code.to_owned(),
                    provider: "7tv".to_owned(),
                    url: code.to_owned(),
                    url_hd: None,
                    scope: "channel".to_owned(),
                    channel: Some(first_channel.clone()),
                    user_id: None,
                })
                .collect();
            let render = |buf: &mut String,
                          channel: &ChannelId,
                          catalog: &[EmoteCatalogEntry],
                          tab: bool| {
                let events = if tab {
                    vec![
                        key(Key::Tab, true, egui::Modifiers::NONE),
                        key(Key::Tab, false, egui::Modifiers::NONE),
                    ]
                } else {
                    Vec::new()
                };
                let _ = ctx.run(raw_input(events), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.memory_mut(|memory| memory.request_focus(id));
                        ChatInput {
                            id: Some(id),
                            channel,
                            logged_in: true,
                            username: None,
                            emote_catalog: catalog,
                            emote_bytes: &UiImageCache::default(),
                            pending_reply: None,
                            message_history: &[],
                            slash_usage_counts: &HashMap::new(),
                            known_channels: &[],
                            chatters: &[],
                            prevent_overlong_twitch_messages: false,
                            animate_emotes: false,
                            accept_dropped_files: false,
                        }
                        .show(ui, buf);
                    });
                });
            };
            render(&mut buf, &first_channel, &catalog, false);
            render(&mut buf, &first_channel, &catalog, true);
            assert_eq!(buf, "Kappa ");
            if !change_channel {
                invalidate_emote_completions(&ctx, id);
            }
            let channel = if change_channel {
                &second_channel
            } else {
                &first_channel
            };
            render(&mut buf, channel, &catalog[..1], true);
            assert_eq!(buf, "Kappa ", "removed emotes must leave the active cycle");
        }
    }

    #[test]
    fn moving_the_caret_starts_a_new_completion_session() {
        let ctx = egui::Context::default();
        let mut buf = "ka tail".to_owned();
        completion_frame(&ctx, &mut buf, Vec::new(), Some(2));
        completion_frame(
            &ctx,
            &mut buf,
            vec![key(Key::Tab, true, egui::Modifiers::NONE)],
            None,
        );
        assert_eq!(buf, "Kappa tail");
        let end = buf.chars().count();
        completion_frame(&ctx, &mut buf, Vec::new(), Some(end));
        completion_frame(
            &ctx,
            &mut buf,
            vec![key(Key::Tab, true, egui::Modifiers::NONE)],
            None,
        );
        assert_eq!(buf, "Kappa TailEmote ");
    }

    #[test]
    fn popup_completions_replace_the_caret_token_without_removing_arguments() {
        for (initial, prefix, expected) in [
            ("é🙂 :kaXXX tail", "é🙂 :ka", "é🙂 Kappa tail"),
            ("é🙂 @alXXX tail", "é🙂 @al", "é🙂 @alice tail"),
            ("/heXXX argument", "/he", "/help argument"),
        ] {
            let ctx = egui::Context::default();
            let mut buf = initial.to_owned();
            completion_frame(&ctx, &mut buf, Vec::new(), Some(prefix.chars().count()));
            completion_frame(
                &ctx,
                &mut buf,
                vec![key(Key::Tab, true, egui::Modifiers::NONE)],
                None,
            );
            assert_eq!(buf, expected);
        }
    }

    fn completion_frame(
        ctx: &egui::Context,
        buf: &mut String,
        mut events: Vec<egui::Event>,
        caret: Option<usize>,
    ) -> Id {
        let releases: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key {
                    key: Key::Tab,
                    pressed: true,
                    modifiers,
                    ..
                } => Some(key(Key::Tab, false, *modifiers)),
                _ => None,
            })
            .collect();
        events.extend(releases);
        let catalog: Vec<EmoteCatalogEntry> = ["Kappa", "KappaHD", "TailEmote"]
            .into_iter()
            .map(|code| EmoteCatalogEntry {
                id: code.to_owned(),
                url_hd: None,
                channel: None,
                user_id: None,
                code: code.to_owned(),
                provider: "test".to_owned(),
                url: format!("https://example.test/{code}.png"),
                scope: "global".to_owned(),
            })
            .collect();
        let chatters = ["alice".to_owned(), "alexander".to_owned()];
        let mut id = Id::NULL;
        let _ = ctx.run(raw_input(events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                id = ui.id().with("chat_input");
                show_test_input_with_completions(
                    ui,
                    buf,
                    &[],
                    caret.is_some(),
                    false,
                    &catalog,
                    &chatters,
                );
                if let Some(caret) = caret {
                    move_cursor_to_end(ctx, id, caret);
                }
            });
        });
        id
    }

    #[test]
    fn unfocused_input_leaves_navigation_and_autocomplete_keys_alone() {
        for pressed in [Key::Tab, Key::ArrowUp, Key::ArrowDown, Key::Enter] {
            let ctx = egui::Context::default();
            let mut buf = "/he".to_owned();
            let _ = ctx.run(
                raw_input(vec![key(pressed, true, egui::Modifiers::NONE)]),
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.memory_mut(|m| m.request_focus(Id::new("other_input")));
                        let result =
                            show_test_input(ui, &mut buf, &["sent".to_owned()], false, false);
                        assert!(result.send.is_none());
                        assert_eq!(buf, "/he");
                        assert!(ui.input(|i| i.events.iter().any(|event| matches!(
                            event,
                            egui::Event::Key { key, pressed: true, .. } if *key == pressed
                        ))));
                    });
                },
            );
        }
    }

    #[test]
    fn history_starts_with_latest_message_and_restores_draft() {
        let history = vec!["older".to_owned(), "newest".to_owned()];
        for draft in ["", "unfinished draft"] {
            let ctx = egui::Context::default();
            let mut buf = draft.to_owned();
            let _ = ctx.run(raw_input(Vec::new()), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_test_input(ui, &mut buf, &history, true, false);
                });
            });
            for (pressed, expected) in [
                (Key::ArrowUp, "newest"),
                (Key::ArrowUp, "older"),
                (Key::ArrowDown, "newest"),
                (Key::ArrowDown, draft),
            ] {
                let _ = ctx.run(
                    raw_input(vec![key(pressed, true, egui::Modifiers::NONE)]),
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            show_test_input(ui, &mut buf, &history, false, false);
                        });
                    },
                );
                assert_eq!(buf, expected);
            }
        }
    }

    #[test]
    fn later_split_keeps_ownership_of_history_shortcuts() {
        let ctx = egui::Context::default();
        let history = vec!["older".to_owned(), "newest".to_owned()];
        let mut first = String::new();
        let mut second = String::new();
        for warmup in [true, false] {
            let events = if warmup {
                Vec::new()
            } else {
                vec![key(Key::ArrowUp, true, egui::Modifiers::NONE)]
            };
            let _ = ctx.run(raw_input(events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.push_id("first", |ui| {
                        show_test_input(ui, &mut first, &history, false, false);
                    });
                    ui.push_id("second", |ui| {
                        show_test_input(ui, &mut second, &history, warmup, false);
                    });
                });
            });
        }
        assert!(first.is_empty());
        assert_eq!(second, "newest");
    }

    #[test]
    fn rejected_enter_keeps_focus_and_preserves_the_draft() {
        for initial in [String::new(), "  ".to_owned(), "x".repeat(501)] {
            let ctx = egui::Context::default();
            let mut buf = initial.clone();
            let mut input_id = Id::NULL;
            let _ = ctx.run(raw_input(Vec::new()), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    input_id = ui.id().with("chat_input");
                    show_test_input(ui, &mut buf, &[], true, false);
                });
            });
            let _ = ctx.run(
                raw_input(vec![key(Key::Enter, true, egui::Modifiers::NONE)]),
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        assert!(show_test_input(ui, &mut buf, &[], false, false)
                            .send
                            .is_none());
                    });
                },
            );
            assert_eq!(buf, initial);
            assert!(ctx.memory(|memory| memory.has_focus(input_id)));
        }
    }

    #[test]
    fn submitted_draft_retains_whitespace_for_recovery_if_dispatch_fails() {
        let ctx = egui::Context::default();
        let original = "  hello 🦀  ";
        let mut buf = original.to_owned();
        let _ = ctx.run(raw_input(Vec::new()), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_test_input(ui, &mut buf, &[], true, false);
            });
        });
        let mut submitted = None;
        let _ = ctx.run(
            raw_input(vec![key(Key::Enter, true, egui::Modifiers::NONE)]),
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    submitted = show_test_input(ui, &mut buf, &[], false, false).send;
                });
            },
        );
        assert_eq!(submitted.as_deref(), Some(original));
        assert!(buf.is_empty());
    }

    #[test]
    fn context_menu_submission_retains_the_original_draft() {
        let ctx = egui::Context::default();
        let original = "  hello 🦀  ";
        let mut buf = original.to_owned();
        let mut input_rect = egui::Rect::NOTHING;
        let _ = ctx.run(raw_input(Vec::new()), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let id = ui.id().with("chat_input");
                show_test_input(ui, &mut buf, &[], true, false);
                input_rect = ctx.read_response(id).unwrap().rect;
            });
        });
        let pointer_event = |position, button, pressed| {
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        for pressed in [true, false] {
            let _ = ctx.run(
                raw_input(pointer_event(
                    input_rect.center(),
                    egui::PointerButton::Secondary,
                    pressed,
                )),
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show_test_input(ui, &mut buf, &[], false, false);
                    });
                },
            );
        }
        let shapes = ctx
            .run(raw_input(Vec::new()), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_test_input(ui, &mut buf, &[], false, false);
                });
            })
            .shapes;
        let position = shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == "Send now" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("context menu should offer Send now");
        let mut submitted = None;
        for pressed in [true, false] {
            let _ = ctx.run(
                raw_input(pointer_event(
                    position,
                    egui::PointerButton::Primary,
                    pressed,
                )),
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        if let Some(text) = show_test_input(ui, &mut buf, &[], false, false).send {
                            submitted = Some(text);
                        }
                    });
                },
            );
        }
        assert_eq!(submitted.as_deref(), Some(original));
        assert!(buf.is_empty());
    }

    #[test]
    fn context_copy_and_cut_respect_unicode_selection() {
        for (primary, secondary) in [(1, 3), (3, 1)] {
            let ctx = egui::Context::default();
            let id = Id::new("selection_test");
            let mut buf = "Aé🙂Z tail".to_owned();
            let output = ctx.run(raw_input(Vec::new()), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::TextEdit::singleline(&mut buf).id(id).show(ui);
                    let mut state = egui::TextEdit::load_state(ctx, id).unwrap();
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(primary),
                            egui::text::CCursor::new(secondary),
                        )));
                    egui::TextEdit::store_state(ctx, id, state);
                    copy_or_cut_selection(ctx, id, &mut buf, false);
                    assert_eq!(buf, "Aé🙂Z tail");
                    copy_or_cut_selection(ctx, id, &mut buf, true);
                    assert_eq!(buf, "AZ tail");
                    let cursor = egui::TextEdit::load_state(ctx, id)
                        .unwrap()
                        .cursor
                        .char_range()
                        .unwrap();
                    assert_eq!(cursor.primary.index, 1);
                    assert_eq!(cursor.secondary.index, 1);
                });
            });
            assert_eq!(
                output
                    .platform_output
                    .commands
                    .iter()
                    .filter(|command| {
                        matches!(command, egui::OutputCommand::CopyText(text) if text == "é🙂")
                    })
                    .count(),
                2
            );
        }
    }

    #[test]
    fn context_cut_without_selection_keeps_the_draft() {
        let ctx = egui::Context::default();
        let id = Id::new("selection_test");
        let mut buf = "keep this draft".to_owned();
        let _ = ctx.run(raw_input(Vec::new()), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::TextEdit::singleline(&mut buf).id(id).show(ui);
                move_cursor_to_end(ctx, id, 4);
                copy_or_cut_selection(ctx, id, &mut buf, true);
            });
        });
        assert_eq!(buf, "keep this draft");
    }

    #[test]
    fn dropped_image_is_consumed_once_by_its_target_pane() {
        let ctx = egui::Context::default();
        let mut raw = raw_input(Vec::new());
        raw.dropped_files.push(egui::DroppedFile {
            name: "example.png".to_owned(),
            bytes: Some(Arc::from([1_u8, 2, 3])),
            ..Default::default()
        });
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for (index, accepts, expected) in [(0, false, 0), (1, true, 1), (2, true, 0)] {
                    ui.push_id(index, |ui| {
                        let result = show_test_input(ui, &mut String::new(), &[], false, accepts);
                        assert_eq!(result.uploads.len(), expected);
                    });
                }
            });
        });
    }

    #[test]
    fn image_paste_ignores_release_after_press_even_when_frames_pass() {
        let mut skip_release = false;
        assert!(image_paste_requested(
            &[key(Key::V, true, egui::Modifiers::CTRL)],
            true,
            &mut skip_release
        ));
        for _ in 0..20 {
            assert!(!image_paste_requested(&[], true, &mut skip_release));
        }
        assert!(!image_paste_requested(
            &[key(Key::V, false, egui::Modifiers::CTRL)],
            true,
            &mut skip_release
        ));
        assert!(image_paste_requested(
            &[key(Key::V, false, egui::Modifiers::CTRL)],
            true,
            &mut skip_release
        ));
    }

    #[test]
    fn image_paste_text_event_and_release_form_one_request() {
        let mut skip_release = false;
        assert!(image_paste_requested(
            &[egui::Event::Paste("clipboard text".to_owned())],
            true,
            &mut skip_release
        ));
        assert!(!image_paste_requested(
            &[key(Key::V, false, egui::Modifiers::CTRL)],
            true,
            &mut skip_release
        ));
    }

    #[test]
    fn spellcheck_word_offsets_preserve_unicode_positions() {
        assert_eq!(
            iter_words("🙂one, café two"),
            vec![("one", 1, 4, 4), ("caf", 6, 9, 9), ("two", 11, 15, 14)]
        );
        assert!(iter_words("").is_empty());
        assert!(iter_words("🙂123").is_empty());
    }

    #[test]
    fn truncate_to_max_chars_is_unicode_safe() {
        let mut s = "abc🙂def".to_owned();
        truncate_to_max_chars(&mut s, 4);
        assert_eq!(s, "abc🙂");
    }
}
