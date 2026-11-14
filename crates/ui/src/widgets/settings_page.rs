use std::collections::HashSet;

use egui::{Context, Margin, RichText};

use crust_core::highlight::HighlightRule;
use crust_core::plugins::{PluginUiHostSlot, PluginUiSnapshot};
use crust_core::PluginStatus;

use crate::app::{ChannelLayout, TabVisualStyle};
use crate::theme as t;

use super::{
    chrome,
    plugin_ui::{has_host_panels_for_slot, render_host_panels_for_slot, PluginUiSessionState},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingsSection {
    Appearance,
    Chat,
    Highlights,
    Filters,
    Nicknames,
    Ignores,
    Commands,
    Channels,
    Hotkeys,
    Notifications,
    StreamerMode,
    Integrations,
    Plugins,
    ExternalTools,
    Updates,
}

impl Default for SettingsSection {
    fn default() -> Self {
        Self::Appearance
    }
}

impl SettingsSection {
    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Chat => "Chat",
            Self::Highlights => "Highlights",
            Self::Filters => "Filters",
            Self::Nicknames => "Nicknames",
            Self::Ignores => "Ignores",
            Self::Commands => "Commands",
            Self::Channels => "Channels",
            Self::Hotkeys => "Hotkeys",
            Self::Notifications => "Notifications",
            Self::StreamerMode => "Streamer mode",
            Self::Integrations => "Connections",
            Self::Plugins => "Plugins",
            Self::ExternalTools => "External tools",
            Self::Updates => "Updates",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Appearance => "Theme and window behavior",
            Self::Chat => "Message rendering and input limits",
            Self::Highlights => "Highlight rules and ignored users",
            Self::Filters => "Message filtering and moderation",
            Self::Nicknames => "Custom display names for other users",
            Self::Ignores => "Blocked users and phrase filters",
            Self::Commands => "Custom slash-command aliases with variable expansion",
            Self::Channels => "Auto-join channel management",
            Self::Hotkeys => "Rebindable keyboard shortcuts",
            Self::Notifications => "Desktop notifications and per-event sounds",
            Self::StreamerMode => "Hide sensitive info while broadcasting",
            Self::Integrations => "Chat services, sign-in, and channel points",
            Self::Plugins => "Installed plugins and their settings",
            Self::ExternalTools => "Streamlink and external video players",
            Self::Updates => "Check for and install Crust updates",
        }
    }

    fn group(self) -> &'static str {
        match self {
            Self::Appearance | Self::Hotkeys | Self::Notifications | Self::StreamerMode => {
                "Interface"
            }
            Self::Chat
            | Self::Highlights
            | Self::Filters
            | Self::Nicknames
            | Self::Ignores
            | Self::Commands
            | Self::Channels => "Chat tools",
            Self::Integrations | Self::Plugins | Self::ExternalTools | Self::Updates => "Services",
        }
    }

    fn search_terms(self) -> &'static str {
        match self {
            Self::Appearance => "theme dark light font text size scale zoom always top window layout sidebar tabs split header title game viewer count close buttons live indicators analytics",
            Self::Chat => "timestamps seconds clock 24 hour history logs indexing collapse long messages lines overflow character limit animations focus moderation presets spell check spelling dictionary",
            Self::Highlights => "highlight rules keyword regex regular expression case sensitive visual alert flash sound ignored usernames",
            Self::Filters => "filter hide dim message sender username pattern regex regular expression moderation",
            Self::Nicknames => "nickname alias login display name channel case sensitive mention",
            Self::Ignores => "ignore block user phrase regex regular expression case sensitive replace highlight mention",
            Self::Commands => "command slash alias trigger expansion variables",
            Self::Channels => "channel auto join startup",
            Self::Hotkeys => "hotkey keyboard shortcut key binding rebind reset conflict",
            Self::Notifications => "notification desktop toast audio sound ping volume file preview whisper mention gifted subscription raid alert",
            Self::StreamerMode => "streamer privacy broadcasting obs hide link preview viewer counts mute suppress sounds",
            Self::Integrations => "integration connection twitch browser session sign in login channel points bonus auto claim pronouns user cards kick irc nickserv username password beta transport",
            Self::Plugins => "plugin lua scripts reload permissions settings extensions",
            Self::ExternalTools => "external tools streamlink mpv player video quality arguments path command twitch session token cookie",
            Self::Updates => "update version install restart check automatic release skip",
        }
    }

    fn matches(self, query: &str) -> bool {
        let title = self.title().to_ascii_lowercase();
        let subtitle = self.subtitle().to_ascii_lowercase();
        query.split_whitespace().all(|term| {
            title.contains(term) || subtitle.contains(term) || self.search_terms().contains(term)
        })
    }
}

#[derive(Clone, Default)]
struct SettingsNavigation {
    query: String,
    selected_before_search: Option<SettingsSection>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SettingsStats {
    pub highlights_count: usize,
    pub ignores_count: usize,
    pub auto_join_count: usize,
}

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default))]
pub struct SettingsPageState {
    pub kick_beta_enabled: bool,
    pub irc_beta_enabled: bool,
    pub irc_nickserv_user: String,
    pub irc_nickserv_pass: String,
    pub always_on_top: bool,
    pub prevent_overlong_twitch_messages: bool,
    pub collapse_long_messages: bool,
    pub collapse_long_message_lines: usize,
    pub animations_when_focused: bool,
    pub show_timestamps: bool,
    pub show_timestamp_seconds: bool,
    pub use_24h_timestamps: bool,
    pub local_log_indexing_enabled: bool,
    pub highlights_buf: String,
    pub ignores_buf: String,
    pub auto_join_buf: String,
    pub light_theme: bool,
    /// Chat body font size in points.
    pub chat_font_size: f32,
    /// UI zoom multiplier, independent of native display density.
    pub ui_font_size: f32,
    /// Top chrome toolbar label size (pt).
    pub topbar_font_size: f32,
    /// Channel tab chip label size (pt).
    pub tabs_font_size: f32,
    /// Message timestamp size (pt).
    pub timestamps_font_size: f32,
    /// Room-state pill size (pt).
    pub pills_font_size: f32,
    /// Tooltip / popover label size (pt). 0.0 = auto.
    pub popups_font_size: f32,
    /// Inline chip / inline badge size (pt). 0.0 = auto.
    pub chips_font_size: f32,
    /// User-card heading size (pt). 0.0 = auto.
    pub usercard_font_size: f32,
    /// Login / dialog helper-text size (pt). 0.0 = auto.
    pub dialog_font_size: f32,
    pub channel_layout: ChannelLayout,
    pub sidebar_visible: bool,
    pub analytics_visible: bool,
    pub irc_status_visible: bool,
    pub tab_style: TabVisualStyle,
    pub show_tab_close_buttons: bool,
    pub show_tab_live_indicators: bool,
    pub split_header_show_title: bool,
    pub split_header_show_game: bool,
    pub split_header_show_viewer_count: bool,
    /// Editable structured highlight rules (mirrors AppSettings.highlight_rules).
    pub highlight_rules: Vec<HighlightRule>,
    /// Per-rule draft pattern buffer (indexed parallel to highlight_rules).
    pub highlight_rule_bufs: Vec<String>,
    pub filter_records: Vec<crust_core::model::filters::FilterRecord>,
    pub filter_record_bufs: Vec<String>,
    pub mod_action_presets: Vec<crust_core::model::mod_actions::ModActionPreset>,
    /// Editable nickname aliases (mirrors AppSettings.nicknames).
    pub nicknames: Vec<crust_core::model::Nickname>,
    /// Editable ignored-user entries (mirrors AppSettings.ignored_users).
    pub ignored_users: Vec<crust_core::ignores::IgnoredUser>,
    /// Editable ignored-phrase entries (mirrors AppSettings.ignored_phrases).
    pub ignored_phrases: Vec<crust_core::ignores::IgnoredPhrase>,
    /// Editable custom command aliases (mirrors AppSettings.command_aliases).
    pub command_aliases: Vec<crust_core::commands::CommandAlias>,
    /// Editable hotkey bindings (mirrors AppSettings.hotkey_bindings, merged with defaults).
    pub hotkey_bindings: crust_core::HotkeyBindings,
    /// Action currently awaiting key capture. `None` when no row is in
    /// capture mode. Capture runs before app shortcuts and widget input.
    pub hotkey_capture_target: Option<crust_core::HotkeyAction>,
    /// Opt-in fetch of pronouns from alejo.io on user profile popup.
    pub show_pronouns_in_usercard: bool,
    /// Opt-in auto-claim of channel-points "Bonus Points" rewards.
    pub auto_claim_bonus_points: bool,
    /// Last-known login state from the embedded webview. `None` = unknown.
    pub twitch_webview_logged_in: Option<bool>,
    /// Set to `true` when the user clicked "Open Twitch sign-in".
    pub twitch_sign_in_requested: bool,
    pub plugin_statuses: Vec<PluginStatus>,
    pub plugin_ui: PluginUiSnapshot,
    pub plugin_reload_requested: bool,
    /// Desktop notification toggle.
    pub desktop_notifications_enabled: bool,
    /// Enable startup/background update checks.
    pub update_checks_enabled: bool,
    /// Last updater check timestamp from settings.
    pub updater_last_checked_at: Option<String>,
    /// Version currently marked as skipped.
    pub updater_skipped_version: String,
    /// Latest available version from runtime, when known.
    pub updater_available_version: Option<String>,
    /// Latest available asset name from runtime, when known.
    pub updater_available_asset: Option<String>,
    /// Latest available release URL from runtime, when known.
    pub updater_available_release_url: Option<String>,
    /// True while install pipeline is running.
    pub updater_install_inflight: bool,
    /// Request a manual update check.
    pub request_update_check_now: bool,
    /// Request install/update staging and immediate restart.
    pub request_update_install_now: bool,
    /// Request skipping the currently available version.
    pub request_skip_available_update: bool,
    /// Request opening the available release page.
    pub request_open_available_release: bool,
    /// Request simulating a gifted sub event toast/notification.
    pub request_test_gifted_sub_alert: bool,
    /// Streamer mode setting (`off`, `auto`, or `on`).
    pub streamer_mode: String,
    /// Hide link preview tooltips while streamer mode is active.
    pub streamer_hide_link_previews: bool,
    /// Hide viewer counts in split headers while streamer mode is active.
    pub streamer_hide_viewer_counts: bool,
    /// Suppress sound notifications while streamer mode is active.
    pub streamer_suppress_sounds: bool,
    /// True iff broadcasting software detection currently considers it active.
    pub streamer_mode_active: bool,
    /// Streamlink binary path (empty = rely on PATH).
    pub external_streamlink_path: String,
    /// Preferred Streamlink quality token.
    pub external_streamlink_quality: String,
    /// Extra CLI args prepended to every Streamlink invocation.
    pub external_streamlink_extra_args: String,
    /// Command template for "Open in player".
    pub external_player_template: String,
    /// Path to the `mpv` binary (empty = rely on PATH).
    pub external_mpv_path: String,
    /// Twitch session `auth-token` cookie value (optional).
    pub external_streamlink_session_token: String,
    /// Per-event sound notification configuration (mention / whisper /
    /// sub / raid / custom highlight). Edits sync back to the runtime via
    /// [`crust_core::events::AppCommand::SetSoundSettings`].
    pub sound_events: crust_core::sound::SoundSettings,
    /// Preview request set by the settings page: each key maps to a
    /// single-shot `true` after the user clicks "Preview" on that row.
    /// The app reads this in the same frame and routes the request to the
    /// [`crate::sound::SoundController`].
    pub sound_preview_request: Option<crust_core::sound::SoundEvent>,
    /// Whether chat-input spellchecking is enabled (mirrors
    /// `AppSettings::spellcheck_enabled`).
    pub spellcheck_enabled: bool,
    /// Sorted snapshot of user-added spellcheck dictionary words (mirrors
    /// `AppSettings::custom_spell_dict`). Edits in this view are serialised
    /// via [`AppCommand::SetCustomSpellDictionary`].
    pub spell_custom_dict: Vec<String>,
    /// One-shot input buffer for the "Add word" field in the settings page.
    pub spell_custom_dict_add_buf: String,
    /// Advanced filter-expression editor modal state (shared by highlight
    /// rules and filter records).
    pub filter_editor_modal: super::filter_editor::FilterEditorModal,
}

