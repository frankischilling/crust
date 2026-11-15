use crate::image_cache::UiImageCache;
use std::collections::HashMap;
use std::sync::Arc;

use egui::{Color32, Id, Key, Order, Pos2, RichText, Stroke, Ui, Vec2};

use crate::commands::{extract_slash_query, slash_command_matches_ranked, SlashCommandSuggestion};
use crate::theme as t;
use crust_core::model::{ChannelId, EmoteCatalogEntry, ReplyInfo, IRC_SERVER_CONTROL_CHANNEL};
use crust_core::{HotkeyAction, HotkeyBindings};

const AUTOCOMPLETE_MAX: usize = 10;
const AUTOCOMPLETE_EMOTE_SIZE: f32 = 20.0;
const SLASH_AUTOCOMPLETE_MAX: usize = 10;
const SLASH_AUTOCOMPLETE_MATCH_LIMIT: usize = usize::MAX;
const USERNAME_AUTOCOMPLETE_MAX: usize = 10;
/// Maximum number of Tab-completion matches to cycle through.
const TAB_COMPLETE_MAX: usize = 50;
const TWITCH_MAX_CHARS: usize = 500;

#[derive(Clone, Copy)]
enum SendShortcut {
    Primary,
    Shifted,
}

fn consume_composer_binding(
    input: &mut egui::InputState,
    binding: &crust_core::KeyBinding,
) -> bool {
    let Some(key) = crate::app::egui_key_from_name(&binding.key) else {
        return false;
    };
    let modifiers = crate::app::binding_modifiers(binding);
    let produces_text = printable_shortcut_key(key);
    let mut consumed = false;
    let mut skip_text = false;
    input.events.retain(|event| {
        match event {
            egui::Event::Key {
                key: event_key,
                pressed,
                modifiers: event_modifiers,
                ..
            } => {
                skip_text = false;
                if *pressed && *event_key == key && event_modifiers.matches_exact(modifiers) {
                    consumed = true;
                    skip_text = produces_text;
                    return false;
                }
            }
            egui::Event::Text(_) if skip_text => {
                // A printable shortcut arrives as a key followed by text.
                // Consume its text too, without dropping the next key's text.
                skip_text = false;
                return false;
            }
            _ => {
                skip_text = false;
            }
        }
        true
    });
    consumed
}

fn printable_shortcut_key(key: Key) -> bool {
    let name = crate::app::egui_key_name(key);
    name.len() == 1
        || name.starts_with("Num")
        || matches!(
            key,
            Key::Space
                | Key::Minus
                | Key::Plus
                | Key::Equals
                | Key::Comma
                | Key::Period
                | Key::Semicolon
                | Key::Slash
                | Key::Backslash
                | Key::OpenBracket
                | Key::CloseBracket
                | Key::Backtick
                | Key::Quote
        )
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ComposerEventOwner {
    id: Id,
    channel: Id,
}

impl ComposerEventOwner {
    fn new(id: Id, channel: &ChannelId) -> Self {
        Self {
            id,
            channel: Id::new(channel),
        }
    }
}

#[derive(Clone, Default)]
struct DeferredComposerEvents {
    prepared_pass: Option<u64>,
    account_scope: Option<Id>,
    pending: HashMap<ComposerEventOwner, Vec<egui::Event>>,
    editor_states: HashMap<ComposerEventOwner, egui::text_edit::TextEditState>,
    ready: Option<(ComposerEventOwner, Vec<egui::Event>)>,
    after_navigation: Vec<egui::Event>,
}

fn composer_events_id(ctx: &egui::Context) -> Id {
    Id::new(("composer_deferred_events", ctx.viewport_id()))
}

fn is_keyboard_event(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key { .. }
            | egui::Event::Text(_)
            | egui::Event::Paste(_)
            | egui::Event::Copy
            | egui::Event::Cut
            | egui::Event::Ime(_)
    )
}

fn event_matches_binding(event: &egui::Event, binding: &crust_core::KeyBinding) -> bool {
    matches!(event, egui::Event::Key { key, pressed: true, modifiers, .. }
        if crate::app::egui_key_from_name(&binding.key) == Some(*key)
            && modifiers.matches_exact(crate::app::binding_modifiers(binding)))
}

fn is_send_shortcut(event: &egui::Event, bindings: &HotkeyBindings) -> bool {
    [HotkeyAction::InputSend, HotkeyAction::InputSendKeep]
        .into_iter()
        .any(|action| {
            event_matches_binding(event, &bindings.get(action))
                || bindings
                    .shifted_send_binding(action)
                    .is_some_and(|binding| event_matches_binding(event, &binding))
        })
}

fn remove_queued_send_shortcuts(events: &mut Vec<egui::Event>, bindings: &HotkeyBindings) {
    let mut skip_text = false;
    events.retain(|event| {
        if std::mem::take(&mut skip_text) && matches!(event, egui::Event::Text(_)) {
            return false;
        }
        if is_send_shortcut(event, bindings) {
            skip_text =
                matches!(event, egui::Event::Key { key, .. } if printable_shortcut_key(*key));
            return false;
        }
        true
    });
}

/// Keep deferred edits on account changes, but require a new Send gesture.
/// Recheck after runtime events as an account can change during a UI pass.
pub(crate) fn sync_composer_account(
    ctx: &egui::Context,
    bindings: &HotkeyBindings,
    account_id: Option<&str>,
) {
    let state_id = composer_events_id(ctx);
    let account_scope = Id::new(account_id);
    ctx.data_mut(|data| {
        let deferred = data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id);
        if deferred
            .account_scope
            .replace(account_scope)
            .is_some_and(|previous| previous != account_scope)
        {
            for events in deferred.pending.values_mut() {
                remove_queued_send_shortcuts(events, bindings);
            }
            if let Some((_, events)) = deferred.ready.as_mut() {
                remove_queued_send_shortcuts(events, bindings);
            }
            remove_queued_send_shortcuts(&mut deferred.after_navigation, bindings);
        }
    });
}

fn is_navigation_shortcut(event: &egui::Event, bindings: &HotkeyBindings) -> bool {
    use crust_core::{HotkeyCategory, KeyBinding};
    if !matches!(event, egui::Event::Key { pressed: true, .. }) {
        return false;
    }
    if HotkeyAction::all().into_iter().any(|action| {
        matches!(
            action.category(),
            HotkeyCategory::Window | HotkeyCategory::Tab | HotkeyCategory::Split
        ) && event_matches_binding(event, &bindings.get(action))
    }) {
        return true;
    }
    if bindings.get(HotkeyAction::ZoomIn) == KeyBinding::new("Equals").with_ctrl()
        && matches!(event, egui::Event::Key { key: Key::Plus, modifiers, .. }
            if modifiers.matches_logically(egui::Modifiers::CTRL))
    {
        return true;
    }
    // Keep the aliases accepted by handle_channel_shortcuts in the same batch.
    [
        (HotkeyAction::NextTab, false),
        (HotkeyAction::PrevTab, true),
    ]
    .into_iter()
    .any(|(action, reverse)| {
        let default = if reverse {
            KeyBinding::new("Tab").with_ctrl().with_shift()
        } else {
            KeyBinding::new("Tab").with_ctrl()
        };
        bindings.get(action) == default
            && (event_matches_binding(
                event,
                &KeyBinding::new(if reverse { "PageUp" } else { "PageDown" }).with_ctrl(),
            ) || event_matches_binding(
                event,
                &KeyBinding::new(if reverse { "ArrowLeft" } else { "ArrowRight" }).with_alt(),
            ))
    })
}

fn shortcut_end(events: &[egui::Event], index: usize) -> usize {
    let paired_text = matches!(&events[index], egui::Event::Key { key, .. } if printable_shortcut_key(*key))
        && matches!(events.get(index + 1), Some(egui::Event::Text(_)));
    index + 1 + usize::from(paired_text)
}

/// Return one editor segment, or a single navigation gesture. Each segment uses
/// the existing visible TextEdit, which keeps selection, undo and IME behavior.
fn composer_event_boundary(events: &[egui::Event], bindings: &HotkeyBindings) -> (usize, bool) {
    let mut has_prefix = false;
    for (index, event) in events.iter().enumerate() {
        if matches!(event, egui::Event::Key { pressed: false, .. }) {
            continue;
        }
        if is_navigation_shortcut(event, bindings) {
            return if has_prefix {
                (index, false)
            } else {
                (shortcut_end(events, index), true)
            };
        }
        if event_matches_binding(event, &bindings.get(HotkeyAction::InputClear)) && has_prefix {
            return (index, false);
        }
        if is_send_shortcut(event, bindings) {
            return (shortcut_end(events, index), false);
        }
        has_prefix = true;
    }
    (events.len(), false)
}

/// Keep edits on either side of Send, Clear and navigation in event order.
/// A hidden editor retains its input until its own channel and draft return.
pub(crate) fn prepare_composer_events(
    ctx: &egui::Context,
    bindings: &HotkeyBindings,
    focused_owner: Option<(Id, &ChannelId)>,
    account_id: Option<&str>,
) {
    sync_composer_account(ctx, bindings, account_id);
    let state_id = composer_events_id(ctx);
    let pass = ctx.cumulative_pass_nr();
    let mut deferred = ctx
        .data_mut(|data| data.remove_temp::<DeferredComposerEvents>(state_id))
        .unwrap_or_default();
    if deferred.prepared_pass == Some(pass) {
        ctx.data_mut(|data| data.insert_temp(state_id, deferred));
        return;
    }
    deferred.prepared_pass = Some(pass);
    // A pointer or an incoming activation can hide the editor after preparation.
    // Put its unclaimed prefix back before its suffix instead of dropping it.
    if let Some((owner, mut events)) = deferred.ready.take() {
        if let Some(mut suffix) = deferred.pending.remove(&owner) {
            events.append(&mut suffix);
        }
        deferred.pending.insert(owner, events);
    }
    // Let this pass establish pointer focus before resuming old keyboard input.
    // Fresh typing follows the clicked control through the normal widget path.
    if ctx.input(|input| {
        input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::PointerButton { pressed: true, .. }))
    }) {
        if !deferred.pending.is_empty() || !deferred.after_navigation.is_empty() {
            ctx.request_repaint();
        }
        ctx.data_mut(|data| data.insert_temp(state_id, deferred));
        return;
    }
    let focused = ctx.memory(|memory| memory.focused()).and_then(|id| {
        focused_owner
            .filter(|(owner_id, _)| *owner_id == id)
            .map(|(_, channel)| ComposerEventOwner::new(id, channel))
    });
    if focused.is_none() && deferred.after_navigation.is_empty() {
        ctx.data_mut(|data| data.insert_temp(state_id, deferred));
        return;
    }
    if deferred.after_navigation.is_empty()
        && focused.is_none_or(|owner| !deferred.pending.contains_key(&owner))
    {
        let needs_boundary = ctx.input(|input| {
            let events: Vec<_> = input
                .events
                .iter()
                .filter(|event| is_keyboard_event(event))
                .cloned()
                .collect();
            composer_event_boundary(&events, bindings).0 < events.len()
        });
        if !needs_boundary {
            ctx.data_mut(|data| data.insert_temp(state_id, deferred));
            return;
        }
    }
    let mut events = focused
        .and_then(|owner| deferred.pending.remove(&owner))
        .unwrap_or_default();
    events.append(&mut deferred.after_navigation);
    ctx.input_mut(|input| {
        input.events.retain(|event| {
            if is_keyboard_event(event) {
                events.push(event.clone());
                false
            } else {
                true
            }
        });
    });
    if let Some(owner) = focused {
        let (end, navigation) = composer_event_boundary(&events, bindings);
        let suffix = events.split_off(end);
        if navigation {
            // Bare character shortcuts have a paired Text event. It belongs to
            // the shortcut, not to the control that receives focus afterward.
            if matches!(events.last(), Some(egui::Event::Text(_))) {
                events.pop();
            }
            ctx.input_mut(|input| input.events.extend(events));
            deferred.after_navigation = suffix;
            deferred.editor_states.remove(&owner);
        } else {
            if !events.is_empty() {
                deferred.ready = Some((owner, events));
            }
            if !suffix.is_empty() {
                deferred.pending.insert(owner, suffix);
            }
        }
    } else {
        // Search boxes and other controls own text after a navigation gesture.
        ctx.input_mut(|input| input.events.extend(events));
    }
    if !deferred.after_navigation.is_empty()
        || deferred
            .ready
            .as_ref()
            .is_some_and(|(owner, _)| deferred.pending.contains_key(owner))
    {
        ctx.request_repaint();
    }
    ctx.data_mut(|data| data.insert_temp(state_id, deferred));
}

fn claim_composer_events(ctx: &egui::Context, owner: ComposerEventOwner) -> bool {
    if !ctx.memory(|memory| memory.has_focus(owner.id)) {
        return false;
    }
    let state_id = composer_events_id(ctx);
    let (events, state) = ctx.data_mut(|data| {
        let deferred = data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id);
        if deferred
            .ready
            .as_ref()
            .is_some_and(|(ready_owner, _)| *ready_owner == owner)
        {
            (
                deferred.ready.take().map(|(_, events)| events),
                deferred.editor_states.remove(&owner),
            )
        } else {
            (None, None)
        }
    });
    let Some(events) = events else {
        return false;
    };
    if let Some(state) = state {
        egui::TextEdit::store_state(ctx, owner.id, state);
    }
    ctx.input_mut(|input| input.events.extend(events));
    true
}

fn finish_composer_events(ctx: &egui::Context, owner: ComposerEventOwner) {
    // TextEdit reads text events without consuming them. Do not expose a replay
    // to another control rendered later in the same pass.
    ctx.input_mut(|input| {
        input.events.retain(|event| {
            !is_keyboard_event(event)
                || matches!(
                    event,
                    egui::Event::Key {
                        key: Key::Escape,
                        ..
                    }
                )
        })
    });
    let state = egui::TextEdit::load_state(ctx, owner.id);
    let state_id = composer_events_id(ctx);
    ctx.data_mut(|data| {
        let deferred = data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id);
        if deferred.pending.contains_key(&owner) {
            if let Some(state) = state {
                deferred.editor_states.insert(owner, state);
            }
        } else {
            deferred.editor_states.remove(&owner);
        }
    });
}

fn bind_navigation_events(ctx: &egui::Context, owner: ComposerEventOwner) {
    if !ctx.memory(|memory| memory.has_focus(owner.id)) {
        return;
    }
    let state_id = composer_events_id(ctx);
    if ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id)
            .after_navigation
            .is_empty()
    }) {
        return;
    }
    let editor_state = egui::TextEdit::load_state(ctx, owner.id);
    let bound = ctx.data_mut(|data| {
        let deferred = data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id);
        if deferred.after_navigation.is_empty() {
            return false;
        }
        let events = std::mem::take(&mut deferred.after_navigation);
        let has_pending = deferred.pending.contains_key(&owner);
        deferred.pending.entry(owner).or_default().extend(events);
        if !has_pending {
            if let Some(state) = editor_state {
                deferred.editor_states.insert(owner, state);
            }
        }
        true
    });
    if bound {
        ctx.request_repaint();
    }
}

/// Move queued input with the draft when a pane becomes the classic editor or
/// the classic editor is moved into a split. Hidden drafts keep their queues.
pub(crate) fn migrate_composer_events(
    ctx: &egui::Context,
    old_id: Id,
    new_id: Id,
    channel: &ChannelId,
) {
    if old_id == new_id {
        return;
    }
    let old_owner = ComposerEventOwner::new(old_id, channel);
    let new_owner = ComposerEventOwner::new(new_id, channel);
    let state_id = composer_events_id(ctx);
    ctx.data_mut(|data| {
        let deferred = data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id);
        if let Some(mut events) = deferred.pending.remove(&old_owner) {
            if let Some(mut existing) = deferred.pending.remove(&new_owner) {
                events.append(&mut existing);
            }
            deferred.pending.insert(new_owner, events);
        }
        if let Some(state) = deferred.editor_states.remove(&old_owner) {
            deferred.editor_states.insert(new_owner, state);
        }
        if let Some((owner, _)) = deferred.ready.as_mut() {
            if *owner == old_owner {
                *owner = new_owner;
            }
        }
    });
}

/// Follow a canonical channel rename without changing which editor owns input.
pub(crate) fn rename_composer_event_channel(ctx: &egui::Context, old: &ChannelId, new: &ChannelId) {
    let old_scope = Id::new(old);
    let new_scope = Id::new(new);
    if old_scope == new_scope {
        return;
    }
    let state_id = composer_events_id(ctx);
    ctx.data_mut(|data| {
        let deferred = data.get_temp_mut_or_default::<DeferredComposerEvents>(state_id);
        let owners: Vec<_> = deferred
            .pending
            .keys()
            .copied()
            .filter(|owner| owner.channel == old_scope)
            .collect();
        for owner in owners {
            let target = ComposerEventOwner {
                channel: new_scope,
                ..owner
            };
            if let Some(events) = deferred.pending.remove(&owner) {
                deferred.pending.entry(target).or_default().extend(events);
            }
        }
        let owners: Vec<_> = deferred
            .editor_states
            .keys()
            .copied()
            .filter(|owner| owner.channel == old_scope)
            .collect();
        for owner in owners {
            let target = ComposerEventOwner {
                channel: new_scope,
                ..owner
            };
            if let Some(state) = deferred.editor_states.remove(&owner) {
                deferred.editor_states.entry(target).or_insert(state);
            }
        }
        if let Some((owner, _)) = deferred.ready.as_mut() {
            if owner.channel == old_scope {
                owner.channel = new_scope;
            }
        }
    });
}

pub(crate) fn composer_owns_keyboard(ctx: &egui::Context, id: Id) -> bool {
    let query = id.with("input_history_query");
    ctx.memory(|memory| match memory.focused() {
        Some(focused) => focused == id || focused == query,
        None => memory.had_focus_last_frame(id) || memory.had_focus_last_frame(query),
    }) || completion_pointer_active(ctx, id)
}

#[derive(Clone, Default)]
struct ComposerFocus {
    requested: Option<(Id, u64)>,
    rendered_pass: u64,
    rendered: Vec<Id>,
}

impl ComposerFocus {
    fn begin_pass(&mut self, pass: u64) {
        if self.rendered_pass != pass {
            self.rendered_pass = pass;
            self.rendered.clear();
        }
        // Allow a sizing pass before the requested editor mounts, but do not
        // let a canceled pane steal focus if it is recreated much later.
        if self
            .requested
            .is_some_and(|(_, requested)| pass.saturating_sub(requested) > 2)
        {
            self.requested = None;
        }
    }
}

/// Focus a rendered composer now, or hand the request to its next render.
pub(crate) fn request_composer_focus(ctx: &egui::Context, target: Option<Id>) {
    let pass = ctx.cumulative_pass_nr();
    let focus_now = ctx.data_mut(|data| {
        let state = data.get_temp_mut_or_default::<ComposerFocus>(Id::new("composer_focus"));
        state.begin_pass(pass);
        state.requested = target.map(|id| (id, pass));
        let ready = target.filter(|id| state.rendered.contains(id));
        if ready.is_some() {
            state.requested = None;
        }
        ready
    });
    if let Some(id) = focus_now {
        ctx.memory_mut(|memory| memory.request_focus(id));
    } else {
        // An earlier composer must not consume input intended for an editor
        // that will receive focus later in this pass.
        ctx.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
    }
    ctx.request_repaint();
}

fn prepare_composer_focus(ctx: &egui::Context, id: Id) {
    let pass = ctx.cumulative_pass_nr();
    let requested = ctx.data_mut(|data| {
        let state = data.get_temp_mut_or_default::<ComposerFocus>(Id::new("composer_focus"));
        state.begin_pass(pass);
        if !state.rendered.contains(&id) {
            state.rendered.push(id);
        }
        if state.requested.is_some_and(|(target, _)| target == id) {
            state.requested = None;
            true
        } else {
            false
        }
    });
    if requested {
        ctx.memory_mut(|memory| memory.request_focus(id));
    }
}

struct ComposerLayout {
    picker_width: f32,
    send_width: f32,
    counter_inline: bool,
    show_send: bool,
    controls_width: f32,
    minimum_text_width: f32,
}

impl ComposerLayout {
    fn new(width: f32, has_counter: bool) -> Self {
        let picker_width = (t::chat_font_size() + 16.0).max(t::bar_h());
        let send_width = (58.0 * t::font_scale()).max(58.0);
        let minimum_text_width = 96.0 * t::font_scale();
        let gap = t::TOOLBAR_SPACING.x;
        let counter_width = 70.0 * t::font_scale();
        let counter_inline =
            has_counter && width >= minimum_text_width + picker_width + counter_width + gap * 2.0;
        let controls_width = picker_width
            + gap
            + if counter_inline {
                counter_width + gap
            } else {
                0.0
            };
        let show_send =
            width > 250.0 && width >= minimum_text_width + controls_width + send_width + gap;
        Self {
            picker_width,
            send_width,
            counter_inline,
            show_send,
            controls_width: controls_width + if show_send { send_width + gap } else { 0.0 },
            minimum_text_width,
        }
    }
}

/// Extra tray height when a Twitch draft's counter needs its own row.
pub(crate) fn chat_input_counter_height(
    ctx: &egui::Context,
    outer_width: f32,
    channel: &ChannelId,
    text: &str,
) -> f32 {
    if text.is_empty() || channel.is_irc() || channel.is_kick() {
        return 0.0;
    }
    let inner_width = outer_width - (t::INPUT_MARGIN.left + t::INPUT_MARGIN.right) as f32;
    if ComposerLayout::new(inner_width, true).counter_inline {
        return 0.0;
    }
    ctx.fonts(|fonts| fonts.row_height(&t::tiny())) + ctx.style().spacing.item_spacing.y
}

fn show_character_counter(ui: &mut Ui, count: usize) {
    let color = if count > TWITCH_MAX_CHARS {
        t::red()
    } else if count > 400 {
        t::yellow()
    } else {
        t::text_muted()
    };
    let hint = if count > TWITCH_MAX_CHARS {
        format!(
            "{} characters over Twitch's {TWITCH_MAX_CHARS}-character limit",
            count - TWITCH_MAX_CHARS
        )
    } else {
        format!("Twitch messages are limited to {TWITCH_MAX_CHARS} characters")
    };
    ui.label(
        RichText::new(format!("{count}/{TWITCH_MAX_CHARS}"))
            .font(t::tiny())
            .color(color),
    )
    .on_hover_text(hint);
}

// Persistent state stored in egui temp data

/// Spell-check context menu state preserved across frames while the menu is
/// open.
#[derive(Clone)]
struct SpellCtx {
    channel: ChannelId,
    expected_buf: String,
    word: String,
    start: usize,
    end: usize,
    suggestions: Vec<String>,
}

impl SpellCtx {
    fn matches(&self, channel: &ChannelId, buf: &str) -> bool {
        self.channel == *channel
            && self.expected_buf == buf
            && buf.get(self.start..self.end) == Some(self.word.as_str())
    }

    fn replace(&self, channel: &ChannelId, buf: &mut String, replacement: &str) -> Option<usize> {
        if !self.matches(channel, buf) {
            return None;
        }
        let cursor = buf.get(..self.start)?.chars().count() + replacement.chars().count();
        buf.replace_range(self.start..self.end, replacement);
        Some(cursor)
    }
}

#[derive(Clone)]
struct CompletionPopupState {
    rect: egui::Rect,
    kind: &'static str,
    selected: usize,
    selected_label: String,
}

#[derive(Clone)]
struct DismissedCompletion {
    text: String,
    caret: usize,
}

fn completion_is_dismissed(ctx: &egui::Context, id: Id, text: &str, caret: usize) -> bool {
    let state_id = id.with("dismissed_completion");
    ctx.data_mut(|data| {
        let dismissed = data.get_temp::<DismissedCompletion>(state_id);
        if dismissed.is_some_and(|state| state.text == text && state.caret == caret) {
            true
        } else {
            data.remove::<DismissedCompletion>(state_id);
            false
        }
    })
}

fn completion_pointer_active(ctx: &egui::Context, id: Id) -> bool {
    let popup = ctx.data(|data| data.get_temp::<CompletionPopupState>(id.with("completion_popup")));
    let Some(popup) = popup else {
        return false;
    };
    let pointer = ctx.input(|input| {
        (input.pointer.any_down() || input.pointer.any_released())
            .then(|| input.pointer.interact_pos())
            .flatten()
    });
    pointer.is_some_and(|pos| {
        popup.rect.contains(pos)
            && ctx.layer_id_at(pos)
                == Some(egui::LayerId::new(
                    Order::Foreground,
                    id.with("completion_popup_area"),
                ))
    })
}

enum CompletionEntry<'a> {
    Emote(&'a EmoteCatalogEntry),
    Command(&'a SlashCommandSuggestion),
    Username(&'a str),
    Channel(&'a str),
}

impl CompletionEntry<'_> {
    fn label(&self) -> &str {
        match self {
            Self::Emote(entry) => &entry.code,
            Self::Command(entry) => &entry.usage,
            Self::Username(name) | Self::Channel(name) => name,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Emote(_) => "emotes",
            Self::Command(_) => "commands",
            Self::Username(_) => "users",
            Self::Channel(_) => "channels",
        }
    }
}

fn completion_popup_rect(
    viewport: egui::Rect,
    input: egui::Rect,
    desired_size: Vec2,
) -> egui::Rect {
    let bounds = viewport.shrink(4.0);
    let width = desired_size.x.min(bounds.width()).max(0.0);
    let above = (input.top() - 4.0 - bounds.top()).max(0.0);
    let below = (bounds.bottom() - input.bottom() - 4.0).max(0.0);
    let use_above = above >= below || above >= desired_size.y;
    let height = desired_size.y.min(if use_above { above } else { below });
    let left = input.left().clamp(bounds.left(), bounds.right() - width);
    let top = if use_above {
        input.top() - 4.0 - height
    } else {
        input.bottom() + 4.0
    };
    egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, height))
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

#[derive(Clone)]
struct InputHistorySearch {
    channel: ChannelId,
    original_draft: String,
    entries: Arc<[(String, String)]>,
    query: String,
    matches: Vec<usize>,
    selected: Option<usize>,
    anchor: usize,
    backwards: bool,
    focus_query: bool,
    opened_at: f64,
}

impl InputHistorySearch {
    fn new(
        channel: &ChannelId,
        draft: &str,
        history: &[String],
        anchor: usize,
        backwards: bool,
        opened_at: f64,
    ) -> Self {
        Self {
            channel: channel.clone(),
            original_draft: draft.to_owned(),
            entries: history
                .iter()
                .map(|text| (text.clone(), text.to_lowercase()))
                .collect(),
            query: String::new(),
            matches: Vec::new(),
            selected: None,
            anchor,
            backwards,
            focus_query: true,
            opened_at,
        }
    }

    fn refresh(&mut self) {
        self.matches.clear();
        self.selected = None;
        if self.query.is_empty() {
            return;
        }
        let query = self.query.to_lowercase();
        self.matches.extend(
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(index, (_, text))| text.contains(&query).then_some(index)),
        );
        if !self.matches.is_empty() {
            self.selected = Some(if self.backwards {
                self.matches
                    .iter()
                    .rposition(|index| *index <= self.anchor)
                    .unwrap_or(self.matches.len() - 1)
            } else {
                self.matches
                    .iter()
                    .position(|index| *index >= self.anchor)
                    .unwrap_or(0)
            });
        }
    }

    fn cycle(&mut self, backwards: bool) {
        self.backwards = backwards;
        if let Some(selected) = self.selected {
            let count = self.matches.len();
            self.selected = Some(if backwards {
                (selected + count - 1) % count
            } else {
                (selected + 1) % count
            });
        }
    }

    fn selected_text(&self) -> Option<&str> {
        self.selected
            .and_then(|selected| self.matches.get(selected))
            .and_then(|index| self.entries.get(*index))
            .map(|(text, _)| text.as_str())
    }
}

fn begin_input_history_search(
    ctx: &egui::Context,
    id: Id,
    channel: &ChannelId,
    draft: &str,
    history: &[String],
    backwards: bool,
) {
    let recalled = ctx.data(|data| data.get_temp::<HistState>(id.with("msg_history_state")));
    let anchor = recalled
        .filter(|state| state.idx >= 0 && state.expected_buf == draft)
        .map_or(history.len(), |state| {
            history.len().saturating_sub(state.idx as usize + 1)
        });
    let opened_at = ctx.input(|input| input.time);
    ctx.data_mut(|data| {
        data.insert_temp(
            id.with("input_history_search"),
            InputHistorySearch::new(channel, draft, history, anchor, backwards, opened_at),
        )
    });
    invalidate_emote_completions(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id.with("input_history_query")));
    ctx.request_repaint();
}