/// Render the Hotkeys settings page - one row per rebindable action
/// with a "Click to bind" capture button. Conflicts render inline with a
/// red warning.
fn render_hotkeys_section(ui: &mut egui::Ui, state: &mut SettingsPageState, compact: bool) {
    use crust_core::{HotkeyAction, HotkeyBindings, HotkeyCategory, KeyBinding};
    ui.label(
        RichText::new(
            "Choose a binding, then press the new key combination. Escape cancels capture.",
        )
        .font(t::small())
        .color(t::text_secondary()),
    );
    ui.horizontal_wrapped(|ui| {
        if ui.button("Reset all to defaults").clicked() {
            state.hotkey_bindings = HotkeyBindings::defaults();
            state.hotkey_capture_target = None;
        }
        let conflicts = state.hotkey_bindings.conflicts().len();
        if conflicts > 0 {
            ui.label(
                RichText::new(format!("{conflicts} conflicting bindings"))
                    .font(t::small())
                    .color(t::red()),
            );
        }
    });
    for category in HotkeyCategory::all() {
        ui.add_space(8.0);
        ui.label(
            RichText::new(category.display_name())
                .font(t::body())
                .strong(),
        );
        for action in HotkeyAction::all() {
            if action.category() != category {
                continue;
            }
            ui.push_id(action.display_name(), |ui| {
                let current = state.hotkey_bindings.get(action);
                let capture_active = state.hotkey_capture_target == Some(action);
                let conflict = state.hotkey_bindings.find_conflict(action, &current);
                ui.label(RichText::new(action.display_name()).font(t::small()).color(
                    if conflict.is_some() {
                        t::red()
                    } else {
                        t::text_primary()
                    },
                ));
                ui.horizontal_wrapped(|ui| {
                    let label = if capture_active {
                        "Press any key...".to_owned()
                    } else {
                        current.display_label()
                    };
                    let color = if capture_active {
                        t::accent()
                    } else if conflict.is_some() {
                        t::red()
                    } else if current.is_unbound() {
                        t::text_muted()
                    } else {
                        t::text_primary()
                    };
                    let bind = ui.button(RichText::new(label).font(t::small()).color(color));
                    if bind.is_pointer_button_down_on() {
                        bind.request_focus();
                    }
                    if bind.clicked() {
                        state.hotkey_capture_target = if capture_active {
                            None
                        } else {
                            bind.request_focus();
                            Some(action)
                        };
                    }
                    ui.memory_mut(|memory| {
                        memory.set_focus_lock_filter(
                            bind.id,
                            if state.hotkey_capture_target == Some(action) {
                                egui::EventFilter {
                                    tab: true,
                                    horizontal_arrows: true,
                                    vertical_arrows: true,
                                    escape: true,
                                }
                            } else {
                                egui::EventFilter::default()
                            },
                        )
                    });
                    if ui
                        .button("Clear")
                        .on_hover_text("Unbind this shortcut")
                        .clicked()
                    {
                        state.hotkey_bindings.set(action, KeyBinding::default());
                        if state.hotkey_capture_target == Some(action) {
                            state.hotkey_capture_target = None;
                        }
                    }
                    if ui
                        .button("Reset")
                        .on_hover_text("Restore the default binding")
                        .clicked()
                    {
                        state
                            .hotkey_bindings
                            .set(action, HotkeyBindings::defaults().get(action));
                        if state.hotkey_capture_target == Some(action) {
                            state.hotkey_capture_target = None;
                        }
                    }
                });
                if let Some(other) = conflict {
                    ui.label(
                        RichText::new(format!("Also assigned to {}", other.display_name()))
                            .font(t::tiny())
                            .color(t::red()),
                    );
                }
            });
            ui.add_space(if compact { 4.0 } else { 6.0 });
        }
    }
}

/// Result of polling egui input for a hotkey-capture gesture.
enum CaptureOutcome {
    /// User pressed Escape -> cancel capture, leave binding untouched.
    Cancel,
    /// User pressed a non-modifier key -> assign it as the new binding.
    Binding(crust_core::KeyBinding),
}

/// Give a pending binding capture the whole keyboard frame before shortcuts or
/// widgets can act on it. The caller cancels capture when its settings page closes.
pub(crate) fn handle_hotkey_capture(
    ctx: &Context,
    target: &mut Option<crust_core::HotkeyAction>,
    bindings: &mut crust_core::HotkeyBindings,
) -> bool {
    let Some(action) = *target else {
        return false;
    };
    let captured = ctx.input_mut(|input| {
        let captured = capture_keybinding(input);
        input.events.retain(|event| {
            !matches!(
                event,
                egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Paste(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Ime(_)
            )
        });
        captured
    });
    if let Some(captured) = captured {
        if let CaptureOutcome::Binding(binding) = captured {
            bindings.set(action, binding);
        }
        *target = None;
    }
    true
}

/// Inspect the current egui input frame for a fresh key-down event and
/// translate it into a [`CaptureOutcome`]. Skips pure modifier presses so
/// users can hold modifiers without prematurely committing the binding.
fn capture_keybinding(input: &egui::InputState) -> Option<CaptureOutcome> {
    for event in &input.events {
        if let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        {
            if *key == egui::Key::Escape {
                return Some(CaptureOutcome::Cancel);
            }
            let name = crate::app::egui_key_name(*key);
            if name.is_empty() {
                // Unsupported key (e.g. a key we haven't mapped).
                // Ignore and keep waiting for a known one.
                continue;
            }
            let binding = crust_core::KeyBinding {
                ctrl: modifiers.ctrl,
                shift: modifiers.shift,
                alt: modifiers.alt,
                command: modifiers.mac_cmd,
                key: name.to_owned(),
            };
            return Some(CaptureOutcome::Binding(binding));
        }
    }
    None
}

/// Render the Notifications settings page - desktop notification toggle
/// plus per-event sound configuration (mention / whisper / sub / raid /
/// custom highlight). Each row has an Enabled checkbox, a file-path text
/// field, a volume slider, and a Preview button. A global "Streamer mode
/// active" hint reminds the user that playback is gated while streaming
/// when they've opted into `streamer_suppress_sounds`.
fn render_notifications_section(ui: &mut egui::Ui, state: &mut SettingsPageState, compact: bool) {
    use crust_core::sound::SoundEvent;

    ui.label(
        RichText::new(
            "Desktop notifications and audio pings for highlights, mentions, whispers, subs, and raids.",
        )
        .font(t::small())
        .color(t::text_secondary()),
    );
    ui.add_space(6.0);

    ui.label(RichText::new("Desktop notifications").strong());
    ui.checkbox(
        &mut state.desktop_notifications_enabled,
        "Show OS desktop notifications for mentions and highlights",
    );
    ui.label(
        RichText::new("Whispers from other users also trigger notifications.")
            .font(t::tiny())
            .color(t::text_muted()),
    );
    if ui.button("Test gifted sub alert").clicked() {
        state.request_test_gifted_sub_alert = true;
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    ui.label(RichText::new("Sound pings").strong());
    ui.label(
        RichText::new(
            "Leave the file path empty to use the built-in default ping. Supported formats: WAV, MP3, OGG, FLAC.",
        )
        .font(t::tiny())
        .color(t::text_muted()),
    );

    if state.streamer_mode_active && state.streamer_suppress_sounds {
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Streamer mode is active - live pings are muted. Preview still works so you can test files.",
            )
            .font(t::tiny())
            .color(t::accent()),
        );
    }

    ui.add_space(6.0);

    let row_height = if compact { 6.0 } else { 8.0 };

    for event in SoundEvent::all() {
        let event = *event;
        let key = event.as_key().to_owned();
        let mut setting = state.sound_events.get(event);
        let mut changed = false;

        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .checkbox(&mut setting.enabled, "")
                    .on_hover_text(format!("Enable {} sound", event.display_name()))
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    RichText::new(event.display_name())
                        .font(t::body())
                        .strong()
                        .color(t::text_primary()),
                );
                ui.add_space(8.0);
                if ui
                    .button(RichText::new("Preview").font(t::tiny()))
                    .on_hover_text("Play this sound once, ignoring streamer mode")
                    .clicked()
                {
                    state.sound_preview_request = Some(event);
                }
                if !setting.path.trim().is_empty()
                    && ui
                        .button(RichText::new("Clear file").font(t::tiny()))
                        .on_hover_text("Revert to the built-in default ping")
                        .clicked()
                {
                    setting.path.clear();
                    changed = true;
                }
            });

            ui.add_space(row_height * 0.25);

            ui.horizontal(|ui| {
                ui.label(RichText::new("File").font(t::tiny()).color(t::text_muted()));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut setting.path)
                        .desired_width(f32::INFINITY)
                        .hint_text("Absolute path to a WAV/MP3/OGG/FLAC file (empty = default)"),
                );
                if response.changed() {
                    changed = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Volume")
                        .font(t::tiny())
                        .color(t::text_muted()),
                );
                let before = setting.volume;
                ui.add(
                    egui::Slider::new(&mut setting.volume, 0.0..=1.0)
                        .show_value(true)
                        .fixed_decimals(2),
                );
                if (before - setting.volume).abs() > f32::EPSILON {
                    changed = true;
                }
            });
        });

        if changed {
            state.sound_events.events.insert(key, setting);
        }
        ui.add_space(row_height * 0.5);
    }
}

/// Render the Commands settings page - list + row editor for the user's
/// custom command aliases. Mirrors the Nicknames editor layout.
fn render_commands_section(ui: &mut egui::Ui, state: &mut SettingsPageState, _compact: bool) {
    use crust_core::commands::CommandAlias;

    ui.label(
        RichText::new("Custom command aliases")
            .font(t::small())
            .strong()
            .color(t::text_primary()),
    );
    ui.label(
        RichText::new(
            "Define a trigger like `hi` and a body like `/me says hi {1} {2+}`. \
             Variables: {1}, {2}, ... {1+} (1st arg and everything after), {input}, \
             {channel}, {streamer}, {user}. Aliases whose body starts with /<cmd> \
             chain into the normal slash-command pipeline.",
        )
        .font(t::tiny())
        .color(t::text_muted()),
    );
    ui.add_space(4.0);

    // Detect duplicates by canonical trigger up-front so we can colour the
    // offending rows.
    let mut seen_triggers: HashSet<String> = HashSet::new();
    let mut duplicate_triggers: HashSet<String> = HashSet::new();
    for a in state.command_aliases.iter() {
        let key = a.canonical_trigger();
        if !key.is_empty() && !seen_triggers.insert(key.clone()) {
            duplicate_triggers.insert(key);
        }
    }

    let mut delete_alias_idx = None;
    for (i, alias) in state.command_aliases.iter_mut().enumerate() {
        ui.push_id(("command_alias", i), |ui| {
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.checkbox(&mut alias.enabled, "Enabled");
                let canonical = alias.canonical_trigger();
                let invalid = !alias.is_valid() || duplicate_triggers.contains(&canonical);
                ui.label("Trigger");
                ui.add(
                    egui::TextEdit::singleline(&mut alias.trigger)
                        .desired_width(f32::INFINITY)
                        .hint_text("hi")
                        .text_color(if invalid { t::red() } else { t::text_primary() }),
                );
                ui.label("Command");
                ui.add(
                    egui::TextEdit::singleline(&mut alias.body)
                        .desired_width(f32::INFINITY)
                        .hint_text("/me says hi {1} {2+}"),
                );
                if alias.is_valid() {
                    let preview_input =
                        format!("/{} alice bob how are you", alias.canonical_trigger());
                    if let Ok(crust_core::commands::AliasExpansion::Expanded { text, .. }) =
                        crust_core::commands::expand_command_aliases(
                            &preview_input,
                            std::slice::from_ref(alias),
                            "channel",
                            "me",
                        )
                    {
                        ui.label(
                            RichText::new(format!("Preview: {text}"))
                                .font(t::tiny())
                                .color(t::text_secondary()),
                        )
                        .on_hover_text(format!("Sample input: {preview_input}"));
                    }
                }
                if ui
                    .button(RichText::new("Remove alias").color(t::red()))
                    .clicked()
                {
                    delete_alias_idx = Some(i);
                }
            });
        });
        ui.add_space(6.0);
    }

    if let Some(i) = delete_alias_idx {
        state.command_aliases.remove(i);
    }

    if ui.button("+ Add alias").clicked() {
        state.command_aliases.push(CommandAlias::new("", "/me "));
    }

    ui.add_space(6.0);

    // Surface validation issues inline so users don't have to send a test
    // message to discover a mistake.
    let invalid_count = state
        .command_aliases
        .iter()
        .filter(|a| !a.is_valid())
        .count();
    if invalid_count > 0 {
        ui.label(
            RichText::new(format!(
                "⚠ {invalid_count} alias(es) have an empty trigger/body or whitespace in the trigger; they are ignored at runtime."
            ))
            .font(t::tiny())
            .color(t::red()),
        );
    }
    if !duplicate_triggers.is_empty() {
        let mut list: Vec<&String> = duplicate_triggers.iter().collect();
        list.sort();
        let joined = list
            .iter()
            .map(|t| format!("/{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        ui.label(
            RichText::new(format!(
                "⚠ Duplicate trigger(s): {joined}. Only the first enabled entry for each trigger will be used."
            ))
            .font(t::tiny())
            .color(t::red()),
        );
    }

    ui.label(
        RichText::new(format!(
            "{} alias(es) defined.",
            state.command_aliases.len()
        ))
        .font(t::tiny())
        .color(t::text_muted()),
    );
}