/// Chat input bar shown at the bottom of the message area.
pub struct ChatInput<'a> {
    /// Explicit identity keeps keyboard focus independent of the pane's layout.
    pub id: Option<Id>,
    /// Shortcuts apply only while this composer owns keyboard focus.
    pub hotkey_bindings: &'a HotkeyBindings,
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
    /// Whether this composer's emote picker is open.
    pub emote_picker_open: bool,
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
    fn consume_action(&self, input: &mut egui::InputState, action: HotkeyAction) -> bool {
        consume_composer_binding(input, &self.hotkey_bindings.get(action))
    }

    fn consume_send(&self, input: &mut egui::InputState, keep: bool) -> Option<SendShortcut> {
        let action = if keep {
            HotkeyAction::InputSendKeep
        } else {
            HotkeyAction::InputSend
        };
        let binding = self.hotkey_bindings.get(action);
        if consume_composer_binding(input, &binding) {
            return Some(SendShortcut::Primary);
        }
        // Shift+Enter follows the default Enter binding. A custom binding or
        // an unbound action has no hidden Enter shortcut.
        if let Some(alternative) = self.hotkey_bindings.shifted_send_binding(action) {
            if consume_composer_binding(input, &alternative) {
                return Some(SendShortcut::Shifted);
            }
        }
        None
    }

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
        prepare_composer_focus(ui.ctx(), text_edit_id);
        let event_owner = ComposerEventOwner::new(text_edit_id, self.channel);
        let claimed_events = claim_composer_events(ui.ctx(), event_owner);
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
            ui.data_mut(|data| {
                data.remove::<HistState>(text_edit_id.with("msg_history_state"));
                data.remove::<SpellCtx>(text_edit_id.with("spell_check_ctx"));
                data.remove::<InputHistorySearch>(text_edit_id.with("input_history_search"));
            });
            ui.memory_mut(|memory| {
                if memory.has_focus(text_edit_id.with("input_history_query")) {
                    memory.request_focus(text_edit_id);
                }
            });
        }
        let popup_pointer_active = completion_pointer_active(ui.ctx(), text_edit_id);
        let mut history_search_active = ui.data(|data| {
            data.get_temp::<InputHistorySearch>(text_edit_id.with("input_history_search"))
                .is_some()
        });
        let composer_had_focus = ui.memory(|m| m.has_focus(text_edit_id)) || popup_pointer_active;
        if composer_had_focus && !history_search_active {
            let direction = ui.input_mut(|input| {
                if self.consume_action(input, HotkeyAction::InputHistorySearchNext) {
                    Some(false)
                } else if self.consume_action(input, HotkeyAction::InputHistorySearchPrev) {
                    Some(true)
                } else {
                    None
                }
            });
            if let Some(backwards) = direction {
                begin_input_history_search(
                    ui.ctx(),
                    text_edit_id,
                    self.channel,
                    buf,
                    self.message_history,
                    backwards,
                );
                history_search_active = true;
            }
        }
        let input_had_focus = composer_had_focus && !history_search_active;
        let mut retain_keyboard_focus = popup_pointer_active && !history_search_active;
        if input_had_focus {
            let clear = ui.input_mut(|input| self.consume_action(input, HotkeyAction::InputClear));
            if clear {
                buf.clear();
                invalidate_emote_completions(ui.ctx(), text_edit_id);
                ui.data_mut(|data| {
                    data.remove::<HistState>(text_edit_id.with("msg_history_state"))
                });
                move_cursor_to_end(ui.ctx(), text_edit_id, 0);
                retain_keyboard_focus = true;
            }
            result.toggle_emote_picker =
                ui.input_mut(|input| self.consume_action(input, HotkeyAction::InputEmotePicker));
        }
        let owns_reply_shortcut = composer_had_focus
            || ui.memory(|memory| {
                memory.focused().is_none() && memory.had_focus_last_frame(text_edit_id)
            });
        let completion_open = ui.data(|data| {
            data.get_temp::<CompletionPopupState>(text_edit_id.with("completion_popup"))
                .is_some()
        });
        if owns_reply_shortcut
            && !history_search_active
            && !completion_open
            && self.pending_reply.is_some()
            && !ui.ctx().is_context_menu_open()
            && ui.input_mut(|input| self.consume_action(input, HotkeyAction::InputCancelReply))
        {
            result.dismiss_reply = true;
            retain_keyboard_focus = true;
        }

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
                let mut counter_below = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = t::TOOLBAR_SPACING;
                    let input_width = ui.available_width();
                    let is_twitch_channel = !self.channel.is_irc() && !self.channel.is_kick();
                    let show_counter = is_twitch_channel && !buf.is_empty();
                    let layout = ComposerLayout::new(input_width, show_counter);
                    let input_h = (t::chat_font_size() + 16.0).max(t::bar_h());
                    if input_width > 300.0 {
                        if let Some(name) = self.username.filter(|name| !name.trim().is_empty()) {
                            let label = format!("{name}:");
                            let label_width = ui.fonts(|fonts| {
                                fonts
                                    .layout_no_wrap(label.clone(), t::small(), t::accent())
                                    .size()
                                    .x
                            });
                            if input_width
                                - layout.controls_width
                                - label_width
                                - t::TOOLBAR_SPACING.x
                                >= layout.minimum_text_width
                            {
                                // Reserve the same height as the editor so the
                                // username stays vertically centered.
                                let (label_rect, label_response) = ui.allocate_exact_size(
                                    egui::vec2(label_width, input_h),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    label_rect.left_center(),
                                    egui::Align2::LEFT_CENTER,
                                    label,
                                    t::small(),
                                    t::accent(),
                                );
                                label_response.on_hover_text(format!("Chatting as {name}"));
                            }
                        }
                    }

                    // Queries stop at the caret, so editing an earlier word leaves the suffix intact.
                    let pre_cursor = cursor_byte_index(ui.ctx(), text_edit_id, buf);
                    let pre_query = &buf[..pre_cursor];
                    let popup_was_open = ui.data(|data| {
                        data.get_temp::<CompletionPopupState>(text_edit_id.with("completion_popup"))
                            .is_some()
                    });
                    // egui clears current focus on Escape before widgets run.
                    // The still-visible popup belongs to the prior composer.
                    let completion_owned_focus = input_had_focus
                        || ui.memory(|memory| {
                            memory.focused().is_none() && memory.had_focus_last_frame(text_edit_id)
                        });
                    if completion_owned_focus
                        && popup_was_open
                        && !ui.ctx().is_context_menu_open()
                        && ui.input_mut(|input| {
                            crate::app::consume_exact_key(input, egui::Modifiers::NONE, Key::Escape)
                        })
                    {
                        ui.data_mut(|data| {
                            data.insert_temp(
                                text_edit_id.with("dismissed_completion"),
                                DismissedCompletion {
                                    text: buf.clone(),
                                    caret: pre_cursor,
                                },
                            )
                        });
                        retain_keyboard_focus = true;
                    }
                    let completions_allowed =
                        !completion_is_dismissed(ui.ctx(), text_edit_id, buf, pre_cursor);
                    // Pre-check autocompletes before TextEdit consumes keys.
                    let pre_emote_matches = if input_had_focus && completions_allowed {
                        find_autocomplete_matches(pre_query, self.emote_catalog)
                    } else {
                        Vec::new()
                    };
                    let pre_username_matches =
                        if input_had_focus && completions_allowed && pre_emote_matches.is_empty() {
                            find_username_matches(pre_query, self.chatters)
                        } else {
                            Vec::new()
                        };
                    let pre_slash_matches = if input_had_focus
                        && completions_allowed
                        && pre_emote_matches.is_empty()
                        && pre_username_matches.is_empty()
                    {
                        find_slash_matches(pre_query, self.slash_usage_counts)
                    } else {
                        Vec::new()
                    };
                    let pre_join_matches = if input_had_focus
                        && completions_allowed
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
                    let mut send_pressed = false;
                    let mut send_keep_pressed = false;
                    let mut unused_enter = false;

                    if input_had_focus {
                        ui.input_mut(|i| {
                            reverse_tab = self.consume_action(i, HotkeyAction::InputCompletePrev);
                            consumed_tab = reverse_tab
                                || self.consume_action(i, HotkeyAction::InputCompleteNext);
                            consumed_up = if autocomplete_active {
                                crate::app::consume_exact_key(
                                    i,
                                    egui::Modifiers::NONE,
                                    Key::ArrowUp,
                                )
                            } else {
                                self.consume_action(i, HotkeyAction::InputHistoryPrev)
                            };
                            consumed_down = if autocomplete_active {
                                crate::app::consume_exact_key(
                                    i,
                                    egui::Modifiers::NONE,
                                    Key::ArrowDown,
                                )
                            } else {
                                self.consume_action(i, HotkeyAction::InputHistoryNext)
                            };
                            send_keep_pressed = self.consume_send(i, true).is_some();
                            let send = self.consume_send(i, false);
                            consumed_enter =
                                matches!(send, Some(SendShortcut::Primary)) && autocomplete_active;
                            send_pressed = send.is_some() && !consumed_enter;
                            // Enter has no other operation in a single-line
                            // composer after its send binding is removed.
                            i.events.retain(|event| {
                                let enter = matches!(
                                    event,
                                    egui::Event::Key {
                                        key: Key::Enter,
                                        pressed: true,
                                        ..
                                    }
                                );
                                unused_enter |= enter;
                                !enter
                            });
                        });
                    }
                    retain_keyboard_focus |= consumed_tab
                        || consumed_enter
                        || consumed_up
                        || consumed_down
                        || send_pressed
                        || send_keep_pressed
                        || unused_enter;

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
                    let text_width = (ui.available_width() - layout.controls_width).max(40.0);
                    // Input box grows with chat font so text never clips.
                    let te_output = ui
                        .allocate_ui(egui::vec2(text_width, input_h), |ui| {
                            egui::TextEdit::singleline(buf)
                                .id(text_edit_id)
                                .interactive(!history_search_active)
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
                    if resp.changed() {
                        ac_sel = 0;
                    }

                    // Recompute matches after TextEdit may have changed buf.
                    // Emote autocomplete takes priority, then @username, then slash, then /join.
                    let input_active = resp.has_focus() || input_had_focus;
                    let cursor_byte = cursor_byte_index(ui.ctx(), text_edit_id, buf);
                    let query = &buf[..cursor_byte];
                    let completions_allowed =
                        !completion_is_dismissed(ui.ctx(), text_edit_id, buf, cursor_byte);
                    let reuse_matches =
                        input_had_focus && !resp.changed() && cursor_byte == pre_cursor;
                    let matches = if !input_active || !completions_allowed {
                        Vec::new()
                    } else if reuse_matches {
                        pre_emote_matches
                    } else {
                        find_autocomplete_matches(query, self.emote_catalog)
                    };
                    let username_matches =
                        if input_active && completions_allowed && matches.is_empty() {
                            if reuse_matches {
                                pre_username_matches
                            } else {
                                find_username_matches(query, self.chatters)
                            }
                        } else {
                            Vec::new()
                        };
                    let slash_matches = if input_active
                        && completions_allowed
                        && matches.is_empty()
                        && username_matches.is_empty()
                    {
                        if reuse_matches {
                            pre_slash_matches
                        } else {
                            find_slash_matches(query, self.slash_usage_counts)
                        }
                    } else {
                        Vec::new()
                    };
                    let join_matches = if input_active
                        && completions_allowed
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
                    let can_submit = !history_search_active
                        && !twitch_over_limit
                        && !buf.trim().is_empty()
                        && (self.logged_in || is_slash_input);

                    if send_pressed || send_keep_pressed {
                        resp.request_focus();
                        if can_submit {
                            result.send = Some(if send_keep_pressed {
                                buf.clone()
                            } else {
                                std::mem::take(buf)
                            });
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(hist_id, HistState::default()));
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(tab_id, TabState::default()));
                        }
                    }

                    let picker = ui
                        .push_id(text_edit_id.with("emote_picker_button"), |ui| {
                            super::chrome::icon_button(
                                ui,
                                super::chrome::ChromeIcon::Emotes,
                                "Emote picker",
                                super::chrome::IconButtonState {
                                    selected: self.emote_picker_open,
                                    toggle_state: Some(self.emote_picker_open),
                                    ..Default::default()
                                },
                            )
                        })
                        .inner;
                    if picker.clicked() {
                        result.toggle_emote_picker = true;
                    }

                    // Send button - hidden at very narrow widths
                    if layout.show_send {
                        let send_btn = ui.add_enabled(
                            can_submit,
                            egui::Button::new(RichText::new("Send").font(t::body()))
                                .min_size(egui::vec2(layout.send_width, layout.picker_width)),
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

                    if show_counter && !buf.is_empty() {
                        if layout.counter_inline {
                            show_character_counter(ui, twitch_char_count);
                        } else {
                            counter_below = Some(twitch_char_count);
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
                            || popup_pointer_active
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
                    } else {
                        ui.data_mut(|data| {
                            data.remove::<CompletionPopupState>(
                                text_edit_id.with("completion_popup"),
                            );
                        });
                    }

                    // Spell-check context menu (right-click)
                    let sc_id = text_edit_id.with("spell_check_ctx");

                    // TextEdit places the caret at the secondary click before
                    // this handler computes suggestions for that word.
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
                                        channel: self.channel.clone(),
                                        expected_buf: buf.clone(),
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

                    ui.ctx().data_mut(|data| {
                        if data
                            .get_temp::<SpellCtx>(sc_id)
                            .is_some_and(|state| !state.matches(self.channel, buf))
                        {
                            data.remove::<SpellCtx>(sc_id);
                        }
                    });
                    let mut spell_replace: Option<(SpellCtx, String)> = None;
                    let mut spell_add_word: Option<String> = None;
                    let mut context_send_now = false;
                    let mut context_cut_input = false;
                    let mut context_copy_selection = false;
                    let mut context_copy_input = false;
                    let mut context_paste_input = false;
                    let mut context_select_all = false;
                    let mut context_clear_input = false;
                    let mut context_insert_help = false;
                    let mut context_search_history = false;
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

                        let history_binding = self
                            .hotkey_bindings
                            .get(HotkeyAction::InputHistorySearchPrev);
                        let history_label = if history_binding.is_unbound() {
                            "Search input history".to_owned()
                        } else {
                            format!("Search input history ({})", history_binding.display_label())
                        };
                        if ui
                            .button(RichText::new(history_label).font(t::small()))
                            .clicked()
                        {
                            context_search_history = true;
                            ui.close_menu();
                        }

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
                                        spell_replace = Some((sc.clone(), sug.clone()));
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

                    if context_search_history {
                        begin_input_history_search(
                            ui.ctx(),
                            text_edit_id,
                            self.channel,
                            buf,
                            self.message_history,
                            true,
                        );
                        retain_keyboard_focus = false;
                    }

                    if let Some((spelling, replacement)) = spell_replace {
                        if let Some(cursor) = spelling.replace(self.channel, buf, &replacement) {
                            move_cursor_to_end(ui.ctx(), text_edit_id, cursor);
                            ui.ctx().memory_mut(|m| m.request_focus(text_edit_id));
                        }
                        ui.ctx().data_mut(|data| data.remove::<SpellCtx>(sc_id));
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
                if let Some(count) = counter_below {
                    show_character_counter(ui, count);
                }
            });

        retain_keyboard_focus |= self.show_input_history_search(ui, text_edit_id, buf);

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

        if claimed_events {
            finish_composer_events(ui.ctx(), event_owner);
        }
        bind_navigation_events(ui.ctx(), event_owner);
        result
    }

    fn show_input_history_search(&self, ui: &mut Ui, id: Id, buf: &mut String) -> bool {
        let state_id = id.with("input_history_search");
        let Some(mut search) = ui.data(|data| data.get_temp::<InputHistorySearch>(state_id)) else {
            return false;
        };
        if search.channel != *self.channel || search.original_draft != *buf {
            ui.data_mut(|data| data.remove::<InputHistorySearch>(state_id));
            return false;
        }
        let query_id = id.with("input_history_query");
        let just_opened = search.opened_at == ui.input(|input| input.time);
        let owns_keys = ui.memory(|memory| {
            memory.has_focus(query_id)
                || (memory.focused().is_none() && memory.had_focus_last_frame(query_id))
        });
        let mut accept = false;
        let mut cancel = false;
        let mut direction = None;
        if owns_keys {
            ui.input_mut(|input| {
                cancel = crate::app::consume_exact_key(input, egui::Modifiers::NONE, Key::Escape);
                accept = crate::app::consume_exact_key(input, egui::Modifiers::NONE, Key::Enter)
                    || crate::app::consume_exact_key(input, egui::Modifiers::NONE, Key::Tab)
                    || crate::app::consume_exact_key(input, egui::Modifiers::SHIFT, Key::Tab);
                if self.consume_action(input, HotkeyAction::InputHistorySearchNext)
                    || crate::app::consume_exact_key(input, egui::Modifiers::NONE, Key::ArrowDown)
                {
                    direction = Some(false);
                } else if self.consume_action(input, HotkeyAction::InputHistorySearchPrev)
                    || crate::app::consume_exact_key(input, egui::Modifiers::NONE, Key::ArrowUp)
                {
                    direction = Some(true);
                }
            });
        }

        let viewport = ui.ctx().screen_rect().shrink(8.0);
        let input_rect = ui
            .ctx()
            .read_response(id)
            .map_or(ui.min_rect(), |response| response.rect);
        let width = input_rect.width().clamp(260.0, 520.0).min(viewport.width());
        let position = egui::pos2(
            input_rect.left().clamp(
                viewport.left(),
                (viewport.right() - width).max(viewport.left()),
            ),
            (input_rect.top() - 210.0).max(viewport.top()),
        );
        let mut open = true;
        let shown = egui::Window::new("Search input history")
            .id(id.with("input_history_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_pos(position)
            .default_width(width)
            .min_width(0.0)
            .max_width(viewport.width())
            .max_height(viewport.height())
            .show(ui.ctx(), |ui| {
                let query = ui.add(
                    egui::TextEdit::singleline(&mut search.query)
                        .id(query_id)
                        .hint_text("Search sent messages...")
                        .desired_width(f32::INFINITY),
                );
                if search.focus_query {
                    query.request_focus();
                    search.focus_query = false;
                }
                if query.changed() {
                    search.refresh();
                }
                if let Some(backwards) = direction {
                    search.cycle(backwards);
                }
                let status = if search.query.is_empty() {
                    "Type to search sent messages".to_owned()
                } else if let Some(selected) = search.selected {
                    format!(
                        "Match {} of {}",
                        search.matches.len() - selected,
                        search.matches.len()
                    )
                } else {
                    "No matches".to_owned()
                };
                let status_color = if !search.query.is_empty() && search.selected.is_none() {
                    t::red()
                } else {
                    t::text_secondary()
                };
                ui.label(RichText::new(status).font(t::small()).color(status_color));
                egui::ScrollArea::vertical()
                    .id_salt(id.with("input_history_preview"))
                    .max_height(80.0 * t::font_scale())
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(search.selected_text().unwrap_or("")).wrap());
                    });
                ui.horizontal_wrapped(|ui| {
                    let has_match = search.selected.is_some();
                    let older = self
                        .hotkey_bindings
                        .get(HotkeyAction::InputHistorySearchPrev);
                    let newer = self
                        .hotkey_bindings
                        .get(HotkeyAction::InputHistorySearchNext);
                    if ui
                        .add_enabled(has_match, egui::Button::new("Older"))
                        .on_hover_text(if older.is_unbound() {
                            "Up".to_owned()
                        } else {
                            format!("{} or Up", older.display_label())
                        })
                        .clicked()
                    {
                        search.cycle(true);
                        query.request_focus();
                    }
                    if ui
                        .add_enabled(has_match, egui::Button::new("Newer"))
                        .on_hover_text(if newer.is_unbound() {
                            "Down".to_owned()
                        } else {
                            format!("{} or Down", newer.display_label())
                        })
                        .clicked()
                    {
                        search.cycle(false);
                        query.request_focus();
                    }
                    accept |= ui
                        .add_enabled(has_match, egui::Button::new("Use message"))
                        .clicked();
                    cancel |= ui.button("Cancel").clicked();
                });
                ui.label(
                    RichText::new("Enter uses the message; Escape keeps your draft.")
                        .font(t::tiny())
                        .color(t::text_helper()),
                );
                if ui.memory(|memory| memory.has_focus(query_id)) {
                    ui.memory_mut(|memory| {
                        memory.set_focus_lock_filter(
                            query_id,
                            egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                ..Default::default()
                            },
                        )
                    });
                }
            });

        let clicked_elsewhere =
            !just_opened && shown.is_some_and(|window| window.response.clicked_elsewhere());
        let focus_moved = !just_opened
            && ui.memory(|memory| memory.focused().is_some_and(|focused| focused != query_id));
        let selected = accept
            .then(|| search.selected_text().map(str::to_owned))
            .flatten();
        if cancel || !open || clicked_elsewhere || focus_moved || selected.is_some() {
            if !cancel && open && !clicked_elsewhere {
                if let Some(selected) = selected {
                    *buf = selected;
                    let cursor = buf.chars().count();
                    move_cursor_to_end(ui.ctx(), id, cursor);
                    invalidate_emote_completions(ui.ctx(), id);
                    ui.data_mut(|data| {
                        data.insert_temp(
                            id.with("dismissed_completion"),
                            DismissedCompletion {
                                text: buf.clone(),
                                caret: buf.len(),
                            },
                        );
                        let idx = self
                            .message_history
                            .iter()
                            .rposition(|text| text.as_str() == buf.as_str())
                            .map_or(-1, |index| (self.message_history.len() - index - 1) as i32);
                        data.insert_temp(
                            id.with("msg_history_state"),
                            HistState {
                                idx,
                                saved_input: search.original_draft.clone(),
                                expected_buf: buf.clone(),
                            },
                        );
                    });
                }
            }
            ui.data_mut(|data| data.remove::<InputHistorySearch>(state_id));
            ui.ctx().request_repaint();
            return !clicked_elsewhere && (!focus_moved || cancel || accept || !open);
        }
        if accept {
            ui.memory_mut(|memory| {
                memory.request_focus(query_id);
                memory.set_focus_lock_filter(
                    query_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
        }
        ui.data_mut(|data| data.insert_temp(state_id, search));
        false
    }

    fn show_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[&EmoteCatalogEntry],
        selected: i32,
    ) -> Option<String> {
        let entries: Vec<_> = matches
            .iter()
            .copied()
            .map(CompletionEntry::Emote)
            .collect();
        self.show_completion_popup(ui, text_resp, &entries, selected)
            .map(|index| matches[index].code.clone())
    }

    fn show_slash_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[SlashCommandSuggestion],
        selected: i32,
    ) -> Option<String> {
        let entries: Vec<_> = matches.iter().map(CompletionEntry::Command).collect();
        self.show_completion_popup(ui, text_resp, &entries, selected)
            .map(|index| matches[index].name.to_owned())
    }

    fn show_join_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[String],
        selected: i32,
    ) -> Option<String> {
        let entries: Vec<_> = matches
            .iter()
            .map(|name| CompletionEntry::Channel(name))
            .collect();
        self.show_completion_popup(ui, text_resp, &entries, selected)
            .map(|index| matches[index].clone())
    }

    fn show_username_autocomplete_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        matches: &[String],
        selected: i32,
    ) -> Option<String> {
        let entries: Vec<_> = matches
            .iter()
            .map(|name| CompletionEntry::Username(name))
            .collect();
        self.show_completion_popup(ui, text_resp, &entries, selected)
            .map(|index| matches[index].clone())
    }

    fn show_completion_popup(
        &self,
        ui: &mut Ui,
        text_resp: &egui::Response,
        entries: &[CompletionEntry<'_>],
        selected: i32,
    ) -> Option<usize> {
        let first = entries.first()?;
        let selected = selected.clamp(0, entries.len() as i32 - 1) as usize;
        let font_height = ui.fonts(|fonts| fonts.row_height(&t::small()));
        let row_height = if matches!(first, CompletionEntry::Command(_)) {
            font_height * 2.0 + 10.0
        } else {
            font_height.max(AUTOCOMPLETE_EMOTE_SIZE) + 8.0
        };
        let rows = entries.len().min(AUTOCOMPLETE_MAX);
        // TextEdit returns its inner text rectangle; anchor outside the full framed widget.
        let input_rect = ui
            .ctx()
            .read_response(text_resp.id)
            .map_or(text_resp.rect, |response| response.rect);
        let popup_rect = completion_popup_rect(
            ui.ctx().screen_rect(),
            input_rect,
            egui::vec2(
                input_rect.width().clamp(240.0, 460.0),
                rows as f32 * (row_height + 2.0) + 6.0,
            ),
        );
        let state_id = text_resp.id.with("completion_popup");
        if popup_rect.width() <= 8.0 || popup_rect.height() <= 8.0 {
            ui.data_mut(|data| data.remove::<CompletionPopupState>(state_id));
            return None;
        }
        let popup_id = text_resp.id.with("completion_popup_area");
        let previous = ui.data(|data| data.get_temp::<CompletionPopupState>(state_id));
        let scroll_to_selection = text_resp.changed()
            || previous.as_ref().is_none_or(|state| {
                state.kind != first.kind()
                    || state.selected != selected
                    || state.selected_label != entries[selected].label()
                    || state.rect.size() != popup_rect.size()
            });
        ui.data_mut(|data| {
            data.insert_temp(
                state_id,
                CompletionPopupState {
                    rect: popup_rect,
                    kind: first.kind(),
                    selected,
                    selected_label: entries[selected].label().to_owned(),
                },
            );
        });
        let mut clicked = None;
        egui::Area::new(popup_id)
            .order(Order::Foreground)
            .fixed_pos(popup_rect.min)
            .default_size(popup_rect.size())
            .movable(false)
            .constrain(false)
            .sense(egui::Sense::hover())
            .show(ui.ctx(), |ui| {
                ui.set_min_size(popup_rect.size());
                ui.set_max_size(popup_rect.size());
                ui.painter().rect_filled(popup_rect, 6.0, t::bg_raised());
                ui.painter().rect_stroke(
                    popup_rect,
                    6.0,
                    Stroke::new(1.0_f32, t::border_subtle()),
                    egui::StrokeKind::Inside,
                );
                let mut body = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(popup_rect.shrink(4.0))
                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                );
                body.set_clip_rect(popup_rect.shrink(4.0));
                body.spacing_mut().item_spacing.y = 2.0;
                egui::ScrollArea::vertical()
                    .id_salt(popup_id.with("rows"))
                    .max_height(popup_rect.height() - 8.0)
                    .animated(false)
                    .auto_shrink([false; 2])
                    .show(&mut body, |ui| {
                        for (index, entry) in entries.iter().enumerate() {
                            let is_selected = index == selected;
                            let (row_rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), row_height),
                                egui::Sense::hover(),
                            );
                            let hovered = ui.rect_contains_pointer(row_rect);
                            if is_selected {
                                ui.painter()
                                    .rect_filled(row_rect, 3.0, t::active_channel_bg());
                                if scroll_to_selection {
                                    ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                                }
                            } else if hovered {
                                ui.painter().rect_filled(row_rect, 3.0, t::hover_row_bg());
                            }
                            if ui.is_rect_visible(row_rect) {
                                let mut row = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(row_rect.shrink2(egui::vec2(6.0, 4.0)))
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                row.set_clip_rect(ui.clip_rect().intersect(row_rect));
                                row.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);
                                self.paint_completion_entry(&mut row, entry, is_selected, hovered);
                            }
                            let response = ui.interact(
                                row_rect,
                                popup_id.with(first.kind()).with(index),
                                egui::Sense::click(),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    ui.is_enabled(),
                                    is_selected,
                                    entry.label(),
                                )
                            });
                            if response.clicked() {
                                clicked = Some(index);
                            }
                        }
                    });
            });
        clicked
    }

    fn paint_completion_entry(
        &self,
        ui: &mut Ui,
        entry: &CompletionEntry<'_>,
        selected: bool,
        hovered: bool,
    ) {
        let color = if selected {
            t::text_primary()
        } else {
            t::text_secondary()
        };
        match entry {
            CompletionEntry::Emote(entry) => {
                let (slot, _) = ui.allocate_exact_size(
                    egui::vec2(AUTOCOMPLETE_EMOTE_SIZE, AUTOCOMPLETE_EMOTE_SIZE),
                    egui::Sense::hover(),
                );
                if let Some(&(w, h, ref raw)) = self.emote_bytes.get(entry.url.as_str()) {
                    let is_animation = super::message_list::is_animation_format(raw);
                    let animated = is_animation && self.animate_emotes && (selected || hovered);
                    let size = fit_size(w, h, AUTOCOMPLETE_EMOTE_SIZE);
                    let image_rect = egui::Rect::from_center_size(slot.center(), size);
                    if is_animation && !animated {
                        if let Some(texture) = self.emote_bytes.static_texture(ui.ctx(), &entry.url)
                        {
                            ui.painter().image(
                                texture.id(),
                                image_rect,
                                egui::Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                    } else {
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
                let provider = provider_label(&entry.provider);
                let provider_width = ui.fonts(|fonts| {
                    fonts
                        .layout_no_wrap(provider.to_owned(), t::small(), color)
                        .size()
                        .x
                });
                let code_width =
                    (ui.available_width() - provider_width - ui.spacing().item_spacing.x).max(0.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(code_width, ui.available_height()),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&entry.code).font(t::small()).color(color),
                            )
                            .truncate(),
                        );
                    },
                );
                ui.label(RichText::new(provider).font(t::small()).color(color));
            }
            CompletionEntry::Command(entry) => {
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&entry.usage)
                                .font(t::small())
                                .color(color)
                                .strong(),
                        )
                        .truncate(),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(&entry.summary).font(t::small()).color(color),
                        )
                        .truncate(),
                    );
                });
            }
            CompletionEntry::Username(name) => {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("@{name}"))
                            .font(t::small())
                            .color(color),
                    )
                    .truncate(),
                );
            }
            CompletionEntry::Channel(name) => {
                ui.add(
                    egui::Label::new(RichText::new(*name).font(t::small()).color(color)).truncate(),
                );
            }
        }
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

    ranked_completion_matches(catalog, query, AUTOCOMPLETE_MAX, |entry| &entry.code)
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

    ranked_completion_matches(chatters, query, USERNAME_AUTOCOMPLETE_MAX, String::as_str)
        .into_iter()
        .cloned()
        .collect()
}