pub fn parse_settings_lines(input: &str, lowercase: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for token in input
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let value = if lowercase {
            token.to_ascii_lowercase()
        } else {
            token.to_owned()
        };
        let key = value.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(value);
        }
    }
    out
}

fn settings_sections() -> [SettingsSection; 15] {
    [
        SettingsSection::Appearance,
        SettingsSection::Hotkeys,
        SettingsSection::Notifications,
        SettingsSection::StreamerMode,
        SettingsSection::Chat,
        SettingsSection::Highlights,
        SettingsSection::Filters,
        SettingsSection::Nicknames,
        SettingsSection::Ignores,
        SettingsSection::Commands,
        SettingsSection::Channels,
        SettingsSection::Integrations,
        SettingsSection::Plugins,
        SettingsSection::ExternalTools,
        SettingsSection::Updates,
    ]
}

fn plugin_status_counts(statuses: &[PluginStatus]) -> (usize, usize) {
    let loaded = statuses.iter().filter(|status| status.loaded).count();
    let failed = statuses.len().saturating_sub(loaded);
    (loaded, failed)
}

#[derive(Clone, Copy)]
enum RuleAction {
    MoveUp,
    MoveDown,
    Duplicate,
    Delete,
}

fn render_rule_actions(ui: &mut egui::Ui, index: usize, count: usize) -> Option<RuleAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(index > 0, egui::Button::new("Move up"))
            .clicked()
        {
            action = Some(RuleAction::MoveUp);
        }
        if ui
            .add_enabled(index + 1 < count, egui::Button::new("Move down"))
            .clicked()
        {
            action = Some(RuleAction::MoveDown);
        }
        if ui.button("Duplicate").clicked() {
            action = Some(RuleAction::Duplicate);
        }
        if ui.button(RichText::new("Remove").color(t::red())).clicked() {
            action = Some(RuleAction::Delete);
        }
    });
    action
}

fn render_plugin_manifest_line(ui: &mut egui::Ui, label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(label)
                .font(t::tiny())
                .strong()
                .color(t::text_muted()),
        );
        ui.label(
            RichText::new(value)
                .font(t::tiny())
                .color(t::text_secondary()),
        );
    });
}

fn render_plugin_status_card(ui: &mut egui::Ui, status: &PluginStatus, compact: bool) {
    let frame = egui::Frame::new()
        .fill(t::bg_surface())
        .stroke(egui::Stroke::new(1.0_f32, t::border_subtle()))
        .inner_margin(Margin::same(if compact { 8 } else { 10 }));

    frame.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(&status.manifest.name)
                    .font(t::body())
                    .strong()
                    .color(t::text_primary()),
            );
            let state_text = if status.loaded { "Loaded" } else { "Failed" };
            let state_color = if status.loaded { t::green() } else { t::red() };
            ui.label(
                RichText::new(state_text)
                    .font(t::tiny())
                    .strong()
                    .color(state_color),
            );
            ui.label(
                RichText::new(format!(
                    "{} command{}",
                    status.command_count,
                    if status.command_count == 1 { "" } else { "s" }
                ))
                .font(t::tiny())
                .color(t::text_muted()),
            );
        });

        if !status.manifest.version.trim().is_empty() {
            ui.label(
                RichText::new(format!("Version {}", status.manifest.version))
                    .font(t::tiny())
                    .color(t::text_muted()),
            );
        }
        render_plugin_manifest_line(ui, "Authors:", &status.manifest.authors.join(", "));
        render_plugin_manifest_line(ui, "Homepage:", &status.manifest.homepage);
        render_plugin_manifest_line(ui, "Tags:", &status.manifest.tags.join(", "));
        render_plugin_manifest_line(ui, "Entry:", &status.manifest.entry);
        render_plugin_manifest_line(ui, "Permissions:", &status.manifest.permissions.join(", "));
        if !status.manifest.description.trim().is_empty() {
            ui.label(
                RichText::new(&status.manifest.description)
                    .font(t::tiny())
                    .color(t::text_secondary()),
            );
        }
        if let Some(error) = status.error.as_ref().filter(|err| !err.trim().is_empty()) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Error: {error}"))
                    .font(t::tiny())
                    .color(t::red()),
            );
        }
    });
}

fn matching_sections(query: &str) -> Vec<SettingsSection> {
    let query = query.trim().to_ascii_lowercase();
    settings_sections()
        .into_iter()
        .filter(|section| section.matches(&query))
        .collect()
}

fn render_settings_search(
    ui: &mut egui::Ui,
    navigation: &mut SettingsNavigation,
    selected: &mut SettingsSection,
) -> Vec<SettingsSection> {
    let search_id = egui::Id::new("settings_search");
    let focused = ui.memory(|memory| memory.has_focus(search_id));
    let (previous, next, accept, clear) = if focused {
        ui.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                !navigation.query.is_empty()
                    && input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        })
    } else {
        (false, false, false, false)
    };
    let was_empty = navigation.query.trim().is_empty();
    if was_empty {
        navigation.selected_before_search = Some(*selected);
    }
    let mut changed = false;
    let response = ui
        .horizontal(|ui| {
            let clear_width = if navigation.query.is_empty() {
                0.0
            } else {
                52.0
            };
            let response = ui
                .add(
                    egui::TextEdit::singleline(&mut navigation.query)
                        .id(search_id)
                        .hint_text("Find a setting...")
                        .desired_width((ui.available_width() - clear_width).max(20.0)),
                )
                .on_hover_text(
                    "Search settings pages by name or option. Use Up and Down to choose a result.",
                );
            changed |= response.changed();
            if clear || (!navigation.query.is_empty() && ui.button("Clear").clicked()) {
                navigation.query.clear();
                changed = true;
                response.request_focus();
            }
            response
        })
        .inner;
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            search_id,
            egui::EventFilter {
                escape: true,
                vertical_arrows: true,
                ..Default::default()
            },
        )
    });
    let matches = matching_sections(&navigation.query);
    if changed {
        if navigation.query.trim().is_empty() {
            if let Some(previous) = navigation.selected_before_search.take() {
                *selected = previous;
            }
        } else if !matches.contains(selected) {
            if let Some(first) = matches.first() {
                *selected = *first;
            }
        }
    }
    if !matches.is_empty() && (previous || next) {
        let current = matches
            .iter()
            .position(|section| section == selected)
            .unwrap_or(0);
        let next_index = if next {
            (current + 1).min(matches.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        *selected = matches[next_index];
    }
    if accept && !matches.is_empty() {
        navigation.selected_before_search = Some(*selected);
        response.surrender_focus();
    }
    matches
}

fn render_sections_nav(
    ui: &mut egui::Ui,
    selected: &mut SettingsSection,
    navigation: &mut SettingsNavigation,
    sections: &[SettingsSection],
    compact: bool,
) {
    let previous = *selected;
    if compact {
        egui::ComboBox::from_id_salt("settings_section_combo")
            .selected_text(selected.title())
            .width(ui.available_width().max(40.0))
            .show_ui(ui, |ui| {
                let mut group = "";
                for section in sections {
                    if group != section.group() {
                        group = section.group();
                        ui.label(RichText::new(group).font(t::tiny()).color(t::text_helper()));
                    }
                    ui.selectable_value(selected, *section, section.title());
                }
            });
    } else {
        let mut group = "";
        for section in sections {
            if group != section.group() {
                if !group.is_empty() {
                    ui.add_space(8.0);
                }
                group = section.group();
                ui.label(RichText::new(group).font(t::tiny()).color(t::text_helper()));
            }
            if ui
                .add_sized(
                    [ui.available_width(), 24.0],
                    egui::SelectableLabel::new(
                        *selected == *section,
                        RichText::new(section.title()).font(t::small()),
                    ),
                )
                .on_hover_text(section.subtitle())
                .clicked()
            {
                *selected = *section;
            }
        }
    }
    if *selected != previous {
        navigation.selected_before_search = Some(*selected);
    }
}

fn render_settings_content(
    ui: &mut egui::Ui,
    settings_section: SettingsSection,
    state: &mut SettingsPageState,
    plugin_ui_session: &mut PluginUiSessionState,
    compact: bool,
    ultra_compact: bool,
) {
    let content_margin = if ultra_compact { 8 } else { 12 };
    chrome::card_frame()
        .inner_margin(Margin::same(content_margin))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            chrome::dialog_header(
                ui,
                settings_section.title(),
                (!ultra_compact).then_some(settings_section.subtitle()),
            );
            ui.add_space(10.0);

            match settings_section {
                SettingsSection::Appearance => {
                    ui.label(
                        RichText::new("Theme and window")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    if compact {
                        ui.horizontal_wrapped(|ui| {
                            ui.selectable_value(&mut state.light_theme, false, "Dark");
                            ui.selectable_value(&mut state.light_theme, true, "Light");
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Theme:");
                            ui.selectable_value(&mut state.light_theme, false, "Dark");
                            ui.selectable_value(&mut state.light_theme, true, "Light");
                        });
                    }
                    ui.checkbox(&mut state.always_on_top, "Always on top");
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Font sizes")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    if compact {
                        ui.label("Chat text");
                        ui.add(
                            egui::Slider::new(
                                &mut state.chat_font_size,
                                t::MIN_CHAT_FONT_SIZE..=t::MAX_CHAT_FONT_SIZE,
                            )
                            .step_by(0.5)
                            .suffix(" pt"),
                        );
                        ui.label("UI scale");
                        ui.add(
                            egui::Slider::new(
                                &mut state.ui_font_size,
                                t::MIN_UI_FONT_SIZE..=t::MAX_UI_FONT_SIZE,
                            )
                            .step_by(0.05)
                            .suffix("x"),
                        );
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Chat text:");
                            ui.add(
                                egui::Slider::new(
                                    &mut state.chat_font_size,
                                    t::MIN_CHAT_FONT_SIZE..=t::MAX_CHAT_FONT_SIZE,
                                )
                                .step_by(0.5)
                                .suffix(" pt"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("UI scale:");
                            ui.add(
                                egui::Slider::new(
                                    &mut state.ui_font_size,
                                    t::MIN_UI_FONT_SIZE..=t::MAX_UI_FONT_SIZE,
                                )
                                .step_by(0.05)
                                .suffix("x"),
                            );
                        });
                    }
                    ui.label(
                        RichText::new("Tip: Ctrl+= / Ctrl+- / Ctrl+0 or Ctrl+scroll to zoom chat.")
                            .font(t::tiny())
                            .color(t::text_helper()),
                    );
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Advanced text sizes")
                        .id_salt("settings_advanced_text_sizes")
                        .show(ui, |ui| {
                        ui.label(RichText::new("Uncheck Auto to set a size for one part of the interface.")
                            .font(t::tiny()).color(t::text_helper()));
                        let section_range = t::MIN_SECTION_FONT_SIZE..=t::MAX_SECTION_FONT_SIZE;
                        let chat_font = state.chat_font_size;
                        for (label, slot, auto_offset) in [
                        ("Top bar", &mut state.topbar_font_size, 1.0_f32),
                        ("Channel tabs", &mut state.tabs_font_size, 1.0),
                        ("Timestamps", &mut state.timestamps_font_size, 1.0),
                        ("Room-state pills", &mut state.pills_font_size, 2.0),
                        ("Tooltips & popovers", &mut state.popups_font_size, 1.5),
                        ("Inline chips", &mut state.chips_font_size, -4.5),
                        ("User card heading", &mut state.usercard_font_size, -14.5),
                        ("Dialog helper text", &mut state.dialog_font_size, 3.5),
                    ] {
                        let auto_value = (chat_font - auto_offset).max(8.5);
                        let mut is_auto = *slot <= 0.0;
                        let mut override_val = if *slot > 0.0 { *slot } else { auto_value };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("{label}:"));
                            ui.checkbox(&mut is_auto, "Auto");
                            ui.add_enabled_ui(!is_auto, |ui| {
                                ui.add(
                                    egui::Slider::new(&mut override_val, section_range.clone())
                                        .step_by(0.5)
                                        .suffix(" pt"),
                                );
                            });
                        });
                        *slot = if is_auto { 0.0 } else { override_val };
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Layout")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(
                            &mut state.channel_layout,
                            ChannelLayout::Sidebar,
                            "Sidebar",
                        );
                        ui.selectable_value(
                            &mut state.channel_layout,
                            ChannelLayout::TopTabs,
                            "Top tabs",
                        );
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(
                            &mut state.tab_style,
                            TabVisualStyle::Compact,
                            "Compact tabs",
                        );
                        ui.selectable_value(
                            &mut state.tab_style,
                            TabVisualStyle::Normal,
                            "Normal tabs",
                        );
                    });
                    ui.checkbox(&mut state.sidebar_visible, "Show sidebar in sidebar mode");
                    ui.checkbox(&mut state.analytics_visible, "Show analytics panel");
                    if state.irc_beta_enabled {
                        ui.checkbox(&mut state.irc_status_visible, "Show IRC status panel");
                    } else {
                        ui.label(
                            RichText::new("Enable IRC beta to use the IRC diagnostics panel.")
                                .font(t::tiny())
                                .color(t::text_helper()),
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Tabs and split headers")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.checkbox(
                        &mut state.show_tab_close_buttons,
                        "Show tab close buttons on hover/selection",
                    );
                    ui.checkbox(
                        &mut state.show_tab_live_indicators,
                        "Show live dots on Twitch tabs",
                    );
                    ui.checkbox(
                        &mut state.split_header_show_viewer_count,
                        "Show split-header viewer count",
                    );
                    ui.checkbox(
                        &mut state.split_header_show_title,
                        "Show split-header title",
                    );
                    ui.checkbox(
                        &mut state.split_header_show_game,
                        "Show split-header game",
                    );
                    if has_host_panels_for_slot(
                        &state.plugin_ui,
                        PluginUiHostSlot::SettingsAppearance,
                    ) {
                        ui.add_space(10.0);
                        render_host_panels_for_slot(
                            ui,
                            &state.plugin_ui,
                            plugin_ui_session,
                            PluginUiHostSlot::SettingsAppearance,
                        );
                    }
                }
                SettingsSection::Chat => {
                    ui.checkbox(&mut state.show_timestamps, "Show message timestamps");
                    ui.add_enabled_ui(state.show_timestamps, |ui| {
                        ui.checkbox(&mut state.use_24h_timestamps, "Use 24-hour clock");
                        ui.checkbox(
                            &mut state.show_timestamp_seconds,
                            "Include seconds in timestamps",
                        );
                    });
                    ui.checkbox(
                        &mut state.local_log_indexing_enabled,
                        "Enable local chat log indexing",
                    );
                    ui.checkbox(
                        &mut state.collapse_long_messages,
                        "Collapse long chat messages",
                    );
                    ui.add_enabled_ui(state.collapse_long_messages, |ui| {
                        if compact {
                            ui.label("Collapse after");
                            ui.add(
                                egui::Slider::new(&mut state.collapse_long_message_lines, 2..=24)
                                    .text("lines"),
                            );
                        } else {
                            ui.horizontal(|ui| {
                                ui.label("Collapse after");
                                ui.add(
                                    egui::Slider::new(&mut state.collapse_long_message_lines, 2..=24)
                                        .text("lines"),
                                );
                            });
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Twitch message overflow")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    if compact {
                        ui.vertical(|ui| {
                            ui.radio_value(
                                &mut state.prevent_overlong_twitch_messages,
                                false,
                                if ultra_compact {
                                    "Highlight over 500 chars"
                                } else {
                                    "Highlight (allow typing over 500 chars)"
                                },
                            );
                            ui.radio_value(
                                &mut state.prevent_overlong_twitch_messages,
                                true,
                                if ultra_compact {
                                    "Prevent over 500 chars"
                                } else {
                                    "Prevent (hard cap at 500)"
                                },
                            );
                        });
                    } else {
                        ui.horizontal_wrapped(|ui| {
                            ui.radio_value(
                                &mut state.prevent_overlong_twitch_messages,
                                false,
                                "Highlight (allow typing over 500 chars)",
                            );
                            ui.radio_value(
                                &mut state.prevent_overlong_twitch_messages,
                                true,
                                "Prevent (hard cap at 500)",
                            );
                        });
                    }
                    ui.checkbox(
                        &mut state.animations_when_focused,
                        if ultra_compact {
                            "Animate only when focused"
                        } else {
                            "Animate only while window is focused"
                        },
                    );

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Moderation actions")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new("Variables: {user}, {channel}")
                            .font(t::tiny())
                            .color(t::text_muted()),
                    );
                    ui.add_space(4.0);

                    let mut delete_preset_idx: Option<usize> = None;

                    egui::Grid::new("mod_presets_grid")
                        .num_columns(3)
                        .spacing(egui::vec2(8.0, 4.0))
                        .show(ui, |ui| {
                            // Header row
                            ui.label(RichText::new("Label").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Command").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.end_row();

                            for (i, preset) in state.mod_action_presets.iter_mut().enumerate() {
                                ui.add(
                                    egui::TextEdit::singleline(&mut preset.label)
                                        .desired_width(if compact { 60.0 } else { 80.0 })
                                        .hint_text("Label"),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut preset.command_template)
                                        .desired_width(if compact { 120.0 } else { 200.0 })
                                        .hint_text("/timeout {user} 600"),
                                );
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("×").font(t::tiny()).color(t::red()),
                                        )
                                        .min_size(egui::vec2(24.0, 20.0)),
                                    )
                                    .on_hover_text("Delete moderation preset")
                                    .clicked()
                                {
                                    delete_preset_idx = Some(i);
                                }
                                ui.end_row();
                            }
                        });

                    if let Some(i) = delete_preset_idx {
                        state.mod_action_presets.remove(i);
                    }
                    if ui.button("+ Add preset").clicked() {
                        state.mod_action_presets.push(crust_core::model::mod_actions::ModActionPreset {
                            label: "".into(),
                            command_template: "".into(),
                            icon_url: None,
                        });
                    }

                    // Spell check
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("Spell check")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new(
                            "Underline misspelled words in the chat input and offer\n\
                             right-click suggestions. Use \"Add to dictionary\" in the\n\
                             input's context menu to teach the checker new words.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(2.0);
                    ui.checkbox(
                        &mut state.spellcheck_enabled,
                        if ultra_compact {
                            "Enable spell check"
                        } else {
                            "Enable spell check for the chat input"
                        },
                    );

                    ui.add_enabled_ui(state.spellcheck_enabled, |ui| {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "Custom dictionary ({} word{})",
                                state.spell_custom_dict.len(),
                                if state.spell_custom_dict.len() == 1 { "" } else { "s" }
                            ))
                            .font(t::tiny())
                            .color(t::text_muted()),
                        );

                        ui.horizontal(|ui| {
                            let add_response = ui.add(
                                egui::TextEdit::singleline(&mut state.spell_custom_dict_add_buf)
                                    .hint_text("word")
                                    .desired_width(if compact { 120.0 } else { 180.0 }),
                            );
                            let enter_pressed = add_response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let add_clicked = ui
                                .button(RichText::new("Add").font(t::small()))
                                .clicked();
                            if enter_pressed || add_clicked {
                                let candidate = state
                                    .spell_custom_dict_add_buf
                                    .trim()
                                    .to_ascii_lowercase();
                                if !candidate.is_empty()
                                    && candidate.len() <= 64
                                    && candidate.chars().all(|c| c.is_ascii_alphabetic())
                                    && !state.spell_custom_dict.iter().any(|w| w == &candidate)
                                {
                                    state.spell_custom_dict.push(candidate);
                                    state.spell_custom_dict.sort();
                                }
                                state.spell_custom_dict_add_buf.clear();
                                if enter_pressed {
                                    add_response.request_focus();
                                }
                            }
                        });

                        if state.spell_custom_dict.is_empty() {
                            ui.label(
                                RichText::new("No custom words yet.")
                                    .font(t::tiny())
                                    .weak()
                                    .italics(),
                            );
                        } else {
                            let mut remove_idx: Option<usize> = None;
                            egui::ScrollArea::vertical()
                                .id_salt("spell_dict_scroll")
                                .max_height(if compact { 120.0 } else { 160.0 })
                                .show(ui, |ui| {
                                    for (idx, word) in
                                        state.spell_custom_dict.iter().enumerate()
                                    {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(word)
                                                    .font(t::small())
                                                    .color(t::text_primary()),
                                            );
                                            if ui
                                                .small_button(
                                                    RichText::new("Remove")
                                                        .font(t::tiny())
                                                        .color(t::red()),
                                                )
                                                .clicked()
                                            {
                                                remove_idx = Some(idx);
                                            }
                                        });
                                    }
                                });
                            if let Some(i) = remove_idx {
                                state.spell_custom_dict.remove(i);
                            }
                        }
                    });

                    if has_host_panels_for_slot(&state.plugin_ui, PluginUiHostSlot::SettingsChat) {
                        ui.add_space(10.0);
                        render_host_panels_for_slot(
                            ui,
                            &state.plugin_ui,
                            plugin_ui_session,
                            PluginUiHostSlot::SettingsChat,
                        );
                    }
                }
                SettingsSection::Highlights => {
                    // Highlight rules table
                    ui.label(
                        RichText::new("Highlight rules")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new("Messages matching a rule will be tinted.\nUse Alert and Sound toggles for additional feedback.")
                            .font(t::tiny())
                            .color(t::text_muted()),
                    );
                    ui.add_space(4.0);

                    // Sync buf length with rule length.
                    while state.highlight_rule_bufs.len() < state.highlight_rules.len() {
                        let pat = state
                            .highlight_rules
                            .get(state.highlight_rule_bufs.len())
                            .map(|r| r.pattern.clone())
                            .unwrap_or_default();
                        state.highlight_rule_bufs.push(pat);
                    }
                    state.highlight_rule_bufs.truncate(state.highlight_rules.len());

                    let mut delete_idx: Option<usize> = None;
                    let mut move_up_idx: Option<usize> = None;
                    let mut move_down_idx: Option<usize> = None;
                    let mut duplicate_idx: Option<usize> = None;

                    let mut open_modal_for = None;
                    let rule_count = state.highlight_rules.len();
                    for (i, rule) in state.highlight_rules.iter_mut().enumerate() {
                        ui.push_id(("highlight_rule", i), |ui| {
                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.checkbox(&mut rule.enabled, "Enabled");
                                let mut mode = super::filter_editor::EditorMode::from_highlight(&rule.effective_mode());
                                let cell = super::filter_editor::render_pattern_editor(ui, &mut mode,
                                    &mut state.highlight_rule_bufs[i], &mut rule.pattern,
                                    "Keyword, regular expression, or expression", rule.enabled);
                                rule.mode = mode.to_highlight();
                                rule.is_regex = matches!(rule.mode, crust_core::highlight::HighlightRuleMode::Regex);
                                if cell.open_modal { open_modal_for = Some(i); }
                                ui.horizontal_wrapped(|ui| {
                                    ui.checkbox(&mut rule.case_sensitive, "Case sensitive");
                                    ui.checkbox(&mut rule.has_alert, "Visual alert");
                                    ui.checkbox(&mut rule.has_sound, "Sound");
                                });
                                match render_rule_actions(ui, i, rule_count) {
                                    Some(RuleAction::MoveUp) => move_up_idx = Some(i),
                                    Some(RuleAction::MoveDown) => move_down_idx = Some(i),
                                    Some(RuleAction::Duplicate) => duplicate_idx = Some(i),
                                    Some(RuleAction::Delete) => delete_idx = Some(i),
                                    None => {}
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }

                    if let Some(idx) = duplicate_idx {
                        let clone = state.highlight_rules.get(idx).cloned();
                        if let Some(rule) = clone {
                            let buf = state.highlight_rule_bufs.get(idx).cloned().unwrap_or_default();
                            state.highlight_rules.insert(idx + 1, rule);
                            state.highlight_rule_bufs.insert(idx + 1, buf);
                        }
                    }
                    if let Some(idx) = move_up_idx {
                        if idx > 0 {
                            state.highlight_rules.swap(idx, idx - 1);
                            state.highlight_rule_bufs.swap(idx, idx - 1);
                        }
                    }
                    if let Some(idx) = move_down_idx {
                        if idx + 1 < state.highlight_rules.len() {
                            state.highlight_rules.swap(idx, idx + 1);
                            state.highlight_rule_bufs.swap(idx, idx + 1);
                        }
                    }
                    if let Some(idx) = delete_idx {
                        state.highlight_rules.remove(idx);
                        state.highlight_rule_bufs.remove(idx);
                    }
                    if let Some(idx) = open_modal_for {
                        let initial = state
                            .highlight_rule_bufs
                            .get(idx)
                            .cloned()
                            .unwrap_or_default();
                        state.filter_editor_modal.open_highlight(idx, &initial);
                    }

                    if ui.button("+ Add rule").clicked() {
                        let new_rule = HighlightRule::new("");
                        state.highlight_rule_bufs.push(String::new());
                        state.highlight_rules.push(new_rule);
                    }

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Keyword highlights")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new(
                            "Used for compatibility with older highlight lists. One per line or comma-separated.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut state.highlights_buf)
                            .desired_width(f32::INFINITY)
                            .desired_rows(if ultra_compact {
                                3
                            } else if compact {
                                4
                            } else {
                                5
                            }),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} keyword highlight(s)",
                            parse_settings_lines(&state.highlights_buf, false).len()
                        ))
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Ignored users
                    ui.label(
                        RichText::new("Ignored usernames (one per line or comma-separated)")
                            .font(t::small())
                            .color(t::text_secondary()),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut state.ignores_buf)
                            .desired_width(f32::INFINITY)
                            .desired_rows(if ultra_compact {
                                3
                            } else if compact {
                                4
                            } else {
                                6
                            }),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} ignored user(s)",
                            parse_settings_lines(&state.ignores_buf, true).len()
                        ))
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(4.0);
                }
                SettingsSection::Filters => {
                    // Filter Records table
                    ui.label(
                        RichText::new("Message filters")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new("Messages matching an active filter will be hidden or dimmed.")
                            .font(t::tiny())
                            .color(t::text_muted()),
                    );
                    ui.add_space(4.0);

                    // Sync buf length with filter length.
                    while state.filter_record_bufs.len() < state.filter_records.len() {
                        let pat = state
                            .filter_records
                            .get(state.filter_record_bufs.len())
                            .map(|r| r.pattern.clone())
                            .unwrap_or_default();
                        state.filter_record_bufs.push(pat);
                    }
                    state.filter_record_bufs.truncate(state.filter_records.len());

                    let mut filter_delete_idx: Option<usize> = None;
                    let mut filter_move_up_idx: Option<usize> = None;
                    let mut filter_move_down_idx: Option<usize> = None;
                    let mut filter_duplicate_idx: Option<usize> = None;

                    let mut open_filter_modal_for: Option<usize> = None;

                    let filter_count = state.filter_records.len();
                    for (i, filter) in state.filter_records.iter_mut().enumerate() {
                        ui.push_id(("filter_record", i), |ui| {
                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.checkbox(&mut filter.enabled, "Enabled");
                                ui.label("Name");
                                ui.add(egui::TextEdit::singleline(&mut filter.name)
                                    .desired_width(f32::INFINITY).hint_text("Filter name"));
                                let mut mode = super::filter_editor::EditorMode::from_filter(&filter.effective_mode());
                                let cell = super::filter_editor::render_pattern_editor(ui, &mut mode,
                                    &mut state.filter_record_bufs[i], &mut filter.pattern,
                                    "Keyword, regular expression, or expression", filter.enabled);
                                filter.mode = mode.to_filter();
                                filter.is_regex = matches!(filter.mode, crust_core::model::filters::FilterMode::Regex);
                                if cell.open_modal { open_filter_modal_for = Some(i); }
                                ui.checkbox(&mut filter.filter_sender, "Match the sender's username");
                                ui.horizontal_wrapped(|ui| {
                                    use crust_core::model::filters::FilterAction;
                                    ui.label("Matching messages:");
                                    ui.selectable_value(&mut filter.action, FilterAction::Hide, "Hide");
                                    ui.selectable_value(&mut filter.action, FilterAction::Dim, "Dim");
                                });
                                match render_rule_actions(ui, i, filter_count) {
                                    Some(RuleAction::MoveUp) => filter_move_up_idx = Some(i),
                                    Some(RuleAction::MoveDown) => filter_move_down_idx = Some(i),
                                    Some(RuleAction::Duplicate) => filter_duplicate_idx = Some(i),
                                    Some(RuleAction::Delete) => filter_delete_idx = Some(i),
                                    None => {}
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }

                    if let Some(idx) = filter_duplicate_idx {
                        let clone = state.filter_records.get(idx).cloned();
                        if let Some(filter) = clone {
                            let buf = state.filter_record_bufs.get(idx).cloned().unwrap_or_default();
                            state.filter_records.insert(idx + 1, filter);
                            state.filter_record_bufs.insert(idx + 1, buf);
                        }
                    }
                    if let Some(idx) = filter_move_up_idx {
                        if idx > 0 {
                            state.filter_records.swap(idx, idx - 1);
                            state.filter_record_bufs.swap(idx, idx - 1);
                        }
                    }
                    if let Some(idx) = filter_move_down_idx {
                        if idx + 1 < state.filter_records.len() {
                            state.filter_records.swap(idx, idx + 1);
                            state.filter_record_bufs.swap(idx, idx + 1);
                        }
                    }
                    if let Some(idx) = filter_delete_idx {
                        state.filter_records.remove(idx);
                        state.filter_record_bufs.remove(idx);
                    }
                    if let Some(idx) = open_filter_modal_for {
                        let initial = state
                            .filter_record_bufs
                            .get(idx)
                            .cloned()
                            .unwrap_or_default();
                        let name = state
                            .filter_records
                            .get(idx)
                            .map(|r| r.name.clone())
                            .unwrap_or_default();
                        state
                            .filter_editor_modal
                            .open_filter(idx, &initial, &name);
                    }

                    if ui.button("+ Add filter").clicked() {
                        use crust_core::model::filters::{FilterRecord, FilterScope};
                        let new_filter = FilterRecord::new("New Filter", "", FilterScope::Global);
                        state.filter_record_bufs.push(String::new());
                        state.filter_records.push(new_filter);
                    }

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "💡 Tip: set mode to ƒx for filter DSL expressions like `author.subscriber && message.content contains \"gg\"`",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                }
                SettingsSection::Nicknames => {
                    ui.label(
                        RichText::new("Nickname aliases")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new(
                            "Map a login to a custom display name.  Shown in chat, user cards, mention toasts.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(4.0);

                    let mut delete_nick_idx: Option<usize> = None;

                    for (i, nickname) in state.nicknames.iter_mut().enumerate() {
                        ui.push_id(("nickname", i), |ui| {
                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.label("Login");
                                ui.add(egui::TextEdit::singleline(&mut nickname.login)
                                    .desired_width(f32::INFINITY).hint_text("login"));
                                ui.label("Display name");
                                ui.add(egui::TextEdit::singleline(&mut nickname.nickname)
                                    .desired_width(f32::INFINITY).hint_text("Alias"));
                                ui.label("Channel");
                                let mut channel = nickname.channel.clone().unwrap_or_default();
                                if ui.add(egui::TextEdit::singleline(&mut channel)
                                    .desired_width(f32::INFINITY).hint_text("All channels")).changed() {
                                    nickname.channel = (!channel.trim().is_empty()).then(|| channel.trim().to_owned());
                                }
                                ui.horizontal_wrapped(|ui| {
                                    ui.checkbox(&mut nickname.case_sensitive, "Case-sensitive login");
                                        ui.checkbox(&mut nickname.replace_mentions, "Use nickname in mentions");
                                });
                                if ui.button(RichText::new("Remove nickname").color(t::red())).clicked() {
                                    delete_nick_idx = Some(i);
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }

                    if let Some(i) = delete_nick_idx {
                        state.nicknames.remove(i);
                    }
                    if ui.button("+ Add nickname").clicked() {
                        state
                            .nicknames
                            .push(crust_core::model::Nickname::new("", ""));
                    }
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!("{} nickname(s)", state.nicknames.len()))
                            .font(t::tiny())
                            .color(t::text_muted()),
                    );
                }
                SettingsSection::Ignores => {
                    ui.label(
                        RichText::new("Ignored users")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new(
                            "Users listed here are suppressed in chat and never trigger mentions.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(4.0);


                    let mut delete_user_idx = None;
                    for (i, user) in state.ignored_users.iter_mut().enumerate() {
                        ui.push_id(("ignored_user", i), |ui| {
                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.checkbox(&mut user.enabled, "Enabled");
                                ui.label("Login or pattern");
                                ui.add(egui::TextEdit::singleline(&mut user.login)
                                    .desired_width(f32::INFINITY).hint_text("login or regular expression"));
                                ui.horizontal_wrapped(|ui| {
                                    ui.checkbox(&mut user.is_regex, "Regular expression");
                                    ui.checkbox(&mut user.case_sensitive, "Case sensitive");
                                });
                                if user.is_regex {
                                    if let Err(error) = regex::RegexBuilder::new(&user.login)
                                        .case_insensitive(!user.case_sensitive).build() {
                                        ui.label(RichText::new(error.to_string()).font(t::tiny()).color(t::red()));
                                    }
                                }
                                if ui.button(RichText::new("Remove user").color(t::red())).clicked() {
                                    delete_user_idx = Some(i);
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }

                    if let Some(i) = delete_user_idx {
                        state.ignored_users.remove(i);
                    }
                    if ui.button("+ Add ignored user").clicked() {
                        state
                            .ignored_users
                            .push(crust_core::ignores::IgnoredUser::new(""));
                    }

                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("Ignored phrases")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.label(
                        RichText::new(
                            "Actions: Block drops the message. Replace rewrites matches. \
                             Highlight-only tints without blocking. Mention-only fires notifications.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(4.0);

                    let mut delete_phrase_idx: Option<usize> = None;


                    for (i, phrase) in state.ignored_phrases.iter_mut().enumerate() {
                        ui.push_id(("ignored_phrase", i), |ui| {
                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.checkbox(&mut phrase.enabled, "Enabled");
                                ui.label("Pattern");
                                ui.add(egui::TextEdit::singleline(&mut phrase.pattern)
                                    .desired_width(f32::INFINITY).hint_text("Phrase or regular expression"));
                                ui.horizontal_wrapped(|ui| {
                                    ui.checkbox(&mut phrase.is_regex, "Regular expression");
                                    ui.checkbox(&mut phrase.case_sensitive, "Case sensitive");
                                });
                                if !phrase.is_regex_valid() {
                                    ui.label(RichText::new("This regular expression is invalid.")
                                        .font(t::tiny()).color(t::red()));
                                }
                                use crust_core::ignores::IgnoredPhraseAction;
                                ui.label("Action");
                                egui::ComboBox::from_id_salt("phrase_action")
                                    .selected_text(match phrase.action {
                                        IgnoredPhraseAction::Block => "Block",
                                        IgnoredPhraseAction::Replace => "Replace",
                                        IgnoredPhraseAction::HighlightOnly => "Highlight only",
                                        IgnoredPhraseAction::MentionOnly => "Mention only",
                                    }).width(ui.available_width().min(240.0))
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut phrase.action, IgnoredPhraseAction::Block, "Block");
                                        ui.selectable_value(&mut phrase.action, IgnoredPhraseAction::Replace, "Replace");
                                        ui.selectable_value(&mut phrase.action, IgnoredPhraseAction::HighlightOnly, "Highlight only");
                                        ui.selectable_value(&mut phrase.action, IgnoredPhraseAction::MentionOnly, "Mention only");
                                    });
                                if matches!(phrase.action, IgnoredPhraseAction::Replace) {
                                    ui.label("Replacement");
                                    ui.add(egui::TextEdit::singleline(&mut phrase.replace_with)
                                        .desired_width(f32::INFINITY).hint_text("Replacement text"));
                                }
                                if ui.button(RichText::new("Remove phrase").color(t::red())).clicked() {
                                    delete_phrase_idx = Some(i);
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }

                    if let Some(i) = delete_phrase_idx {
                        state.ignored_phrases.remove(i);
                    }
                    if ui.button("+ Add ignored phrase").clicked() {
                        state
                            .ignored_phrases
                            .push(crust_core::ignores::IgnoredPhrase::new(""));
                    }

                    let invalid_count = state
                        .ignored_phrases
                        .iter()
                        .filter(|p| p.enabled && !p.is_regex_valid())
                        .count()
                        + state
                            .ignored_users
                            .iter()
                            .filter(|u| {
                                if !u.enabled || !u.is_regex {
                                    return false;
                                }
                                let mut b = regex::RegexBuilder::new(&u.login);
                                b.case_insensitive(!u.case_sensitive);
                                b.build().is_err()
                            })
                            .count();
                    if invalid_count > 0 {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!(
                                "⚠ {invalid_count} entry/entries have invalid regex (highlighted in red)."
                            ))
                            .font(t::tiny())
                            .color(t::red()),
                        );
                    }
                }
                SettingsSection::Commands => {
                    render_commands_section(ui, state, compact);
                }
                SettingsSection::Channels => {
                    ui.label(
                        RichText::new(
                            "Auto-join channels on startup/reconnect (one per line or comma-separated).",
                        )
                        .font(t::small())
                        .color(t::text_secondary()),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut state.auto_join_buf)
                            .desired_width(f32::INFINITY)
                            .desired_rows(if ultra_compact {
                                5
                            } else if compact {
                                7
                            } else {
                                10
                            })
                            .hint_text(
                                "twitch:channel\nkick:channel\nirc://irc.libera.chat/#rust",
                            ),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} auto-join entry(ies)",
                            parse_settings_lines(&state.auto_join_buf, false).len()
                        ))
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                }
                SettingsSection::Hotkeys => {
                    render_hotkeys_section(ui, state, compact);
                }
                SettingsSection::Notifications => {
                    render_notifications_section(ui, state, compact);
                }
                SettingsSection::StreamerMode => {
                    ui.label(
                        RichText::new(
                            "Hide sensitive on-screen info while broadcasting software is running.",
                        )
                        .font(t::small())
                        .color(t::text_secondary()),
                    );
                    ui.add_space(6.0);

                    ui.label(RichText::new("Mode").strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut state.streamer_mode, "off".to_owned(), "Off");
                        ui.radio_value(
                            &mut state.streamer_mode,
                            "auto".to_owned(),
                            "Auto (detect OBS / Streamlabs)",
                        );
                        ui.radio_value(&mut state.streamer_mode, "on".to_owned(), "Always on");
                    });

                    let active_label = if state.streamer_mode_active {
                        "Status: ACTIVE"
                    } else {
                        "Status: inactive"
                    };
                    let active_color = if state.streamer_mode_active {
                        t::accent()
                    } else {
                        t::text_muted()
                    };
                    ui.add_space(2.0);
                    ui.label(RichText::new(active_label).font(t::small()).color(active_color));

                    ui.add_space(10.0);
                    ui.label(RichText::new("When active").strong());
                    ui.checkbox(
                        &mut state.streamer_hide_link_previews,
                        "Hide link previews",
                    );
                    ui.checkbox(
                        &mut state.streamer_hide_viewer_counts,
                        "Hide viewer counts in split headers",
                    );
                    ui.checkbox(
                        &mut state.streamer_suppress_sounds,
                        "Suppress sound notifications",
                    );
                }
                SettingsSection::Integrations => {
                    ui.checkbox(&mut state.kick_beta_enabled, "Kick compatibility (beta)");
                    ui.checkbox(&mut state.irc_beta_enabled, "IRC chat compatibility (beta)");
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("User cards")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.checkbox(
                        &mut state.show_pronouns_in_usercard,
                        "Fetch and show pronouns from alejo.io",
                    );
                    ui.label(
                        RichText::new(
                            "Opens a request to api.pronouns.alejo.io when you open a user card. Off by default.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Channel points")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    ui.checkbox(
                        &mut state.auto_claim_bonus_points,
                        "Auto-claim Bonus Points",
                    );
                    ui.label(
                        RichText::new(
                            "Silently claims the Bonus Points button on every joined Twitch channel as soon as it appears. Also displays your point balance in the channel header.\n\nRequires the Twitch session token (auth-token cookie) to be set under External Tools - the chat OAuth token will not work for this. With no session token set, balance and auto-claim do nothing.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        let (badge_text, badge_col) = match state.twitch_webview_logged_in {
                            Some(true)  => ("Signed in",   t::green()),
                            Some(false) => ("Not signed in", t::red()),
                            None        => ("Unknown",      t::text_muted()),
                        };
                        ui.label(
                            RichText::new(format!("Twitch browser session: {badge_text}"))
                                .font(t::tiny())
                                .color(badge_col),
                        );
                        if ui.button("Open Twitch sign-in").clicked() {
                            state.twitch_sign_in_requested = true;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Opens an embedded browser window where you can sign in to twitch.tv. \
                             Cookies are stored under the Crust config directory and persist across restarts.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(8.0);
                    if state.irc_beta_enabled {
                        ui.label(
                            RichText::new("IRC NickServ sign-in")
                                .font(t::small())
                                .strong()
                                .color(t::text_primary()),
                        );
                        if compact {
                            ui.label("Username:");
                            ui.add(
                                egui::TextEdit::singleline(&mut state.irc_nickserv_user)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.label("Password:");
                            ui.add(
                                egui::TextEdit::singleline(&mut state.irc_nickserv_pass)
                                    .desired_width(f32::INFINITY)
                                    .password(true),
                            );
                        } else {
                            egui::Grid::new("settings_irc_auth_grid")
                                .num_columns(2)
                                .spacing(egui::vec2(8.0, 6.0))
                                .show(ui, |ui| {
                                    ui.label("Username:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut state.irc_nickserv_user)
                                            .desired_width(f32::INFINITY),
                                    );
                                    ui.end_row();

                                    ui.label("Password:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut state.irc_nickserv_pass)
                                            .desired_width(f32::INFINITY)
                                            .password(true),
                                    );
                                    ui.end_row();
                                });
                        }
                    } else {
                        ui.label(
                            RichText::new("Enable IRC beta to configure NickServ auto-identify.")
                                .font(t::tiny())
                                .color(t::text_muted()),
                        );
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Enabling beta transports may require restarting Crust.")
                            .font(t::small())
                            .color(t::text_muted()),
                    );

                }
                SettingsSection::Plugins => {
                    let (loaded_count, failed_count) = plugin_status_counts(&state.plugin_statuses);
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Reload plugins").clicked() {
                            state.plugin_reload_requested = true;
                        }
                        ui.label(RichText::new(format!("{loaded_count} loaded, {failed_count} failed"))
                            .font(t::tiny()).color(t::text_muted()));
                    });
                    if state.plugin_statuses.is_empty() {
                        ui.label(RichText::new("No plugins found in the Crust plugin directory.")
                            .font(t::small()).color(t::text_secondary()));
                    }
                    for status in &state.plugin_statuses {
                        ui.add_space(8.0);
                        render_plugin_status_card(ui, status, compact);
                    }
                    if !state.plugin_ui.settings_pages.is_empty() {
                        super::plugin_ui::render_plugin_settings_hub(ui, &state.plugin_ui, plugin_ui_session);
                    }
                    if has_host_panels_for_slot(&state.plugin_ui, PluginUiHostSlot::SettingsIntegrations) {
                        ui.add_space(10.0);
                        render_host_panels_for_slot(ui, &state.plugin_ui, plugin_ui_session,
                            PluginUiHostSlot::SettingsIntegrations);
                    }
                }
                SettingsSection::ExternalTools => {
                    ui.label(
                        RichText::new(
                            "Used by right-click -> 'Open in Streamlink' and 'Open in player' on a Twitch channel.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    // Platform-specific path hints so both Windows and Linux /
                    // macOS users see an example that matches their system.
                    let streamlink_hint = if cfg!(target_os = "windows") {
                        r"leave blank to use PATH, e.g. C:\Program Files\Streamlink\bin"
                    } else {
                        "leave blank to use PATH, e.g. /usr/bin/streamlink"
                    };
                    let mpv_hint = if cfg!(target_os = "windows") {
                        r"leave blank to use PATH, e.g. C:\Program Files\mpv\mpv.exe"
                    } else {
                        "leave blank to use PATH, e.g. /usr/bin/mpv"
                    };
                    let player_hint =
                        "{streamlink} --player {mpv} twitch.tv/{channel} {quality}";
                    if compact {
                        ui.label("Streamlink path:");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.external_streamlink_path)
                                .hint_text(streamlink_hint)
                                .desired_width(f32::INFINITY),
                        );
                        ui.label("Preferred quality:");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.external_streamlink_quality)
                                .hint_text("best")
                                .desired_width(f32::INFINITY),
                        );
                        ui.label("Extra Streamlink args:");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.external_streamlink_extra_args)
                                .hint_text("--twitch-disable-ads")
                                .desired_width(f32::INFINITY),
                        );
                        ui.label("mpv path:");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.external_mpv_path)
                                .hint_text(mpv_hint)
                                .desired_width(f32::INFINITY),
                        );
                        ui.label(
                            "Player command ({channel} / {url} / {quality} / {mpv} / {streamlink}):",
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut state.external_player_template)
                                .hint_text(player_hint)
                                .desired_width(f32::INFINITY),
                        );
                    } else {
                        egui::Grid::new("settings_external_tools_grid")
                            .num_columns(2)
                            .spacing(egui::vec2(8.0, 6.0))
                            .show(ui, |ui| {
                                ui.label("Streamlink path:");
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut state.external_streamlink_path,
                                    )
                                    .hint_text(streamlink_hint)
                                    .desired_width(f32::INFINITY),
                                );
                                ui.end_row();

                                ui.label("Quality:");
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut state.external_streamlink_quality,
                                    )
                                    .hint_text("best")
                                    .desired_width(f32::INFINITY),
                                );
                                ui.end_row();

                                ui.label("Extra args:");
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut state.external_streamlink_extra_args,
                                    )
                                    .hint_text("--twitch-disable-ads")
                                    .desired_width(f32::INFINITY),
                                );
                                ui.end_row();

                                ui.label("mpv path:");
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.external_mpv_path)
                                        .hint_text(mpv_hint)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();

                                ui.label("Player command:");
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut state.external_player_template,
                                    )
                                    .hint_text(player_hint)
                                    .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                            });
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Twitch session token (optional):")
                            .font(t::small())
                            .color(t::text_secondary()),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut state.external_streamlink_session_token)
                            .password(true)
                            .hint_text("paste your twitch.tv `auth-token` cookie")
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(
                        RichText::new(
                            "When set, Streamlink is launched with `--twitch-api-header \"Authorization=OAuth <token>\" --twitch-purge-client-integrity` so Turbo / subscriber ad-skip applies and age-gated streams play. Get the value from your browserDevTools -> Application -> Cookies -> twitch.tv -> the `auth-token` row (hex string, ~30 chars). The chat OAuth token will not work here; Twitch rejects it.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Streamlink and mpv must be installed separately. The player command is parsed with shell quoting. Variables: {channel}, {url}, {quality}, {mpv}, {streamlink}.",
                        )
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );

                }
                SettingsSection::Updates => {
                    ui.label(
                        RichText::new(format!(
                            "Current version: v{}",
                            env!("CARGO_PKG_VERSION")
                        ))
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );

                    if !(cfg!(target_os = "windows") || cfg!(target_os = "linux")) {
                        ui.label(
                            RichText::new(
                                "Auto-update is currently only supported on Windows and Debian-based Linux distributions.",
                            )
                                .font(t::tiny())
                                .color(t::text_muted()),
                        );
                    } else {
                        ui.checkbox(
                            &mut state.update_checks_enabled,
                            "Enable automatic update checks (startup + every 24h)",
                        );

                        if let Some(last_checked) = state.updater_last_checked_at.as_ref() {
                            ui.label(
                                RichText::new(format!("Last checked: {}", last_checked))
                                    .font(t::tiny())
                                    .color(t::text_muted()),
                            );
                        }

                        if !state.updater_skipped_version.trim().is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "Skipped version: {}",
                                    state.updater_skipped_version
                                ))
                                .font(t::tiny())
                                .color(t::text_muted()),
                            );
                        }

                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Check now").clicked() {
                                state.request_update_check_now = true;
                            }

                            if let Some(version) = state.updater_available_version.as_ref() {
                                let install_label = if state.updater_install_inflight {
                                    "Installing..."
                                } else {
                                    "Install update and restart"
                                };
                                if ui
                                    .add_enabled(
                                        !state.updater_install_inflight,
                                        egui::Button::new(install_label),
                                    )
                                    .clicked()
                                {
                                    state.request_update_install_now = true;
                                }

                                if ui.button("Skip this version").clicked() {
                                    state.request_skip_available_update = true;
                                }

                                if state.updater_available_release_url.is_some()
                                    && ui.button("Open release page").clicked()
                                {
                                    state.request_open_available_release = true;
                                }

                                let mut details = format!("Update available: v{}", version);
                                if let Some(asset) = state.updater_available_asset.as_ref() {
                                    details.push_str(&format!(" ({})", asset));
                                }
                                ui.label(
                                    RichText::new(details)
                                        .font(t::tiny())
                                        .color(t::text_secondary()),
                                );
                            }
                        });
                    }
                }
            }
        });
}