fn ranked_completion_matches<'a, T>(
    candidates: &'a [T],
    query: &str,
    limit: usize,
    name: impl Fn(&T) -> &str,
) -> Vec<&'a T> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let mut matches: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let name = name(candidate);
            let prefix = if name.is_ascii() && query.is_ascii() {
                let query = query.as_bytes();
                if !name
                    .as_bytes()
                    .windows(query.len())
                    .any(|part| part.eq_ignore_ascii_case(query))
                {
                    return None;
                }
                name.as_bytes()
                    .get(..query.len())
                    .is_some_and(|part| part.eq_ignore_ascii_case(query))
            } else {
                let lower = name.to_lowercase();
                if !lower.contains(&query) {
                    return None;
                }
                lower.starts_with(&query)
            };
            Some(((!prefix, name.len(), name, index), candidate))
        })
        .collect();
    // Keep only the visible candidates before sorting; the original index breaks equal-name ties.
    if matches.len() > limit {
        matches.select_nth_unstable_by(limit, |left, right| left.0.cmp(&right.0));
        matches.truncate(limit);
    }
    matches.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    matches
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
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
            if ch.is_irc_query() {
                return None;
            }
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
        data.remove::<CompletionPopupState>(id.with("completion_popup"));
        data.remove::<DismissedCompletion>(id.with("dismissed_completion"));
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
    request_composer_focus(ctx, Some(id));
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

#[cfg(test)]
std::thread_local! {
    static CLIPBOARD_IMAGE_OVERRIDE: std::cell::RefCell<Option<Option<PendingUpload>>> =
        const { std::cell::RefCell::new(None) };
}

/// Keep synthetic paste events independent of the desktop clipboard.
#[cfg(test)]
pub(crate) fn with_test_clipboard_image<R>(
    image: Option<PendingUpload>,
    run: impl FnOnce() -> R,
) -> R {
    struct RestoreClipboardImage(Option<Option<PendingUpload>>);

    impl Drop for RestoreClipboardImage {
        fn drop(&mut self) {
            CLIPBOARD_IMAGE_OVERRIDE.with(|slot| slot.replace(self.0.take()));
        }
    }

    let _restore =
        RestoreClipboardImage(CLIPBOARD_IMAGE_OVERRIDE.with(|slot| slot.replace(Some(image))));
    run()
}

/// Read an image from the OS clipboard and re-encode as PNG.
/// Returns `None` if the clipboard is empty, unavailable, or holds no image.
///
/// Retries briefly on Windows because egui-winit's own `arboard` instance may
/// hold the clipboard open for a few milliseconds while it tries its
/// text-paste attempt; a cold retry after a short sleep usually succeeds.
fn try_clipboard_image_as_png() -> Option<PendingUpload> {
    #[cfg(test)]
    if let Some(image) = CLIPBOARD_IMAGE_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return image;
    }

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
            hotkey_bindings: &HotkeyBindings::defaults(),
            id: None,
            channel: &ChannelId::new("example"),
            logged_in: true,
            username: None,
            emote_catalog: catalog,
            emote_bytes: &UiImageCache::default(),
            emote_picker_open: false,
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

    fn narrow_composer_frame(
        ctx: &egui::Context,
        buf: &mut String,
        width: f32,
        events: Vec<egui::Event>,
        focus: bool,
    ) -> (egui::FullOutput, ChatInputResult, egui::Rect, egui::Rect) {
        let mut result = None;
        let mut field = egui::Rect::NOTHING;
        let mut tray = egui::Rect::NOTHING;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(width, 200.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let height = (t::chat_font_size() + 16.0).max(t::bar_h())
                            + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom) as f32
                            + chat_input_counter_height(
                                ctx,
                                width,
                                &ChannelId::new("example"),
                                buf,
                            );
                        egui::TopBottomPanel::bottom("test_composer_tray")
                            .exact_height(height)
                            .frame(egui::Frame::NONE)
                            .show_inside(ui, |ui| {
                                tray = ui.max_rect();
                                ui.set_clip_rect(tray);
                                let id = ui.id().with("chat_input");
                                result = Some(show_test_input(ui, buf, &[], focus, false));
                                field = ctx.read_response(id).unwrap().rect;
                            });
                    });
            },
        );
        (output, result.unwrap(), field, tray)
    }

    fn visible_input_text(output: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            let egui::Shape::Text(text) = &shape.shape else {
                return None;
            };
            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
            (text.galley.text() == label && shape.clip_rect.contains_rect(rect)).then_some(rect)
        })
    }

    #[test]
    fn username_label_is_vertically_centered_on_the_editor() {
        let ctx = egui::Context::default();
        let mut buf = String::new();
        let mut editor = egui::Rect::NOTHING;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(640.0, 220.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let id = Id::new("aligned_username_composer");
                    ChatInput {
                        hotkey_bindings: &HotkeyBindings::defaults(),
                        id: Some(id),
                        channel: &ChannelId::new("example"),
                        logged_in: true,
                        username: Some("tester"),
                        emote_catalog: &[],
                        emote_bytes: &UiImageCache::default(),
                        emote_picker_open: false,
                        pending_reply: None,
                        message_history: &[],
                        slash_usage_counts: &HashMap::new(),
                        known_channels: &[],
                        chatters: &[],
                        prevent_overlong_twitch_messages: false,
                        animate_emotes: false,
                        accept_dropped_files: false,
                    }
                    .show(ui, &mut buf);
                    editor = ctx.read_response(id).expect("editor exists").rect;
                });
            },
        );
        let label = visible_input_text(&output, "tester:").expect("username is drawn");
        assert!(
            (label.center().y - editor.center().y).abs() <= 1.0,
            "username center {} differs from editor center {}",
            label.center().y,
            editor.center().y,
        );
    }

    fn completion_menu_frame(
        ctx: &egui::Context,
        buf: &mut String,
        viewport: Vec2,
        events: Vec<egui::Event>,
        focus: bool,
    ) -> (egui::FullOutput, ChatInputResult) {
        let id = Id::new("completion_menu_test");
        let catalog: Vec<_> = (0..10)
            .map(|index| EmoteCatalogEntry {
                id: index.to_string(),
                code: format!("Kappa{index:02}"),
                url: format!("https://example.test/{index}.png"),
                url_hd: None,
                provider: "test".to_owned(),
                scope: "global".to_owned(),
                channel: None,
                user_id: None,
                zero_width: false,
            })
            .collect();
        let chatters: Vec<_> = (0..10).map(|index| format!("alice{index:02}")).collect();
        let channel = if buf.starts_with("/join ") {
            ChannelId::irc("irc.example.test", 6697, true, "room")
        } else {
            ChannelId::new("example")
        };
        let known: Vec<_> = (0..10)
            .map(|index| ChannelId::irc("irc.example.test", 6697, true, &format!("room{index}")))
            .collect();
        let mut result = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, viewport)),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let height = ui
                            .fonts(|fonts| fonts.row_height(&t::body()))
                            .max(t::chat_font_size())
                            + 16.0
                            + (t::INPUT_MARGIN.top + t::INPUT_MARGIN.bottom) as f32
                            + chat_input_counter_height(ctx, viewport.x, &channel, buf);
                        egui::TopBottomPanel::bottom("completion_menu_tray")
                            .exact_height(height)
                            .frame(egui::Frame::NONE)
                            .show_inside(ui, |ui| {
                                if focus {
                                    ui.memory_mut(|memory| memory.request_focus(id));
                                }
                                result = Some(
                                    ChatInput {
                                        hotkey_bindings: &HotkeyBindings::defaults(),
                                        id: Some(id),
                                        channel: &channel,
                                        logged_in: true,
                                        username: None,
                                        emote_catalog: &catalog,
                                        emote_bytes: &UiImageCache::default(),
                                        emote_picker_open: false,
                                        pending_reply: None,
                                        message_history: &[],
                                        slash_usage_counts: &HashMap::new(),
                                        known_channels: &known,
                                        chatters: &chatters,
                                        prevent_overlong_twitch_messages: false,
                                        animate_emotes: false,
                                        accept_dropped_files: false,
                                    }
                                    .show(ui, buf),
                                );
                            });
                    });
            },
        );
        (output, result.unwrap())
    }

    fn completion_popup(ctx: &egui::Context) -> CompletionPopupState {
        ctx.data(|data| {
            data.get_temp::<CompletionPopupState>(
                Id::new("completion_menu_test").with("completion_popup"),
            )
        })
        .expect("the focused query should have a completion menu")
    }

    #[test]
    fn escape_dismisses_completions_until_the_draft_changes_and_keeps_focus() {
        for (initial, typed) in [(":Kap", "p"), ("@al", "i"), ("/", "m"), ("/join #r", "o")] {
            let ctx = egui::Context::default();
            let id = Id::new("completion_menu_test");
            let mut buf = initial.to_owned();
            let size = egui::vec2(480.0, 320.0);
            for _ in 0..3 {
                completion_menu_frame(&ctx, &mut buf, size, Vec::new(), true);
            }
            completion_popup(&ctx);
            let (_, action) = completion_menu_frame(
                &ctx,
                &mut buf,
                size,
                vec![key(Key::Escape, true, egui::Modifiers::NONE)],
                false,
            );
            assert!(action.send.is_none());
            assert_eq!(buf, initial);
            assert!(ctx.memory(|memory| memory.has_focus(id)));
            for _ in 0..3 {
                completion_menu_frame(&ctx, &mut buf, size, Vec::new(), false);
                assert!(
                    ctx.data(
                        |data| data.get_temp::<CompletionPopupState>(id.with("completion_popup"))
                    )
                    .is_none(),
                    "Escape must keep {initial:?} dismissed without changing the draft"
                );
            }
            completion_menu_frame(
                &ctx,
                &mut buf,
                size,
                vec![egui::Event::Text(typed.to_owned())],
                false,
            );
            assert_eq!(buf, format!("{initial}{typed}"));
            assert_eq!(completion_popup(&ctx).selected, 0);
        }
    }

    #[test]
    fn changing_the_completion_filter_selects_the_first_match() {
        let ctx = egui::Context::default();
        let mut buf = "@al".to_owned();
        let size = egui::vec2(480.0, 320.0);
        for _ in 0..3 {
            completion_menu_frame(&ctx, &mut buf, size, Vec::new(), true);
        }
        for _ in 0..5 {
            completion_menu_frame(
                &ctx,
                &mut buf,
                size,
                vec![key(Key::ArrowDown, true, egui::Modifiers::NONE)],
                false,
            );
        }
        assert_eq!(completion_popup(&ctx).selected, 5);
        completion_menu_frame(
            &ctx,
            &mut buf,
            size,
            vec![egui::Event::Text("i".to_owned())],
            false,
        );
        assert_eq!(completion_popup(&ctx).selected, 0);
        assert_eq!(completion_popup(&ctx).selected_label, "alice00");
    }

    #[test]
    fn covered_completion_popups_do_not_retain_composer_pointer_focus() {
        let ctx = egui::Context::default();
        let id = Id::new("completion_menu_test");
        let mut buf = "@al".to_owned();
        let size = egui::vec2(480.0, 320.0);
        for _ in 0..3 {
            completion_menu_frame(&ctx, &mut buf, size, Vec::new(), true);
        }
        let popup = completion_popup(&ctx);
        let position = popup.rect.center();
        for pressed in [false, true] {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, size)),
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    egui::Area::new(id.with("completion_popup_area"))
                        .order(Order::Foreground)
                        .fixed_pos(popup.rect.min)
                        .show(ctx, |ui| {
                            ui.allocate_space(popup.rect.size());
                        });
                    egui::Area::new(Id::new("covering_control"))
                        .order(Order::Tooltip)
                        .fixed_pos(popup.rect.min)
                        .show(ctx, |ui| {
                            ui.add_sized(popup.rect.size(), egui::Button::new("Cover"));
                        });
                    if pressed {
                        assert!(
                            !completion_pointer_active(ctx, id),
                            "a covered popup must not steal the pointer from the top layer"
                        );
                    }
                },
            );
        }
    }

    #[test]
    fn completion_menus_fit_short_viewports_and_accept_pointer_selection() {
        for size in [egui::vec2(480.0, 320.0), egui::vec2(320.0, 220.0)] {
            for font_scale in [1.0, 1.7] {
                for initial in [":ka", "@al", "/", "/join #r"] {
                    let ctx = egui::Context::default();
                    let mut fonts = egui::FontDefinitions::default();
                    for font in fonts.font_data.values_mut() {
                        Arc::make_mut(font).tweak.scale *= font_scale;
                    }
                    ctx.set_fonts(fonts);
                    let mut buf = initial.to_owned();
                    let mut rendered = egui::FullOutput::default();
                    for frame in 0..3 {
                        rendered =
                            completion_menu_frame(&ctx, &mut buf, size, Vec::new(), frame == 0).0;
                    }
                    let popup = completion_popup(&ctx);
                    assert!(
                        egui::Rect::from_min_size(Pos2::ZERO, size).contains_rect(popup.rect),
                        "{initial} menu {:?} must fit {size:?} at {font_scale}",
                        popup.rect
                    );
                    let composer = ctx
                        .read_response(Id::new("completion_menu_test"))
                        .unwrap()
                        .rect;
                    let area = ctx
                        .memory(|memory| {
                            memory.area_rect(
                                Id::new("completion_menu_test").with("completion_popup_area"),
                            )
                        })
                        .unwrap();
                    assert!(
                        !area.intersects(composer),
                        "menu {area:?} must not cover composer {composer:?}"
                    );
                    assert!(egui::Rect::from_min_size(Pos2::ZERO, size).contains_rect(area));
                    let label = if initial == "@al" {
                        format!("@{}", popup.selected_label)
                    } else {
                        popup.selected_label.clone()
                    };
                    let row = visible_input_text(&rendered, &label)
                        .unwrap_or_else(|| {
                            let rows: Vec<_> = rendered.shapes.iter().filter_map(|shape| match &shape.shape {
                                egui::Shape::Text(text) if text.galley.text() == label =>
                                    Some((text.pos, text.galley.size(), shape.clip_rect)),
                                _ => None,
                            }).collect();
                            panic!("selected completion {label} must be visible at {size:?}/{font_scale}: {rows:?}");
                        });
                    let expected = match initial {
                        "@al" => format!("@{} ", popup.selected_label),
                        "/" => format!(
                            "{} ",
                            popup.selected_label.split_whitespace().next().unwrap()
                        ),
                        "/join #r" => format!("/join {} ", popup.selected_label),
                        _ => format!("{} ", popup.selected_label),
                    };
                    for pressed in [true, false] {
                        let (_, result) = completion_menu_frame(
                            &ctx,
                            &mut buf,
                            size,
                            vec![
                                egui::Event::PointerMoved(row.center()),
                                egui::Event::PointerButton {
                                    pos: row.center(),
                                    button: egui::PointerButton::Primary,
                                    pressed,
                                    modifiers: egui::Modifiers::NONE,
                                },
                            ],
                            false,
                        );
                        assert!(result.send.is_none());
                    }
                    assert_eq!(
                        buf, expected,
                        "clicking {initial} completion should replace the token"
                    );
                    assert!(ctx.memory(|memory| memory.has_focus(Id::new("completion_menu_test"))));
                    completion_menu_frame(
                        &ctx,
                        &mut buf,
                        size,
                        vec![egui::Event::Text("next".to_owned())],
                        false,
                    );
                    assert_eq!(
                        buf,
                        format!("{expected}next"),
                        "typing should return to the composer"
                    );
                }
            }
        }
    }

    #[test]
    fn completion_menu_wheel_scroll_stays_put_until_keyboard_selection_changes() {
        let ctx = egui::Context::default();
        let mut buf = "/".to_owned();
        let size = egui::vec2(480.0, 320.0);
        for frame in 0..3 {
            completion_menu_frame(&ctx, &mut buf, size, Vec::new(), frame == 0);
        }
        let first = completion_popup(&ctx);
        completion_menu_frame(
            &ctx,
            &mut buf,
            size,
            vec![
                egui::Event::PointerMoved(first.rect.center()),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -180.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            false,
        );
        let mut output = egui::FullOutput::default();
        for _ in 0..8 {
            output = completion_menu_frame(&ctx, &mut buf, size, Vec::new(), false).0;
        }
        assert!(
            visible_input_text(&output, &first.selected_label).is_none(),
            "wheel scrolling must not snap back to the selected first row"
        );
        assert_eq!(completion_popup(&ctx).selected, 0);
        completion_menu_frame(
            &ctx,
            &mut buf,
            size,
            vec![key(Key::ArrowDown, true, egui::Modifiers::NONE)],
            false,
        );
        for _ in 0..8 {
            output = completion_menu_frame(&ctx, &mut buf, size, Vec::new(), false).0;
        }
        let selected = completion_popup(&ctx);
        assert_eq!(selected.selected, 1);
        assert!(
            visible_input_text(&output, &selected.selected_label).is_some(),
            "keyboard movement should reveal its selected row"
        );
    }

    #[test]
    fn changing_completion_query_reveals_an_unchanged_first_match() {
        let ctx = egui::Context::default();
        let mut buf = "@al".to_owned();
        let size = egui::vec2(480.0, 220.0);
        for frame in 0..3 {
            completion_menu_frame(&ctx, &mut buf, size, Vec::new(), frame == 0);
        }
        let first = completion_popup(&ctx);
        completion_menu_frame(
            &ctx,
            &mut buf,
            size,
            vec![
                egui::Event::PointerMoved(first.rect.center()),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -180.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            false,
        );
        let mut output = egui::FullOutput::default();
        for _ in 0..8 {
            output = completion_menu_frame(&ctx, &mut buf, size, Vec::new(), false).0;
        }
        let label = format!("@{}", first.selected_label);
        assert!(visible_input_text(&output, &label).is_none());
        completion_menu_frame(
            &ctx,
            &mut buf,
            size,
            vec![egui::Event::Text("i".into())],
            false,
        );
        for _ in 0..3 {
            output = completion_menu_frame(&ctx, &mut buf, size, Vec::new(), false).0;
        }
        assert_eq!(buf, "@ali");
        assert_eq!(completion_popup(&ctx).selected_label, first.selected_label);
        assert!(visible_input_text(&output, &label).is_some(),
            "editing the filter should reveal its selected completion even when the first match stays the same");
    }

    #[test]
    fn completion_ranking_preserves_unicode_and_stable_ties_with_large_catalogs() {
        let mut names: Vec<_> = (0..20_000)
            .rev()
            .map(|index| format!("item{index:05}"))
            .collect();
        names.extend([
            "xitem".to_owned(),
            "ITEM".to_owned(),
            "item".to_owned(),
            "item".to_owned(),
        ]);
        let matches = ranked_completion_matches(&names, "ItEm", 10, String::as_str);
        assert_eq!(
            &matches[..3],
            [&names[20_001], &names[20_002], &names[20_003]]
        );
        assert!(matches[3..].iter().all(|name| name.starts_with("item000")));
        assert_eq!(matches.len(), 10);
        let unicode = vec!["Émile".to_owned(), "émil".to_owned(), "unémile".to_owned()];
        assert_eq!(
            ranked_completion_matches(&unicode, "ÉMI", 10, String::as_str),
            [&unicode[1], &unicode[0], &unicode[2]]
        );
    }

    #[test]
    fn narrow_composer_keeps_the_picker_clickable_and_counters_inside_the_tray() {
        for width in [150.0, 200.0, 250.0, 360.0] {
            for initial in [
                String::new(),
                "hello".to_owned(),
                "é".repeat(400),
                "🙂".repeat(501),
            ] {
                let ctx = egui::Context::default();
                let mut buf = initial.clone();
                let mut output = egui::FullOutput::default();
                for _ in 0..3 {
                    let frame = narrow_composer_frame(&ctx, &mut buf, width, Vec::new(), false);
                    output = frame.0;
                    assert!(
                        frame.3.contains_rect(frame.2),
                        "text field {:?} must fit tray {:?} at {width}",
                        frame.2,
                        frame.3
                    );
                }
                let picker = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Circle(circle) if circle.radius >= 5.0 => Some(circle.center),
                        _ => None,
                    })
                    .find(|point| {
                        output
                            .shapes
                            .iter()
                            .any(|shape| shape.clip_rect.contains(*point))
                    })
                    .expect("the picker remains visible at every supported pane width");
                if !initial.is_empty() {
                    let counter_label = format!("{}/500", initial.chars().count());
                    let counter =
                        visible_input_text(&output, &counter_label).unwrap_or_else(|| {
                            panic!("counter {counter_label} must be fully visible at width {width}")
                        });
                    if width <= 200.0 {
                        assert!(
                            counter.top() > picker.y,
                            "narrow counters belong below the input row"
                        );
                    }
                }
                if width <= 200.0 {
                    assert!(visible_input_text(&output, "Send").is_none());
                }
                let mut toggled = false;
                for pressed in [true, false] {
                    let frame = narrow_composer_frame(
                        &ctx,
                        &mut buf,
                        width,
                        vec![
                            egui::Event::PointerMoved(picker),
                            egui::Event::PointerButton {
                                pos: picker,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        false,
                    );
                    toggled |= frame.1.toggle_emote_picker;
                    assert!(frame.1.send.is_none());
                }
                assert!(toggled, "picker should open at width {width}");
                assert_eq!(
                    buf, initial,
                    "opening the picker preserves the complete draft"
                );
            }
        }
    }

    #[test]
    fn narrow_composer_enter_submits_valid_text_and_preserves_over_limit_drafts() {
        for initial in ["hello".to_owned(), "🙂".repeat(501)] {
            let ctx = egui::Context::default();
            let mut buf = initial.clone();
            for _ in 0..3 {
                narrow_composer_frame(&ctx, &mut buf, 150.0, Vec::new(), true);
            }
            let (_, result, _, _) = narrow_composer_frame(
                &ctx,
                &mut buf,
                150.0,
                vec![key(Key::Enter, true, egui::Modifiers::NONE)],
                true,
            );
            if initial.chars().count() <= TWITCH_MAX_CHARS {
                assert_eq!(result.send, Some(initial));
                assert!(buf.is_empty());
            } else {
                assert!(result.send.is_none());
                assert_eq!(buf, initial);
            }
        }
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
                ..Default::default()
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
                                hotkey_bindings: &HotkeyBindings::defaults(),
                                id: None,
                                channel: &ChannelId::new("example"),
                                logged_in: true,
                                username: None,
                                emote_catalog: &[],
                                emote_bytes: &UiImageCache::default(),
                                emote_picker_open: false,
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
        completion_frame(
            &ctx,
            &mut buf,
            vec![egui::Event::Text("more ".to_owned())],
            None,
        );
        assert_eq!(buf, "hé Kappa more tail🙂");
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
                    zero_width: false,
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
                            hotkey_bindings: &HotkeyBindings::defaults(),
                            id: Some(id),
                            channel,
                            logged_in: true,
                            username: None,
                            emote_catalog: catalog,
                            emote_bytes: &UiImageCache::default(),
                            emote_picker_open: false,
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
                zero_width: false,
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

    fn input_history_search_frame(
        ctx: &egui::Context,
        channel: &ChannelId,
        buf: &mut String,
        history: &[String],
        events: Vec<egui::Event>,
        focus: bool,
    ) -> ChatInputResult {
        let mut result = None;
        let _ = ctx.run(raw_input(events), |ctx| {
            egui::TopBottomPanel::bottom("history_search_test_input").show(ctx, |ui| {
                let id = Id::new("history_search_test");
                if focus {
                    ui.memory_mut(|memory| memory.request_focus(id));
                }
                result = Some(
                    ChatInput {
                        hotkey_bindings: &HotkeyBindings::defaults(),
                        id: Some(id),
                        channel,
                        logged_in: true,
                        username: None,
                        emote_catalog: &[],
                        emote_bytes: &UiImageCache::default(),
                        emote_picker_open: false,
                        pending_reply: None,
                        message_history: history,
                        slash_usage_counts: &HashMap::new(),
                        known_channels: &[],
                        chatters: &[],
                        prevent_overlong_twitch_messages: false,
                        animate_emotes: false,
                        accept_dropped_files: false,
                    }
                    .show(ui, buf),
                );
            });
        });
        result.unwrap()
    }

    fn input_history_search_state(ctx: &egui::Context) -> Option<InputHistorySearch> {
        ctx.data(|data| data.get_temp(Id::new("history_search_test").with("input_history_search")))
    }

    fn configured_composer_frame(
        ctx: &egui::Context,
        buf: &mut String,
        bindings: &HotkeyBindings,
        events: Vec<egui::Event>,
        focus: bool,
    ) -> ChatInputResult {
        owned_composer_frame(
            ctx,
            Id::new("configured_composer"),
            &ChannelId::new("example"),
            buf,
            bindings,
            events,
            focus,
        )
    }

    fn show_configured_composer(
        ui: &mut Ui,
        id: Id,
        channel: &ChannelId,
        buf: &mut String,
        bindings: &HotkeyBindings,
    ) -> ChatInputResult {
        ChatInput {
            id: Some(id),
            hotkey_bindings: bindings,
            channel,
            logged_in: true,
            username: None,
            emote_catalog: &[],
            emote_bytes: &UiImageCache::default(),
            emote_picker_open: false,
            pending_reply: None,
            message_history: &["older message".to_owned(), "newer message".to_owned()],
            slash_usage_counts: &HashMap::new(),
            known_channels: &[],
            chatters: &[],
            prevent_overlong_twitch_messages: false,
            animate_emotes: false,
            accept_dropped_files: false,
        }
        .show(ui, buf)
    }

    fn owned_composer_frame(
        ctx: &egui::Context,
        id: Id,
        channel: &ChannelId,
        buf: &mut String,
        bindings: &HotkeyBindings,
        events: Vec<egui::Event>,
        focus: bool,
    ) -> ChatInputResult {
        account_composer_frame(
            ctx,
            id,
            channel,
            buf,
            bindings,
            events,
            focus,
            Some("first-account"),
        )
    }

    fn account_composer_frame(
        ctx: &egui::Context,
        id: Id,
        channel: &ChannelId,
        buf: &mut String,
        bindings: &HotkeyBindings,
        events: Vec<egui::Event>,
        focus: bool,
        account_id: Option<&str>,
    ) -> ChatInputResult {
        let mut result = None;
        let _ = ctx.run(raw_input(events), |ctx| {
            if focus {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
            prepare_composer_events(ctx, bindings, Some((id, channel)), account_id);
            egui::TopBottomPanel::bottom("configured_composer_panel").show(ctx, |ui| {
                result = Some(show_configured_composer(ui, id, channel, buf, bindings));
            });
        });
        result.unwrap()
    }

    fn split_composer_frame(
        ctx: &egui::Context,
        drafts: &mut [String; 2],
        bindings: &HotkeyBindings,
        events: Vec<egui::Event>,
        focus_before: Option<usize>,
        focus_after: Option<usize>,
    ) -> Vec<(usize, String)> {
        let ids = [Id::new(("event_owner", 0)), Id::new(("event_owner", 1))];
        let channels = [ChannelId::new("first"), ChannelId::new("second")];
        let mut sent = Vec::new();
        let _ = ctx.run(raw_input(events), |ctx| {
            if let Some(index) = focus_before {
                ctx.memory_mut(|memory| memory.request_focus(ids[index]));
            }
            let focused = ctx
                .memory(|memory| memory.focused())
                .and_then(|id| ids.iter().position(|candidate| *candidate == id));
            let owner = focused.map(|index| (ids[index], &channels[index]));
            prepare_composer_events(ctx, bindings, owner, Some("first-account"));
            // Repeated preparation in the same pass must not duplicate a replay.
            prepare_composer_events(ctx, bindings, owner, Some("first-account"));
            if let Some(index) = focus_after {
                ctx.memory_mut(|memory| memory.request_focus(ids[index]));
            }
            if ctx.input_mut(|input| {
                crate::app::consume_binding(input, &bindings.get(HotkeyAction::SplitFocusNext))
            }) {
                request_composer_focus(ctx, Some(ids[(focused.unwrap_or(0) + 1) % 2]));
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                for index in 0..2 {
                    let result = show_configured_composer(
                        ui,
                        ids[index],
                        &channels[index],
                        &mut drafts[index],
                        bindings,
                    );
                    if let Some(text) = result.send {
                        sent.push((index, text));
                    }
                }
            });
        });
        sent
    }

    #[test]
    fn configured_send_replaces_enter_and_can_be_unbound() {
        for binding in [
            crust_core::KeyBinding::new("F2"),
            crust_core::KeyBinding::default(),
        ] {
            let ctx = egui::Context::default();
            let mut bindings = HotkeyBindings::defaults();
            bindings.set(HotkeyAction::InputSend, binding.clone());
            let mut buf = "draft to send".to_owned();
            configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
            for modifiers in [egui::Modifiers::NONE, egui::Modifiers::SHIFT] {
                let result = configured_composer_frame(
                    &ctx,
                    &mut buf,
                    &bindings,
                    vec![key(Key::Enter, true, modifiers)],
                    true,
                );
                assert!(result.send.is_none());
                assert_eq!(buf, "draft to send");
            }
            let result = configured_composer_frame(
                &ctx,
                &mut buf,
                &bindings,
                vec![key(Key::F2, true, egui::Modifiers::NONE)],
                true,
            );
            if binding.is_unbound() {
                assert!(result.send.is_none());
                assert_eq!(buf, "draft to send");
            } else {
                assert_eq!(result.send.as_deref(), Some("draft to send"));
                assert!(buf.is_empty());
            }
        }
    }

    #[test]
    fn send_keep_preserves_text_and_rebinding_removes_ctrl_enter() {
        let ctx = egui::Context::default();
        let mut bindings = HotkeyBindings::defaults();
        let mut buf = "send this twice".to_owned();
        configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![key(Key::Enter, true, egui::Modifiers::CTRL)],
            false,
        );
        assert_eq!(result.send.as_deref(), Some("send this twice"));
        assert_eq!(buf, "send this twice");
        bindings.set(
            HotkeyAction::InputSendKeep,
            crust_core::KeyBinding::new("F3"),
        );
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![key(Key::Enter, true, egui::Modifiers::CTRL)],
            true,
        );
        assert!(result.send.is_none());
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![key(Key::F3, true, egui::Modifiers::NONE)],
            true,
        );
        assert_eq!(result.send.as_deref(), Some("send this twice"));
        assert_eq!(buf, "send this twice");
    }

    #[test]
    fn shift_enter_sends_the_draft_while_enter_accepts_a_visible_completion() {
        for shifted in [false, true] {
            let ctx = egui::Context::default();
            let bindings = HotkeyBindings::defaults();
            let mut buf = "/he".to_owned();
            configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
            let modifiers = if shifted {
                egui::Modifiers::SHIFT
            } else {
                egui::Modifiers::NONE
            };
            let result = configured_composer_frame(
                &ctx,
                &mut buf,
                &bindings,
                vec![key(Key::Enter, true, modifiers)],
                false,
            );
            if shifted {
                assert_eq!(result.send.as_deref(), Some("/he"));
                assert!(buf.is_empty());
            } else {
                assert!(result.send.is_none());
                assert_eq!(buf.trim(), "/help");
            }
        }
    }

    #[test]
    fn configured_history_and_clear_preserve_focus_and_draft() {
        let ctx = egui::Context::default();
        let mut bindings = HotkeyBindings::defaults();
        bindings.set(
            HotkeyAction::InputHistoryPrev,
            crust_core::KeyBinding::new("F6"),
        );
        bindings.set(
            HotkeyAction::InputHistoryNext,
            crust_core::KeyBinding::new("F7"),
        );
        bindings.set(HotkeyAction::InputClear, crust_core::KeyBinding::new("F8"));
        let mut buf = "unfinished draft".to_owned();
        configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
        for (pressed, expected) in [
            (Key::ArrowUp, "unfinished draft"),
            (Key::F6, "newer message"),
            (Key::F6, "older message"),
            (Key::F7, "newer message"),
            (Key::F7, "unfinished draft"),
            (Key::F8, ""),
            (Key::F6, "newer message"),
            (Key::F7, ""),
        ] {
            let result = configured_composer_frame(
                &ctx,
                &mut buf,
                &bindings,
                vec![key(pressed, true, egui::Modifiers::NONE)],
                false,
            );
            assert!(result.send.is_none());
            assert_eq!(buf, expected);
            assert!(ctx.memory(|memory| memory.has_focus(Id::new("configured_composer"))));
        }
    }

    #[test]
    fn configured_composer_shortcuts_leave_unfocused_controls_alone() {
        let ctx = egui::Context::default();
        let mut bindings = HotkeyBindings::defaults();
        bindings.set(HotkeyAction::InputSend, crust_core::KeyBinding::new("F2"));
        bindings.set(
            HotkeyAction::InputEmotePicker,
            crust_core::KeyBinding::new("F9"),
        );
        let mut buf = "draft".to_owned();
        configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), false);
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![
                key(Key::F2, true, egui::Modifiers::NONE),
                key(Key::F9, true, egui::Modifiers::NONE),
            ],
            false,
        );
        assert!(result.send.is_none());
        assert!(!result.toggle_emote_picker);
        assert_eq!(buf, "draft");
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![key(Key::F9, true, egui::Modifiers::NONE)],
            true,
        );
        assert!(result.toggle_emote_picker);
        assert_eq!(buf, "draft");
    }

    #[test]
    fn printable_input_shortcuts_consume_their_text_and_preserve_other_typing() {
        let ctx = egui::Context::default();
        let mut bindings = HotkeyBindings::defaults();
        bindings.set(HotkeyAction::InputClear, crust_core::KeyBinding::new("U"));
        bindings.set(HotkeyAction::InputSend, crust_core::KeyBinding::new("S"));
        let mut buf = "old draft".to_owned();
        configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![
                key(Key::U, true, egui::Modifiers::NONE),
                egui::Event::Text("u".to_owned()),
                key(Key::A, true, egui::Modifiers::NONE),
                egui::Event::Text("a".to_owned()),
            ],
            false,
        );
        assert!(result.send.is_none());
        assert_eq!(buf, "a");
        let result = configured_composer_frame(
            &ctx,
            &mut buf,
            &bindings,
            vec![
                key(Key::S, true, egui::Modifiers::NONE),
                egui::Event::Text("s".to_owned()),
            ],
            false,
        );
        assert_eq!(result.send.as_deref(), Some("a"));
        assert!(buf.is_empty());
    }

    #[test]
    fn typing_after_send_edits_the_next_draft_in_event_order() {
        for keep in [false, true] {
            let ctx = egui::Context::default();
            let bindings = HotkeyBindings::defaults();
            let mut buf = "draft".to_owned();
            configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
            move_cursor_to_end(&ctx, Id::new("configured_composer"), buf.chars().count());
            let modifiers = if keep {
                egui::Modifiers::CTRL
            } else {
                egui::Modifiers::NONE
            };
            let result = configured_composer_frame(
                &ctx,
                &mut buf,
                &bindings,
                vec![
                    egui::Event::Text(" before".to_owned()),
                    key(Key::Enter, true, modifiers),
                    key(Key::Enter, false, modifiers),
                    key(Key::A, true, egui::Modifiers::NONE),
                    egui::Event::Text(" after".to_owned()),
                ],
                false,
            );
            assert_eq!(result.send.as_deref(), Some("draft before"));
            assert_eq!(buf, if keep { "draft before" } else { "" });
            let result = configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), false);
            assert!(result.send.is_none());
            assert_eq!(buf, if keep { "draft before after" } else { " after" });
            configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), false);
            assert_eq!(
                buf,
                if keep { "draft before after" } else { " after" },
                "queued text is applied once"
            );
        }
    }

    #[test]
    fn clear_preserves_the_order_of_typing_and_send() {
        for printable in [false, true] {
            let ctx = egui::Context::default();
            let mut bindings = HotkeyBindings::defaults();
            let (clear_key, clear_modifiers, send_key) = if printable {
                bindings.set(HotkeyAction::InputClear, crust_core::KeyBinding::new("U"));
                bindings.set(HotkeyAction::InputSend, crust_core::KeyBinding::new("S"));
                (Key::U, egui::Modifiers::NONE, Key::S)
            } else {
                (Key::U, egui::Modifiers::CTRL, Key::Enter)
            };
            let mut buf = "old draft".to_owned();
            configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), true);
            move_cursor_to_end(&ctx, Id::new("configured_composer"), buf.chars().count());
            let mut events = vec![
                egui::Event::Text(" before clear".to_owned()),
                key(clear_key, true, clear_modifiers),
            ];
            if printable {
                events.push(egui::Event::Text("u".to_owned()));
            }
            events.extend([
                key(Key::A, true, egui::Modifiers::NONE),
                egui::Event::Text("after clear".to_owned()),
                key(send_key, true, egui::Modifiers::NONE),
            ]);
            if printable {
                events.push(egui::Event::Text("s".to_owned()));
            }
            assert!(
                configured_composer_frame(&ctx, &mut buf, &bindings, events, false)
                    .send
                    .is_none()
            );
            let sent = configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), false);
            assert_eq!(sent.send.as_deref(), Some("after clear"));
            assert!(buf.is_empty());
            assert!(
                configured_composer_frame(&ctx, &mut buf, &bindings, Vec::new(), false)
                    .send
                    .is_none()
            );
        }
    }

    #[test]
    fn queued_typing_stays_on_each_side_of_a_pane_navigation_shortcut() {
        for navigation_arrives_later in [false, true] {
            let ctx = egui::Context::default();
            let bindings = HotkeyBindings::defaults();
            let mut drafts = ["first draft".to_owned(), "second draft".to_owned()];
            split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), Some(0), None);
            for index in 0..2 {
                move_cursor_to_end(
                    &ctx,
                    Id::new(("event_owner", index)),
                    drafts[index].chars().count(),
                );
            }
            let mut events = vec![
                key(Key::Enter, true, egui::Modifiers::NONE),
                egui::Event::Text("next first".to_owned()),
            ];
            let mut navigation = vec![
                key(
                    Key::PageDown,
                    true,
                    egui::Modifiers::CTRL | egui::Modifiers::ALT,
                ),
                egui::Event::Text(" plus second".to_owned()),
                key(Key::Enter, true, egui::Modifiers::NONE),
            ];
            if !navigation_arrives_later {
                events.append(&mut navigation);
            }
            let mut sent = split_composer_frame(&ctx, &mut drafts, &bindings, events, None, None);
            assert_eq!(sent, vec![(0, "first draft".to_owned())]);
            sent.extend(split_composer_frame(
                &ctx,
                &mut drafts,
                &bindings,
                navigation,
                None,
                None,
            ));
            assert_eq!(drafts, ["next first", "second draft"]);
            for _ in 0..4 {
                sent.extend(split_composer_frame(
                    &ctx,
                    &mut drafts,
                    &bindings,
                    Vec::new(),
                    None,
                    None,
                ));
            }
            assert_eq!(
                sent,
                vec![
                    (0, "first draft".to_owned()),
                    (1, "second draft plus second".to_owned())
                ]
            );
            assert_eq!(drafts, ["next first", ""]);
        }
    }

    #[test]
    fn focus_change_after_preparation_retains_the_original_draft_input() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let mut drafts = ["sent first".to_owned(), "second draft".to_owned()];
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), Some(0), None);
        let sent = split_composer_frame(
            &ctx,
            &mut drafts,
            &bindings,
            vec![
                key(Key::Enter, true, egui::Modifiers::NONE),
                egui::Event::Text("next first".to_owned()),
            ],
            None,
            None,
        );
        assert_eq!(sent, vec![(0, "sent first".to_owned())]);
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), None, Some(1));
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), None, None);
        assert_eq!(drafts, ["", "second draft"]);
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), Some(0), None);
        assert_eq!(drafts, ["next first", "second draft"]);
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), None, None);
        assert_eq!(drafts, ["next first", "second draft"]);
    }

    #[test]
    fn text_after_navigation_keeps_its_target_when_focus_moves_again() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let mut drafts = ["first draft".to_owned(), "second draft".to_owned()];
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), Some(0), None);
        for index in 0..2 {
            move_cursor_to_end(
                &ctx,
                Id::new(("event_owner", index)),
                drafts[index].chars().count(),
            );
        }
        split_composer_frame(
            &ctx,
            &mut drafts,
            &bindings,
            vec![
                key(
                    Key::PageDown,
                    true,
                    egui::Modifiers::CTRL | egui::Modifiers::ALT,
                ),
                egui::Event::Text(" after navigation".to_owned()),
            ],
            None,
            None,
        );
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), Some(0), None);
        assert_eq!(drafts, ["first draft", "second draft"]);
        split_composer_frame(&ctx, &mut drafts, &bindings, Vec::new(), Some(1), None);
        assert_eq!(drafts, ["first draft", "second draft after navigation"]);
    }

    #[test]
    fn reused_composer_id_keeps_hidden_channel_input_until_its_draft_returns() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let id = Id::new("reused_composer");
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let mut first_draft = "first message".to_owned();
        let mut second_draft = "second draft".to_owned();
        owned_composer_frame(
            &ctx,
            id,
            &first,
            &mut first_draft,
            &bindings,
            Vec::new(),
            true,
        );
        let sent = owned_composer_frame(
            &ctx,
            id,
            &first,
            &mut first_draft,
            &bindings,
            vec![
                key(Key::Enter, true, egui::Modifiers::NONE),
                egui::Event::Text("next first".to_owned()),
            ],
            false,
        );
        assert_eq!(sent.send.as_deref(), Some("first message"));
        owned_composer_frame(
            &ctx,
            id,
            &second,
            &mut second_draft,
            &bindings,
            Vec::new(),
            false,
        );
        move_cursor_to_end(&ctx, id, second_draft.chars().count());
        owned_composer_frame(
            &ctx,
            id,
            &second,
            &mut second_draft,
            &bindings,
            vec![egui::Event::Text(" edited".to_owned())],
            false,
        );
        assert!(first_draft.is_empty());
        assert_eq!(second_draft, "second draft edited");
        owned_composer_frame(
            &ctx,
            id,
            &first,
            &mut first_draft,
            &bindings,
            Vec::new(),
            false,
        );
        assert_eq!(first_draft, "next first");
        assert_eq!(second_draft, "second draft edited");
        owned_composer_frame(
            &ctx,
            id,
            &first,
            &mut first_draft,
            &bindings,
            Vec::new(),
            false,
        );
        assert_eq!(first_draft, "next first");
    }

    #[test]
    fn queued_input_moves_with_a_draft_between_classic_and_split_editors() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let channel = ChannelId::new("example");
        let classic = Id::new("classic_editor");
        let split = Id::new("split_editor");
        let mut draft = "sent message".to_owned();
        owned_composer_frame(
            &ctx,
            classic,
            &channel,
            &mut draft,
            &bindings,
            Vec::new(),
            true,
        );
        let sent = owned_composer_frame(
            &ctx,
            classic,
            &channel,
            &mut draft,
            &bindings,
            vec![
                key(Key::Enter, true, egui::Modifiers::NONE),
                egui::Event::Text("next draft".to_owned()),
            ],
            false,
        );
        assert_eq!(sent.send.as_deref(), Some("sent message"));
        migrate_composer_events(&ctx, classic, split, &channel);
        owned_composer_frame(
            &ctx,
            split,
            &channel,
            &mut draft,
            &bindings,
            Vec::new(),
            true,
        );
        assert_eq!(draft, "next draft");
        migrate_composer_events(&ctx, split, classic, &channel);
        owned_composer_frame(
            &ctx,
            classic,
            &channel,
            &mut draft,
            &bindings,
            Vec::new(),
            true,
        );
        assert_eq!(draft, "next draft");
    }

    #[test]
    fn queued_input_follows_a_channel_redirect_once() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let old = ChannelId::new("old_name");
        let new = ChannelId::new("new_name");
        let id = Id::new("redirected_composer");
        let mut draft = "sent message".to_owned();
        owned_composer_frame(&ctx, id, &old, &mut draft, &bindings, Vec::new(), true);
        owned_composer_frame(
            &ctx,
            id,
            &old,
            &mut draft,
            &bindings,
            vec![
                key(Key::Enter, true, egui::Modifiers::NONE),
                egui::Event::Text("next draft".to_owned()),
            ],
            false,
        );
        rename_composer_event_channel(&ctx, &old, &new);
        owned_composer_frame(&ctx, id, &new, &mut draft, &bindings, Vec::new(), false);
        assert_eq!(draft, "next draft");
        owned_composer_frame(&ctx, id, &new, &mut draft, &bindings, Vec::new(), false);
        assert_eq!(draft, "next draft");
    }

    #[test]
    fn account_changes_keep_queued_text_but_require_a_new_send_gesture() {
        for (keep, shifted, printable) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, false),
            (false, false, true),
            (true, false, true),
        ] {
            let ctx = egui::Context::default();
            let mut bindings = HotkeyBindings::defaults();
            if printable {
                bindings.set(
                    if keep {
                        HotkeyAction::InputSendKeep
                    } else {
                        HotkeyAction::InputSend
                    },
                    crust_core::KeyBinding::new("S"),
                );
            }
            let id = Id::new("account_composer");
            let channel = ChannelId::new("example");
            let mut draft = "first message".to_owned();
            account_composer_frame(
                &ctx,
                id,
                &channel,
                &mut draft,
                &bindings,
                Vec::new(),
                true,
                Some("alice-id"),
            );
            let first_key = if printable && !keep {
                Key::S
            } else {
                Key::Enter
            };
            let mut events = vec![key(first_key, true, egui::Modifiers::NONE)];
            if first_key == Key::S {
                events.push(egui::Event::Text("s".to_owned()));
            }
            events.push(egui::Event::Text("next draft".to_owned()));
            let mut modifiers = egui::Modifiers::NONE;
            if keep && !printable {
                modifiers |= egui::Modifiers::CTRL;
            }
            if shifted {
                modifiers |= egui::Modifiers::SHIFT;
            }
            events.push(key(
                if printable { Key::S } else { Key::Enter },
                true,
                modifiers,
            ));
            if printable {
                events.push(egui::Event::Text("s".to_owned()));
            }
            let sent = account_composer_frame(
                &ctx,
                id,
                &channel,
                &mut draft,
                &bindings,
                events,
                false,
                Some("alice-id"),
            );
            assert_eq!(sent.send.as_deref(), Some("first message"));
            for _ in 0..2 {
                let result = account_composer_frame(
                    &ctx,
                    id,
                    &channel,
                    &mut draft,
                    &bindings,
                    Vec::new(),
                    false,
                    Some("bob-id"),
                );
                assert!(
                    result.send.is_none(),
                    "a queued gesture cannot send under the new account"
                );
                assert_eq!(draft, "next draft");
            }
            let mut fresh_send = vec![key(first_key, true, egui::Modifiers::NONE)];
            if first_key == Key::S {
                fresh_send.push(egui::Event::Text("s".to_owned()));
            }
            let result = account_composer_frame(
                &ctx,
                id,
                &channel,
                &mut draft,
                &bindings,
                fresh_send,
                false,
                Some("bob-id"),
            );
            assert_eq!(result.send.as_deref(), Some("next draft"));
        }
    }

    #[test]
    fn an_account_change_after_preparation_cancels_ready_and_pending_send() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let id = Id::new("account_composer");
        let channel = ChannelId::new("example");
        let mut draft = "first message".to_owned();
        account_composer_frame(
            &ctx,
            id,
            &channel,
            &mut draft,
            &bindings,
            Vec::new(),
            true,
            Some("alice-id"),
        );
        move_cursor_to_end(&ctx, id, draft.chars().count());
        let mut sent = None;
        let _ = ctx.run(
            raw_input(vec![
                key(Key::Enter, true, egui::Modifiers::NONE),
                egui::Event::Text(" next draft".to_owned()),
                key(Key::Enter, true, egui::Modifiers::CTRL),
            ]),
            |ctx| {
                prepare_composer_events(ctx, &bindings, Some((id, &channel)), Some("alice-id"));
                // Runtime events can finish an account switch after input was staged.
                sync_composer_account(ctx, &bindings, Some("bob-id"));
                egui::CentralPanel::default().show(ctx, |ui| {
                    sent = show_configured_composer(ui, id, &channel, &mut draft, &bindings).send;
                });
            },
        );
        assert!(sent.is_none());
        assert_eq!(draft, "first message");
        let result = account_composer_frame(
            &ctx,
            id,
            &channel,
            &mut draft,
            &bindings,
            Vec::new(),
            false,
            Some("bob-id"),
        );
        assert!(result.send.is_none());
        assert_eq!(draft, "first message next draft");
    }

    #[test]
    fn account_changes_cancel_send_in_the_navigation_suffix() {
        let ctx = egui::Context::default();
        let bindings = HotkeyBindings::defaults();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let first_id = Id::new("account_first");
        let second_id = Id::new("account_second");
        let mut first_draft = "first draft".to_owned();
        let mut second_draft = String::new();
        account_composer_frame(
            &ctx,
            first_id,
            &first,
            &mut first_draft,
            &bindings,
            Vec::new(),
            true,
            Some("alice-id"),
        );
        let mut sent = None;
        let _ = ctx.run(
            raw_input(vec![
                key(
                    Key::PageDown,
                    true,
                    egui::Modifiers::CTRL | egui::Modifiers::ALT,
                ),
                egui::Event::Text("second draft".to_owned()),
                key(Key::Enter, true, egui::Modifiers::NONE),
            ]),
            |ctx| {
                prepare_composer_events(ctx, &bindings, Some((first_id, &first)), Some("alice-id"));
                assert!(ctx.input_mut(|input| crate::app::consume_binding(
                    input,
                    &bindings.get(HotkeyAction::SplitFocusNext)
                )));
                request_composer_focus(ctx, Some(second_id));
                sync_composer_account(ctx, &bindings, Some("bob-id"));
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_configured_composer(ui, first_id, &first, &mut first_draft, &bindings);
                    sent = show_configured_composer(
                        ui,
                        second_id,
                        &second,
                        &mut second_draft,
                        &bindings,
                    )
                    .send;
                });
            },
        );
        assert!(sent.is_none());
        let result = account_composer_frame(
            &ctx,
            second_id,
            &second,
            &mut second_draft,
            &bindings,
            Vec::new(),
            false,
            Some("bob-id"),
        );
        assert!(result.send.is_none());
        assert_eq!(first_draft, "first draft");
        assert_eq!(second_draft, "second draft");
    }

    #[test]
    fn input_history_search_filters_cycles_and_accepts_without_sending() {
        let ctx = egui::Context::default();
        let channel = ChannelId::new("example");
        let history = vec![
            "older Café visit".to_owned(),
            "other message".to_owned(),
            "newer CAFÉ visit".to_owned(),
        ];
        let mut buf = "original draft".to_owned();
        input_history_search_frame(&ctx, &channel, &mut buf, &history, Vec::new(), true);
        input_history_search_frame(
            &ctx,
            &channel,
            &mut buf,
            &history,
            vec![key(Key::R, true, egui::Modifiers::CTRL)],
            false,
        );
        assert!(input_history_search_state(&ctx).is_some());
        let result = input_history_search_frame(
            &ctx,
            &channel,
            &mut buf,
            &history,
            vec![egui::Event::Text("cAfÉ".to_owned())],
            false,
        );
        assert!(result.send.is_none());
        assert_eq!(
            input_history_search_state(&ctx).unwrap().selected_text(),
            Some("newer CAFÉ visit")
        );
        assert_eq!(buf, "original draft");
        for (pressed, modifiers, expected) in [
            (Key::R, egui::Modifiers::CTRL, "older Café visit"),
            (Key::R, egui::Modifiers::CTRL, "newer CAFÉ visit"),
            (
                Key::R,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                "older Café visit",
            ),
            (Key::ArrowDown, egui::Modifiers::NONE, "newer CAFÉ visit"),
            (Key::ArrowUp, egui::Modifiers::NONE, "older Café visit"),
        ] {
            let result = input_history_search_frame(
                &ctx,
                &channel,
                &mut buf,
                &history,
                vec![key(pressed, true, modifiers)],
                false,
            );
            assert!(result.send.is_none());
            assert_eq!(
                input_history_search_state(&ctx).unwrap().selected_text(),
                Some(expected)
            );
            assert_eq!(buf, "original draft");
        }
        let result = input_history_search_frame(
            &ctx,
            &channel,
            &mut buf,
            &history,
            vec![key(Key::Enter, true, egui::Modifiers::NONE)],
            false,
        );
        assert!(result.send.is_none(), "accepting a result must not send it");
        assert_eq!(buf, "older Café visit");
        assert!(input_history_search_state(&ctx).is_none());
        assert!(ctx.memory(|memory| memory.has_focus(Id::new("history_search_test"))));
        let result = input_history_search_frame(
            &ctx,
            &channel,
            &mut buf,
            &history,
            vec![key(Key::Enter, true, egui::Modifiers::NONE)],
            false,
        );
        assert_eq!(result.send.as_deref(), Some("older Café visit"));
    }

    #[test]
    fn input_history_search_keeps_draft_on_escape_and_on_missing_match() {
        for history in [vec!["matching message".to_owned()], Vec::new()] {
            let ctx = egui::Context::default();
            let channel = ChannelId::new("example");
            let mut buf = "  original 🦀 draft  ".to_owned();
            input_history_search_frame(&ctx, &channel, &mut buf, &history, Vec::new(), true);
            input_history_search_frame(
                &ctx,
                &channel,
                &mut buf,
                &history,
                vec![key(Key::R, true, egui::Modifiers::CTRL)],
                false,
            );
            input_history_search_frame(
                &ctx,
                &channel,
                &mut buf,
                &history,
                vec![egui::Event::Text("missing".to_owned())],
                false,
            );
            let result = input_history_search_frame(
                &ctx,
                &channel,
                &mut buf,
                &history,
                vec![key(Key::Enter, true, egui::Modifiers::NONE)],
                false,
            );
            assert!(result.send.is_none());
            assert!(input_history_search_state(&ctx).is_some());
            assert_eq!(buf, "  original 🦀 draft  ");
            let result = input_history_search_frame(
                &ctx,
                &channel,
                &mut buf,
                &history,
                vec![key(Key::Escape, true, egui::Modifiers::NONE)],
                false,
            );
            assert!(result.send.is_none());
            assert!(input_history_search_state(&ctx).is_none());
            assert_eq!(buf, "  original 🦀 draft  ");
            assert!(ctx.memory(|memory| memory.has_focus(Id::new("history_search_test"))));
        }
    }

    #[test]
    fn input_history_search_does_not_apply_a_result_to_another_channel() {
        let ctx = egui::Context::default();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let history = vec!["matching message".to_owned()];
        let mut buf = "first draft".to_owned();
        input_history_search_frame(&ctx, &first, &mut buf, &history, Vec::new(), true);
        input_history_search_frame(
            &ctx,
            &first,
            &mut buf,
            &history,
            vec![key(Key::R, true, egui::Modifiers::CTRL)],
            false,
        );
        input_history_search_frame(
            &ctx,
            &first,
            &mut buf,
            &history,
            vec![egui::Event::Text("match".to_owned())],
            false,
        );
        assert!(input_history_search_state(&ctx).unwrap().selected.is_some());
        buf = "second draft".to_owned();
        let result =
            input_history_search_frame(&ctx, &second, &mut buf, &history, Vec::new(), false);
        assert!(result.send.is_none());
        assert!(input_history_search_state(&ctx).is_none());
        assert_eq!(buf, "second draft");
    }

    #[test]
    fn input_history_search_shortcuts_belong_to_the_focused_split() {
        let ctx = egui::Context::default();
        let first_id = Id::new("history_search_first_split");
        let second_id = Id::new("history_search_second_split");
        let first_channel = ChannelId::new("first");
        let second_channel = ChannelId::new("second");
        let history = vec!["matching message".to_owned()];
        let mut first = "first draft".to_owned();
        let mut second = "second draft".to_owned();
        for (index, events) in [
            Vec::new(),
            vec![key(Key::R, true, egui::Modifiers::CTRL)],
            vec![egui::Event::Text("match".to_owned())],
            vec![key(Key::Enter, true, egui::Modifiers::NONE)],
        ]
        .into_iter()
        .enumerate()
        {
            let _ = ctx.run(raw_input(events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for (id, channel, buf) in [
                        (first_id, &first_channel, &mut first),
                        (second_id, &second_channel, &mut second),
                    ] {
                        if index == 0 && id == second_id {
                            ui.memory_mut(|memory| memory.request_focus(id));
                        }
                        let result = ChatInput {
                            hotkey_bindings: &HotkeyBindings::defaults(),
                            id: Some(id),
                            channel,
                            logged_in: true,
                            username: None,
                            emote_catalog: &[],
                            emote_bytes: &UiImageCache::default(),
                            emote_picker_open: false,
                            pending_reply: None,
                            message_history: &history,
                            slash_usage_counts: &HashMap::new(),
                            known_channels: &[],
                            chatters: &[],
                            prevent_overlong_twitch_messages: false,
                            animate_emotes: false,
                            accept_dropped_files: false,
                        }
                        .show(ui, buf);
                        assert!(result.send.is_none());
                    }
                });
            });
            assert!(ctx
                .data(|data| data
                    .get_temp::<InputHistorySearch>(first_id.with("input_history_search")))
                .is_none());
            assert_eq!(first, "first draft");
        }
        assert_eq!(second, "matching message");
        assert!(ctx.memory(|memory| memory.has_focus(second_id)));
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

    fn spelling_test_frame(
        ctx: &egui::Context,
        buf: &mut String,
        channel: &ChannelId,
        events: Vec<egui::Event>,
        focus: bool,
    ) -> egui::FullOutput {
        ctx.run(raw_input(events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let id = Id::new("spelling_menu_test");
                if focus {
                    ui.memory_mut(|memory| memory.request_focus(id));
                }
                ChatInput {
                    hotkey_bindings: &HotkeyBindings::defaults(),
                    id: Some(id),
                    channel,
                    logged_in: true,
                    username: None,
                    emote_catalog: &[],
                    emote_bytes: &UiImageCache::default(),
                    emote_picker_open: false,
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
        })
    }

    fn spelling_word_position(output: &egui::FullOutput, draft: &str, word: &str) -> Pos2 {
        let character = draft[..draft.find(word).unwrap()].chars().count() + 1;
        output
            .shapes
            .iter()
            .find_map(|shape| {
                let egui::Shape::Text(text) = &shape.shape else {
                    return None;
                };
                (text.galley.text() == draft).then(|| {
                    text.pos
                        + text
                            .galley
                            .pos_from_ccursor(egui::text::CCursor::new(character))
                            .center()
                            .to_vec2()
                })
            })
            .expect("composer text is drawn")
    }

    fn spelling_pointer(
        position: Pos2,
        button: egui::PointerButton,
        pressed: bool,
    ) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    #[test]
    fn spelling_menu_corrects_the_clicked_word_and_preserves_unicode() {
        let _settings = crate::spellcheck::test_settings_guard();
        let ctx = egui::Context::default();
        let id = Id::new("spelling_menu_test");
        let channel = ChannelId::new("example");
        let original = "é🙂 teh recieve";
        let mut buf = original.to_owned();
        let output = spelling_test_frame(&ctx, &mut buf, &channel, Vec::new(), true);
        let pointer = spelling_word_position(&output, original, "teh");
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        let selection = egui::text::CCursorRange::two(
            egui::text::CCursor::new(original.chars().count()),
            egui::text::CCursor::new(original.chars().count() - "recieve".len()),
        );
        state.cursor.set_char_range(Some(selection));
        egui::TextEdit::store_state(&ctx, id, state);
        for pressed in [true, false] {
            spelling_test_frame(
                &ctx,
                &mut buf,
                &channel,
                spelling_pointer(pointer, egui::PointerButton::Secondary, pressed),
                false,
            );
        }
        let spelling = ctx
            .data(|data| data.get_temp::<SpellCtx>(id.with("spell_check_ctx")))
            .expect("right-clicked misspelling has suggestions");
        assert_eq!(spelling.word, "teh");
        assert_eq!(buf, original);
        let clicked_cursor = egui::TextEdit::load_state(&ctx, id)
            .unwrap()
            .cursor
            .char_range()
            .unwrap();
        assert_eq!(clicked_cursor.primary.index, "é🙂 t".chars().count());
        assert_eq!(clicked_cursor.primary, clicked_cursor.secondary);

        let output = spelling_test_frame(&ctx, &mut buf, &channel, Vec::new(), false);
        let correction = visible_input_text(&output, "the")
            .expect("the is offered")
            .center();
        for pressed in [true, false] {
            spelling_test_frame(
                &ctx,
                &mut buf,
                &channel,
                spelling_pointer(correction, egui::PointerButton::Primary, pressed),
                false,
            );
        }
        assert_eq!(buf, "é🙂 the recieve");
        let cursor = egui::TextEdit::load_state(&ctx, id)
            .unwrap()
            .cursor
            .char_range()
            .unwrap();
        assert_eq!(cursor.primary.index, "é🙂 the".chars().count());
        assert_eq!(cursor.primary, cursor.secondary);
        assert!(ctx.memory(|memory| memory.has_focus(id)));
    }

    #[test]
    fn spelling_menu_discards_suggestions_after_draft_or_channel_changes() {
        let _settings = crate::spellcheck::test_settings_guard();
        let id = Id::new("spelling_menu_test");
        let channel = ChannelId::new("example");
        let original = "é🙂 teh tail";
        for (replacement, next_channel) in [
            ("", channel.clone()),
            ("🙂🙂🙂", channel.clone()),
            ("é🙂 teh other", channel.clone()),
            (original, ChannelId::new("another")),
        ] {
            let ctx = egui::Context::default();
            let mut buf = original.to_owned();
            let output = spelling_test_frame(&ctx, &mut buf, &channel, Vec::new(), true);
            let pointer = spelling_word_position(&output, original, "teh");
            for pressed in [true, false] {
                spelling_test_frame(
                    &ctx,
                    &mut buf,
                    &channel,
                    spelling_pointer(pointer, egui::PointerButton::Secondary, pressed),
                    false,
                );
            }
            assert!(ctx
                .data(|data| data.get_temp::<SpellCtx>(id.with("spell_check_ctx")))
                .is_some());
            buf = replacement.to_owned();
            let output = spelling_test_frame(&ctx, &mut buf, &next_channel, Vec::new(), false);
            assert!(ctx
                .data(|data| data.get_temp::<SpellCtx>(id.with("spell_check_ctx")))
                .is_none());
            assert!(visible_input_text(&output, "\"teh\" - misspelled").is_none());
            assert_eq!(buf, replacement);
        }
    }

    #[test]
    fn stale_spelling_action_cannot_replace_a_changed_draft() {
        let channel = ChannelId::new("example");
        let original = "é🙂 teh tail";
        let start = original.find("teh").unwrap();
        let spelling = SpellCtx {
            channel: channel.clone(),
            expected_buf: original.to_owned(),
            word: "teh".to_owned(),
            start,
            end: start + 3,
            suggestions: vec!["the".to_owned()],
        };
        for (edited, next_channel) in [
            ("", channel.clone()),
            ("🙂🙂🙂", channel.clone()),
            ("é🙂 teh other", channel.clone()),
            (original, ChannelId::new("another")),
        ] {
            let mut buf = edited.to_owned();
            assert_eq!(spelling.replace(&next_channel, &mut buf, "the"), None);
            assert_eq!(buf, edited);
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

    #[test]
    fn join_completion_does_not_offer_private_conversation_targets() {
        let room = ChannelId::irc("irc.example.test", 6697, true, "rust");
        let query = ChannelId::irc_query("irc.example.test", 6697, true, "alice");
        let matches = find_join_channel_matches("/join ", &room, &[room.clone(), query]);
        assert_eq!(matches, vec!["#rust"]);
    }
}