fn bounded_settings_scroll<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    max_height: f32,
    fill_height: bool,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let width = ui.available_width().max(0.0);
    egui::ScrollArea::both()
        .id_salt(id)
        .max_width(width)
        .max_height(max_height.max(0.0))
        .min_scrolled_width(0.0)
        .min_scrolled_height(0.0)
        .auto_shrink([false, !fill_height])
        .show(ui, |ui| {
            // Keep labels wrapping normally; oversized controls can still be
            // reached with the horizontal scrollbar instead of growing the window.
            ui.set_max_width(width);
            add_contents(ui)
        })
}

pub fn show_settings_page(
    ctx: &Context,
    settings_open: &mut bool,
    settings_section: &mut SettingsSection,
    state: &mut SettingsPageState,
    plugin_ui_session: &mut PluginUiSessionState,
    _stats: SettingsStats,
) {
    if !*settings_open || *settings_section != SettingsSection::Hotkeys {
        state.hotkey_capture_target = None;
    }
    handle_hotkey_capture(
        ctx,
        &mut state.hotkey_capture_target,
        &mut state.hotkey_bindings,
    );
    let bounds = ctx.screen_rect().shrink(8.0);
    let style = ctx.style();
    let frame = egui::Frame::window(&style);
    let heading = egui::TextStyle::Heading.resolve(&style);
    let title_height = ctx
        .fonts(|fonts| fonts.row_height(&heading))
        .max(style.spacing.interact_size.y)
        + frame.inner_margin.sum().y
        + frame.stroke.width;
    let max_size = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
        .max(egui::Vec2::splat(1.0));
    let min_size = egui::vec2(300.0, 280.0).min(max_size);
    let default_size = egui::vec2(760.0, 560.0).min(max_size);
    let settings_default_pos = (bounds.center() - default_size * 0.5).max(bounds.min);
    let navigation_id = egui::Id::new("settings_navigation_state");
    let mut navigation = ctx
        .data_mut(|data| data.get_temp::<SettingsNavigation>(navigation_id))
        .unwrap_or_default();
    egui::Window::new("Settings")
        .open(settings_open)
        .enabled(!state.filter_editor_modal.open)
        .collapsible(false)
        .resizable(true)
        .order(egui::Order::Foreground)
        .frame(frame)
        .constrain_to(bounds)
        .min_size(min_size)
        .max_size(max_size)
        .default_pos(settings_default_pos)
        .default_size(default_size)
        .show(ctx, |ui| {
            let compact_layout = ui.available_width() < 650.0;
            let ultra_compact_layout = ui.available_width() < 500.0;
            let matches = render_settings_search(ui, &mut navigation, settings_section);
            ui.label(
                RichText::new("Changes save automatically.")
                    .font(t::tiny())
                    .color(t::text_helper()),
            );
            ui.add_space(6.0);
            if matches.is_empty() {
                ui.label("No matching settings.");
                ui.label(
                    RichText::new("Try a page name or an option such as font, sound, or updates.")
                        .font(t::small())
                        .color(t::text_secondary()),
                );
                ui.allocate_space(ui.available_size().max(egui::Vec2::ZERO));
                return;
            }
            if compact_layout {
                render_sections_nav(ui, settings_section, &mut navigation, &matches, true);
                ui.add_space(8.0);
            }
            let body = ui.available_rect_before_wrap();
            ui.allocate_rect(body, egui::Sense::hover());
            let mut content_rect = body;
            if !compact_layout {
                let nav_rect =
                    egui::Rect::from_min_size(body.min, egui::vec2(170.0, body.height()));
                let mut nav_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("settings_navigation")
                        .max_rect(nav_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                bounded_settings_scroll(
                    &mut nav_ui,
                    "settings_navigation_scroll",
                    body.height(),
                    true,
                    |ui| {
                        render_sections_nav(ui, settings_section, &mut navigation, &matches, false);
                    },
                );
                content_rect.min.x = nav_rect.right() + 10.0;
            }
            // A section keeps the same parent identity in both layouts, including
            // its text fields and scroll position.
            let mut content_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("settings_section", *settings_section))
                    .max_rect(content_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            bounded_settings_scroll(
                &mut content_ui,
                "settings_content_scroll",
                content_rect.height(),
                true,
                |ui| {
                    render_settings_content(
                        ui,
                        *settings_section,
                        state,
                        plugin_ui_session,
                        content_rect.width() < 600.0,
                        ultra_compact_layout,
                    );
                },
            );
        });
    ctx.data_mut(|data| data.insert_temp(navigation_id, navigation));
    if !*settings_open {
        state.filter_editor_modal.close();
    }

    if !*settings_open || *settings_section != SettingsSection::Hotkeys {
        state.hotkey_capture_target = None;
    }

    // Render the shared advanced-expression modal (no-op when closed).
    // This is drawn above the settings window so the user can tweak the
    // expression without losing the underlying row's context.
    {
        let (hi_rules, hi_bufs, fi_records, fi_bufs) = (
            state.highlight_rules.as_mut_slice(),
            state.highlight_rule_bufs.as_mut_slice(),
            state.filter_records.as_mut_slice(),
            state.filter_record_bufs.as_mut_slice(),
        );
        state
            .filter_editor_modal
            .show(ctx, hi_rules, hi_bufs, fi_records, fi_bufs);
    }
}

#[cfg(test)]
mod responsive_tests {
    use super::*;

    struct SettingsHarness {
        ctx: Context,
        state: SettingsPageState,
        section: SettingsSection,
        plugins: PluginUiSessionState,
        open: bool,
        time: f64,
    }

    impl SettingsHarness {
        fn new() -> Self {
            Self {
                ctx: Context::default(),
                state: SettingsPageState {
                    chat_font_size: t::DEFAULT_CHAT_FONT_SIZE,
                    ui_font_size: 1.0,
                    hotkey_bindings: crust_core::HotkeyBindings::defaults(),
                    command_aliases: vec![crust_core::commands::CommandAlias {
                        trigger: "greet".to_owned(),
                        body: "Hello {1}!".to_owned(),
                        enabled: true,
                    }],
                    ..Default::default()
                },
                section: SettingsSection::Appearance,
                plugins: PluginUiSessionState::default(),
                open: true,
                time: 0.0,
            }
        }

        fn frame(&mut self, size: egui::Vec2, events: Vec<egui::Event>) -> egui::FullOutput {
            self.time += 0.1;
            self.ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    time: Some(self.time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    show_settings_page(
                        ctx,
                        &mut self.open,
                        &mut self.section,
                        &mut self.state,
                        &mut self.plugins,
                        SettingsStats {
                            highlights_count: 0,
                            ignores_count: 0,
                            auto_join_count: 0,
                        },
                    )
                },
            )
        }

        fn window_rect(&self) -> egui::Rect {
            self.ctx
                .memory(|memory| memory.area_rect(egui::Id::new("Settings")))
                .unwrap()
        }

        fn click(&mut self, size: egui::Vec2, position: egui::Pos2) {
            for pressed in [true, false] {
                self.frame(
                    size,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        }
    }

    fn visible_text_rect(output: &egui::FullOutput, wanted: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                if text.galley.text() == wanted && shape.clip_rect.contains_rect(rect) {
                    return Some(rect);
                }
            }
            None
        })
    }

    fn key_event(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn settings_search_finds_moved_options_and_clear_restores_the_previous_page() {
        let mut harness = SettingsHarness::new();
        let size = egui::vec2(1100.0, 800.0);
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(size, Vec::new());
        }
        harness.click(
            size,
            visible_text_rect(&output, "Find a setting...")
                .unwrap()
                .center(),
        );
        harness.frame(size, vec![egui::Event::Text("INSTALL UPDATE".to_owned())]);
        output = harness.frame(size, Vec::new());
        assert_eq!(harness.section, SettingsSection::Updates);
        let check =
            visible_text_rect(&output, "Check now").expect("update action is immediately visible");
        harness.click(size, check.center());
        assert!(harness.state.request_update_check_now);
        output = harness.frame(size, Vec::new());
        harness.click(size, visible_text_rect(&output, "Clear").unwrap().center());
        assert_eq!(harness.section, SettingsSection::Appearance);
        output = harness.frame(size, Vec::new());
        assert!(visible_text_rect(&output, "Theme and window").is_some());
        assert!(visible_text_rect(&output, "Advanced text sizes").is_some());
    }

    #[test]
    fn settings_search_keeps_keyboard_navigation_in_results_and_hides_unrelated_controls() {
        let mut harness = SettingsHarness::new();
        let size = egui::vec2(480.0, 640.0);
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(size, Vec::new());
        }
        harness.click(
            size,
            visible_text_rect(&output, "Find a setting...")
                .unwrap()
                .center(),
        );
        harness.frame(size, vec![egui::Event::Text("sound".to_owned())]);
        assert_eq!(harness.section, SettingsSection::Notifications);
        harness.frame(size, vec![key_event(egui::Key::ArrowDown, true)]);
        assert_eq!(harness.section, SettingsSection::StreamerMode);
        harness.frame(
            size,
            vec![
                key_event(egui::Key::ArrowDown, false),
                key_event(egui::Key::Enter, true),
            ],
        );
        assert!(!harness
            .ctx
            .memory(|memory| memory.has_focus(egui::Id::new("settings_search"))));
        output = harness.frame(size, vec![key_event(egui::Key::Enter, false)]);
        harness.click(size, visible_text_rect(&output, "Clear").unwrap().center());
        assert_eq!(harness.section, SettingsSection::StreamerMode);
        harness.frame(size, vec![egui::Event::Text("no-such-option".to_owned())]);
        output = harness.frame(size, Vec::new());
        assert!(visible_text_rect(&output, "No matching settings.").is_some());
        assert!(visible_text_rect(&output, "Suppress sound notifications").is_none());
        harness.frame(size, vec![key_event(egui::Key::Escape, true)]);
        output = harness.frame(size, vec![key_event(egui::Key::Escape, false)]);
        assert!(visible_text_rect(&output, "Suppress sound notifications").is_some());
    }

    #[test]
    fn settings_pages_restore_their_own_scroll_positions() {
        let mut harness = SettingsHarness::new();
        harness.section = SettingsSection::Chat;
        let size = egui::vec2(1100.0, 800.0);
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(size, Vec::new());
        }
        let window = harness.window_rect();
        harness.frame(
            size,
            vec![
                egui::Event::PointerMoved(egui::pos2(
                    window.right() - 80.0,
                    window.bottom() - 60.0,
                )),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -3000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        for _ in 0..30 {
            output = harness.frame(size, Vec::new());
        }
        assert!(visible_text_rect(&output, "No custom words yet.").is_some());
        harness.click(
            size,
            visible_text_rect(&output, "Appearance").unwrap().center(),
        );
        output = harness.frame(size, Vec::new());
        assert!(
            visible_text_rect(&output, "Theme and window").is_some(),
            "a new page must open at its top"
        );
        harness.click(size, visible_text_rect(&output, "Chat").unwrap().center());
        output = harness.frame(size, Vec::new());
        assert!(
            visible_text_rect(&output, "No custom words yet.").is_some(),
            "returning must restore the old page position"
        );
    }

    #[test]
    fn settings_search_focus_survives_the_responsive_breakpoint() {
        let mut harness = SettingsHarness::new();
        let wide = egui::vec2(1100.0, 800.0);
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(wide, Vec::new());
        }
        harness.click(
            wide,
            visible_text_rect(&output, "Find a setting...")
                .unwrap()
                .center(),
        );
        harness.frame(wide, vec![egui::Event::Text("external".to_owned())]);
        assert_eq!(harness.section, SettingsSection::ExternalTools);
        let narrow = egui::vec2(320.0, 420.0);
        for _ in 0..4 {
            harness.frame(narrow, Vec::new());
        }
        assert!(harness
            .ctx
            .memory(|memory| memory.has_focus(egui::Id::new("settings_search"))));
        harness.frame(narrow, vec![egui::Event::Text(" player".to_owned())]);
        assert_eq!(harness.section, SettingsSection::ExternalTools);
        assert_eq!(
            harness
                .ctx
                .data_mut(|data| data
                    .get_temp::<SettingsNavigation>(egui::Id::new("settings_navigation_state")))
                .unwrap()
                .query,
            "external player"
        );
    }

    #[test]
    fn populated_settings_editors_keep_actions_reachable_without_horizontal_scrolling() {
        let size = egui::vec2(320.0, 640.0);
        for (section, remove) in [
            (SettingsSection::Highlights, "Remove"),
            (SettingsSection::Filters, "Remove"),
            (SettingsSection::Nicknames, "Remove nickname"),
            (SettingsSection::Ignores, "Remove user"),
            (SettingsSection::Commands, "Remove alias"),
        ] {
            let mut harness = SettingsHarness::new();
            harness.section = section;
            harness.state.highlight_rules = vec![HighlightRule::new("hello")];
            harness.state.filter_records = vec![crust_core::model::filters::FilterRecord::new(
                "Test",
                "hello",
                crust_core::model::filters::FilterScope::Global,
            )];
            harness.state.nicknames = vec![crust_core::model::Nickname::new("viewer", "Friend")];
            harness.state.ignored_users = vec![crust_core::ignores::IgnoredUser::new("ignored")];
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = harness.frame(size, Vec::new());
            }
            let action = visible_text_rect(&output, remove).unwrap_or_else(|| {
                panic!("{section:?}: {remove} must be fully visible in a narrow editor")
            });
            harness.click(size, action.center());
            let empty = match section {
                SettingsSection::Highlights => harness.state.highlight_rules.is_empty(),
                SettingsSection::Filters => harness.state.filter_records.is_empty(),
                SettingsSection::Nicknames => harness.state.nicknames.is_empty(),
                SettingsSection::Ignores => harness.state.ignored_users.is_empty(),
                SettingsSection::Commands => harness.state.command_aliases.is_empty(),
                _ => unreachable!(),
            };
            assert!(empty, "{section:?} action must still edit its own list");
        }
    }

    #[test]
    fn highlight_cards_reorder_duplicate_and_remove_the_matching_edit_buffers() {
        let mut harness = SettingsHarness::new();
        harness.section = SettingsSection::Highlights;
        harness.state.highlight_rules =
            vec![HighlightRule::new("first"), HighlightRule::new("second")];
        let size = egui::vec2(480.0, 640.0);
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(size, Vec::new());
        }
        harness.click(
            size,
            visible_text_rect(&output, "Move down").unwrap().center(),
        );
        assert_eq!(harness.state.highlight_rule_bufs, ["second", "first"]);
        output = harness.frame(size, Vec::new());
        harness.click(
            size,
            visible_text_rect(&output, "Duplicate").unwrap().center(),
        );
        assert_eq!(
            harness.state.highlight_rule_bufs,
            ["second", "second", "first"]
        );
        output = harness.frame(size, Vec::new());
        harness.click(size, visible_text_rect(&output, "Remove").unwrap().center());
        assert_eq!(harness.state.highlight_rule_bufs, ["second", "first"]);
        assert_eq!(
            harness
                .state
                .highlight_rules
                .iter()
                .map(|rule| rule.pattern.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
    }

    #[test]
    fn hotkey_capture_keeps_tab_in_the_binding_and_restores_normal_focus_afterward() {
        let mut harness = SettingsHarness::new();
        harness.section = SettingsSection::Hotkeys;
        let size = egui::vec2(1100.0, 800.0);
        let action = crust_core::HotkeyAction::ZoomIn;
        let label = harness.state.hotkey_bindings.get(action).display_label();
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(size, Vec::new());
        }
        let bind = visible_text_rect(&output, &label).expect("binding button is visible");
        harness.click(size, bind.center());
        assert_eq!(harness.state.hotkey_capture_target, Some(action));
        let focused = harness.ctx.memory(|memory| memory.focused()).unwrap();
        let key = |pressed| egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        harness.frame(size, vec![key(true)]);
        assert_eq!(
            harness.state.hotkey_bindings.get(action),
            crust_core::KeyBinding::new("Tab")
        );
        assert!(harness.state.hotkey_capture_target.is_none());
        assert_eq!(harness.ctx.memory(|memory| memory.focused()), Some(focused));
        harness.frame(size, vec![key(false)]);
        harness.frame(size, vec![key(true)]);
        assert_ne!(harness.ctx.memory(|memory| memory.focused()), Some(focused));
    }

    #[test]
    fn leaving_hotkeys_or_closing_settings_cancels_pending_capture() {
        for close_window in [false, true] {
            let mut harness = SettingsHarness::new();
            harness.section = SettingsSection::Hotkeys;
            let size = egui::vec2(1100.0, 800.0);
            let action = crust_core::HotkeyAction::ZoomIn;
            let previous = harness.state.hotkey_bindings.clone();
            let label = harness.state.hotkey_bindings.get(action).display_label();
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = harness.frame(size, Vec::new());
            }
            harness.click(size, visible_text_rect(&output, &label).unwrap().center());
            assert_eq!(harness.state.hotkey_capture_target, Some(action));
            let target = if close_window {
                let window = harness.window_rect();
                let title_y = visible_text_rect(&output, "Settings").unwrap().center().y;
                egui::pos2(window.right() - (title_y - window.top()), title_y)
            } else {
                visible_text_rect(&output, "Appearance").unwrap().center()
            };
            harness.click(size, target);
            assert!(harness.state.hotkey_capture_target.is_none());
            if close_window {
                assert!(!harness.open);
            } else {
                assert_eq!(harness.section, SettingsSection::Appearance);
            }
            harness.open = true;
            harness.section = SettingsSection::Hotkeys;
            harness.frame(
                size,
                vec![egui::Event::Key {
                    key: egui::Key::K,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                }],
            );
            assert_eq!(harness.state.hotkey_bindings, previous);
        }
    }

    #[test]
    fn settings_sections_stay_inside_a_shrinking_viewport_and_close_remains_clickable() {
        let mut harness = SettingsHarness::new();
        let mut output = egui::FullOutput::default();
        let sizes = [
            egui::vec2(900.0, 700.0),
            egui::vec2(480.0, 640.0),
            egui::vec2(320.0, 300.0),
        ];
        for size in sizes {
            for section in settings_sections() {
                harness.section = section;
                for _ in 0..4 {
                    output = harness.frame(size, Vec::new());
                }
                let window = harness.window_rect();
                assert!(
                    egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(window),
                    "{section:?} window {window:?} escaped viewport {size:?}"
                );
                assert!(visible_text_rect(&output, "Settings").is_some());
            }
        }
        let window = harness.window_rect();
        let title_y = visible_text_rect(&output, "Settings").unwrap().center().y;
        let close = egui::pos2(window.right() - (title_y - window.top()), title_y);
        harness.click(*sizes.last().unwrap(), close);
        assert!(
            !harness.open,
            "Close should remain reachable after shrinking the viewport"
        );
    }

    #[test]
    fn narrow_settings_scroll_to_bottom_controls_without_moving_the_title() {
        let mut harness = SettingsHarness::new();
        let size = egui::vec2(480.0, 420.0);
        let mut output = egui::FullOutput::default();
        for _ in 0..4 {
            output = harness.frame(size, Vec::new());
        }
        assert!(visible_text_rect(&output, "Show split-header game").is_none());
        let window = harness.window_rect();
        let pointer = egui::pos2(window.center().x, window.bottom() - 60.0);
        harness.frame(
            size,
            vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -2000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        for _ in 0..30 {
            output = harness.frame(size, Vec::new());
        }
        let game = visible_text_rect(&output, "Show split-header game")
            .expect("The bottom appearance control should be reachable by scrolling");
        assert!(visible_text_rect(&output, "Settings").is_some());
        harness.click(size, game.center());
        assert!(harness.state.split_header_show_game);
    }
}
