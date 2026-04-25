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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
            Self::Integrations => "Integrations",
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
            Self::Integrations => "Plugins, Kick/IRC beta, and NickServ",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SettingsStats {
    pub highlights_count: usize,
    pub ignores_count: usize,
    pub auto_join_count: usize,
}

#[derive(Clone, Debug)]
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
    /// UI scale ratio fed to `pixels_per_point`.
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
    /// capture mode. The renderer polls egui input events to assign the
    /// next key press to this action.
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
            "Click a row's button, then press the key combination you want. Press Escape to cancel capture. Use Reset to restore defaults.",
        )
        .font(t::small())
        .color(t::text_secondary()),
    );
    ui.add_space(6.0);

    // Poll for a captured key press if a row is in capture mode. Done
    // *before* drawing rows so the newly-assigned label paints this frame.
    if let Some(target) = state.hotkey_capture_target {
        let captured = ui.ctx().input(|i| capture_keybinding(i));
        if let Some(capture) = captured {
            match capture {
                CaptureOutcome::Cancel => {
                    state.hotkey_capture_target = None;
                }
                CaptureOutcome::Binding(binding) => {
                    state.hotkey_bindings.set(target, binding);
                    state.hotkey_capture_target = None;
                }
            }
        } else {
            ui.ctx().request_repaint();
        }
    }

    let mut reset_all = false;
    ui.horizontal(|ui| {
        if ui
            .button(RichText::new("Reset all to defaults").font(t::small()))
            .clicked()
        {
            reset_all = true;
        }
        let conflicts = state.hotkey_bindings.conflicts();
        if !conflicts.is_empty() {
            ui.label(
                RichText::new(format!(
                    "{} binding{} conflict",
                    conflicts.len(),
                    if conflicts.len() == 1 { "" } else { "s" }
                ))
                .font(t::small())
                .color(t::red())
                .strong(),
            );
        }
    });
    if reset_all {
        state.hotkey_bindings = HotkeyBindings::defaults();
        state.hotkey_capture_target = None;
    }
    ui.add_space(4.0);

    for category in HotkeyCategory::all() {
        ui.label(
            RichText::new(category.display_name())
                .font(t::body())
                .strong()
                .color(t::text_primary()),
        );
        ui.add_space(2.0);

        egui::Grid::new(("hotkeys_grid", category.as_str()))
            .num_columns(4)
            .striped(true)
            .spacing(egui::vec2(10.0, 6.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Action")
                        .font(t::tiny())
                        .color(t::text_muted()),
                );
                ui.label(
                    RichText::new("Binding")
                        .font(t::tiny())
                        .color(t::text_muted()),
                );
                ui.label(RichText::new("").font(t::tiny()));
                ui.label(RichText::new("").font(t::tiny()));
                ui.end_row();

                for action in HotkeyAction::all() {
                    if action.category() != category {
                        continue;
                    }
                    let current = state.hotkey_bindings.get(action);
                    let capture_active = state.hotkey_capture_target == Some(action);
                    let conflict = state.hotkey_bindings.find_conflict(action, &current);

                    let action_color = if conflict.is_some() {
                        t::red()
                    } else {
                        t::text_primary()
                    };
                    ui.label(
                        RichText::new(action.display_name())
                            .font(t::small())
                            .color(action_color),
                    );

                    let button_label = if capture_active {
                        "Press any key...".to_owned()
                    } else {
                        current.display_label()
                    };
                    let button_color = if capture_active {
                        t::accent()
                    } else if conflict.is_some() {
                        t::red()
                    } else if current.is_unbound() {
                        t::text_muted()
                    } else {
                        t::text_primary()
                    };
                    let btn = ui.button(
                        RichText::new(button_label)
                            .font(t::small())
                            .color(button_color),
                    );
                    if btn.clicked() {
                        state.hotkey_capture_target = if capture_active {
                            None
                        } else {
                            Some(action)
                        };
                    }

                    if ui
                        .button(RichText::new("Clear").font(t::tiny()))
                        .on_hover_text("Unbind this shortcut")
                        .clicked()
                    {
                        state.hotkey_bindings.set(action, KeyBinding::default());
                        if state.hotkey_capture_target == Some(action) {
                            state.hotkey_capture_target = None;
                        }
                    }
                    if ui
                        .button(RichText::new("Reset").font(t::tiny()))
                        .on_hover_text("Reset this binding to the default")
                        .clicked()
                    {
                        state
                            .hotkey_bindings
                            .set(action, HotkeyBindings::defaults().get(action));
                        if state.hotkey_capture_target == Some(action) {
                            state.hotkey_capture_target = None;
                        }
                    }

                    ui.end_row();

                    if let Some(other) = conflict {
                        ui.label("");
                        ui.label(
                            RichText::new(format!(
                                "Conflicts with: {}",
                                other.display_name()
                            ))
                            .font(t::tiny())
                            .color(t::red()),
                        );
                        ui.label("");
                        ui.label("");
                        ui.end_row();
                    }
                }
            });

        ui.add_space(if compact { 6.0 } else { 10.0 });
    }
}

/// Result of polling egui input for a hotkey-capture gesture.
enum CaptureOutcome {
    /// User pressed Escape -> cancel capture, leave binding untouched.
    Cancel,
    /// User pressed a non-modifier key -> assign it as the new binding.
    Binding(crust_core::KeyBinding),
}

/// Inspect the current egui input frame for a fresh key-down event and
/// translate it into a [`CaptureOutcome`]. Skips pure modifier presses so
/// users can hold modifiers without prematurely committing the binding.
fn capture_keybinding(input: &egui::InputState) -> Option<CaptureOutcome> {
    for event in &input.events {
        if let egui::Event::Key {
            key,
            pressed: true,
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
fn render_notifications_section(
    ui: &mut egui::Ui,
    state: &mut SettingsPageState,
    compact: bool,
) {
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
        RichText::new(
            "Also fires on whispers from other users. Windows users: requires the PowerShell toast fallback.",
        )
        .font(t::tiny())
        .color(t::text_muted()),
    );

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
            ui.horizontal(|ui| {
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
fn render_commands_section(ui: &mut egui::Ui, state: &mut SettingsPageState, compact: bool) {
    use crust_core::commands::CommandAlias;

    ui.label(
        RichText::new("Custom Command Aliases")
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

    let action_btn_size = egui::vec2(26.0, 22.0);
    let mut delete_alias_idx: Option<usize> = None;

    egui::Grid::new("command_aliases_grid")
        .num_columns(5)
        .spacing(egui::vec2(8.0, 6.0))
        .show(ui, |ui| {
            ui.label(RichText::new("On").font(t::tiny()).color(t::text_muted()));
            ui.label(
                RichText::new("Trigger")
                    .font(t::tiny())
                    .color(t::text_muted()),
            );
            ui.label(RichText::new("Body").font(t::tiny()).color(t::text_muted()));
            ui.label(RichText::new(" ").font(t::tiny()));
            ui.label(RichText::new(" ").font(t::tiny()));
            ui.end_row();

            for (i, alias) in state.command_aliases.iter_mut().enumerate() {
                ui.checkbox(&mut alias.enabled, "");

                let canonical = alias.canonical_trigger();
                let is_duplicate = !canonical.is_empty()
                    && duplicate_triggers.contains(&canonical);
                let is_invalid = !alias.is_valid();
                let trigger_color = if is_duplicate || is_invalid {
                    t::red()
                } else if alias.enabled {
                    t::text_primary()
                } else {
                    t::text_muted()
                };
                ui.add(
                    egui::TextEdit::singleline(&mut alias.trigger)
                        .desired_width(if compact { 90.0 } else { 130.0 })
                        .text_color(trigger_color)
                        .hint_text("hi"),
                );

                let body_color = if alias.enabled {
                    t::text_primary()
                } else {
                    t::text_muted()
                };
                ui.add(
                    egui::TextEdit::singleline(&mut alias.body)
                        .desired_width(if compact { 240.0 } else { 360.0 })
                        .text_color(body_color)
                        .hint_text("/me says hi {1} {2+}"),
                );

                // Live preview pill: shows the expansion for a fixed sample
                // input so the user can sanity-check variable placement
                // without sending a test message.
                let preview_args = "alice bob how are you";
                let preview_input = if canonical.is_empty() {
                    String::new()
                } else {
                    format!("/{canonical} {preview_args}")
                };
                let preview_text = if preview_input.is_empty() || !alias.is_valid() {
                    String::new()
                } else {
                    match crust_core::commands::expand_command_aliases(
                        &preview_input,
                        std::slice::from_ref(alias),
                        "forsen",
                        "me",
                    ) {
                        Ok(crust_core::commands::AliasExpansion::Expanded {
                            text, ..
                        }) => text,
                        _ => String::new(),
                    }
                };
                if preview_text.is_empty() {
                    ui.label(RichText::new(" ").font(t::tiny()));
                } else {
                    ui.label(
                        RichText::new(format!("⇒ {preview_text}"))
                            .font(t::tiny())
                            .color(t::text_muted()),
                    )
                    .on_hover_text(format!(
                        "Sample input: {preview_input}\n\
                         Expansion:    {preview_text}",
                    ));
                }

                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("").font(t::tiny()).color(t::red()),
                        )
                        .min_size(action_btn_size),
                    )
                    .on_hover_text("Delete alias")
                    .clicked()
                {
                    delete_alias_idx = Some(i);
                }
                ui.end_row();
            }
        });

    if let Some(i) = delete_alias_idx {
        state.command_aliases.remove(i);
    }

    if ui.button("+ Add alias").clicked() {
        state
            .command_aliases
            .push(CommandAlias::new("", "/me "));
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
        RichText::new(format!("{} alias(es) defined.", state.command_aliases.len()))
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

fn settings_sections() -> [SettingsSection; 12] {
    [
        SettingsSection::Appearance,
        SettingsSection::Chat,
        SettingsSection::Highlights,
        SettingsSection::Filters,
        SettingsSection::Nicknames,
        SettingsSection::Ignores,
        SettingsSection::Commands,
        SettingsSection::Channels,
        SettingsSection::Hotkeys,
        SettingsSection::Notifications,
        SettingsSection::StreamerMode,
        SettingsSection::Integrations,
    ]
}

fn plugin_status_counts(statuses: &[PluginStatus]) -> (usize, usize) {
    let loaded = statuses.iter().filter(|status| status.loaded).count();
    let failed = statuses.len().saturating_sub(loaded);
    (loaded, failed)
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
        .stroke(egui::Stroke::new(1.0, t::border_subtle()))
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

fn render_sections_nav(
    ui: &mut egui::Ui,
    settings_section: &mut SettingsSection,
    stats: SettingsStats,
    compact: bool,
    ultra_compact: bool,
) {
    let section_margin = if ultra_compact { 6 } else { 10 };
    egui::Frame::new()
        .fill(t::bg_surface())
        .stroke(egui::Stroke::new(1.0, t::border_subtle()))
        .inner_margin(Margin::same(section_margin))
        .show(ui, |ui| {
            ui.label(
                RichText::new("Sections")
                    .font(t::small())
                    .strong()
                    .color(t::text_primary()),
            );
            ui.add_space(8.0);

            if compact {
                if ultra_compact {
                    egui::ComboBox::from_id_salt("settings_section_combo")
                        .selected_text(settings_section.title())
                        .width(ui.available_width().max(120.0))
                        .show_ui(ui, |ui| {
                            for section in settings_sections() {
                                ui.selectable_value(settings_section, section, section.title());
                            }
                        });
                } else {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                        for section in settings_sections() {
                            let selected = *settings_section == section;
                            let title = if selected {
                                RichText::new(section.title())
                                    .font(t::small())
                                    .strong()
                                    .color(t::text_primary())
                            } else {
                                RichText::new(section.title())
                                    .font(t::small())
                                    .color(t::text_secondary())
                            };
                            if ui.selectable_label(selected, title).clicked() {
                                *settings_section = section;
                            }
                        }
                    });
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(settings_section.subtitle())
                        .font(t::tiny())
                        .color(t::text_muted()),
                );
            } else {
                for section in settings_sections() {
                    let selected = *settings_section == section;
                    let title = if selected {
                        RichText::new(section.title())
                            .font(t::body())
                            .strong()
                            .color(t::text_primary())
                    } else {
                        RichText::new(section.title())
                            .font(t::body())
                            .color(t::text_secondary())
                    };
                    let resp = ui.selectable_label(selected, title);
                    if resp.clicked() {
                        *settings_section = section;
                    }
                    ui.add_space(1.0);
                    ui.label(
                        RichText::new(section.subtitle())
                            .font(t::tiny())
                            .color(t::text_muted()),
                    );
                    ui.add_space(8.0);
                }
            }
        });

    ui.add_space(8.0);
    let stats_label = if ultra_compact {
        format!(
            "{} hl • {} ign • {} join",
            stats.highlights_count, stats.ignores_count, stats.auto_join_count
        )
    } else {
        format!(
            "{} highlights, {} ignored, {} auto-join",
            stats.highlights_count, stats.ignores_count, stats.auto_join_count
        )
    };
    ui.label(
        RichText::new(stats_label)
            .font(t::tiny())
            .color(t::text_muted()),
    );
    ui.label(
        RichText::new(if ultra_compact {
            "Tip: one value per line"
        } else {
            "Tip: put one value per line for easier editing."
        })
        .font(t::tiny())
        .color(t::text_muted()),
    );
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
            chrome::dialog_header(
                ui,
                settings_section.title(),
                (!ultra_compact).then_some(settings_section.subtitle()),
            );
            ui.add_space(10.0);

            match settings_section {
                SettingsSection::Appearance => {
                    ui.label(
                        RichText::new("Theme and Shell")
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
                            .color(t::text_muted()),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Section sizes (uncheck Auto to override)")
                            .font(t::small())
                            .strong()
                            .color(t::text_secondary()),
                    );
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
                        ui.horizontal(|ui| {
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
                                .color(t::text_muted()),
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Tabs and Split Headers")
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
                        RichText::new("Moderation Action Presets")
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
                                            RichText::new("").font(t::tiny()).color(t::red()),
                                        )
                                        .min_size(egui::vec2(24.0, 20.0)),
                                    )
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
                        RichText::new("Highlight Rules")
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
                    let action_btn_size =
                        egui::vec2(26.0, super::filter_editor::ROW_BTN_HEIGHT);

                    let mut open_modal_for: Option<usize> = None;
                    egui::Grid::new("highlight_rules_grid")
                        .num_columns(11)
                        .spacing(egui::vec2(6.0, 6.0))
                        .min_row_height(super::filter_editor::ROW_BTN_HEIGHT)
                        .show(ui, |ui| {
                            fn hdr(ui: &mut egui::Ui, s: &str) {
                                ui.label(
                                    RichText::new(s)
                                        .font(t::tiny())
                                        .color(t::text_muted())
                                        .strong(),
                                );
                            }
                            hdr(ui, "On");
                            hdr(ui, "Mode");
                            hdr(ui, "Pattern");
                            hdr(ui, "Edit");
                            hdr(ui, "Aa");
                            hdr(ui, "Alert");
                            hdr(ui, "Sound");
                            hdr(ui, "Del");
                            hdr(ui, "↑");
                            hdr(ui, "↓");
                            hdr(ui, "Dup");
                            ui.end_row();

                            for (i, rule) in state.highlight_rules.iter_mut().enumerate() {
                                // Enabled toggle
                                ui.checkbox(&mut rule.enabled, "");

                                // Mode cycler + pattern cell (Aa / .* / ƒx).
                                let mut mode = super::filter_editor::EditorMode::from_highlight(
                                    &rule.effective_mode(),
                                );
                                let buf = &mut state.highlight_rule_bufs[i];
                                let cell = super::filter_editor::render_pattern_cell(
                                    ui,
                                    &mut mode,
                                    buf,
                                    &mut rule.pattern,
                                    "keyword or expression",
                                    rule.enabled,
                                    compact,
                                );
                                rule.mode = mode.to_highlight();
                                // Reset legacy `is_regex` when picking a non-regex mode so
                                // we don't accidentally re-interpret as regex later.
                                rule.is_regex = matches!(
                                    rule.mode,
                                    crust_core::highlight::HighlightRuleMode::Regex
                                );
                                if cell.open_modal {
                                    open_modal_for = Some(i);
                                }

                                // Case-sensitive toggle ("Aa")
                                let aa_col = if rule.case_sensitive {
                                    t::yellow()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Aa").font(t::tiny()).color(aa_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    rule.case_sensitive = !rule.case_sensitive;
                                }

                                // Visual alert toggle
                                let alert_col = if rule.has_alert {
                                    t::bits_orange()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("⚠").font(t::tiny()).color(alert_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Show visual alert/flash on match")
                                    .clicked()
                                {
                                    rule.has_alert = !rule.has_alert;
                                }

                                // Sound notification toggle
                                let sound_col = if rule.has_sound {
                                    t::green()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("🔊").font(t::tiny()).color(sound_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Play sound notification on match")
                                    .clicked()
                                {
                                    rule.has_sound = !rule.has_sound;
                                }

                                // Delete button
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("🗑").font(t::tiny()).color(
                                                t::red(),
                                            ),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    delete_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("↑").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Move rule up")
                                    .clicked()
                                {
                                    move_up_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("↓").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Move rule down")
                                    .clicked()
                                {
                                    move_down_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("⎘").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Duplicate rule")
                                    .clicked()
                                {
                                    duplicate_idx = Some(i);
                                }

                                ui.end_row();
                            }
                        });

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
                        RichText::new("Keyword Highlights (Legacy)")
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
                        RichText::new("Filter Records (Hide/Dim Messages)")
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
                    let filter_btn_size =
                        egui::vec2(26.0, super::filter_editor::ROW_BTN_HEIGHT);
                    egui::Grid::new("filter_records_grid")
                        .num_columns(11)
                        .spacing(egui::vec2(6.0, 6.0))
                        .min_row_height(super::filter_editor::ROW_BTN_HEIGHT)
                        .show(ui, |ui| {
                            fn hdr(ui: &mut egui::Ui, s: &str) {
                                ui.label(
                                    RichText::new(s)
                                        .font(t::tiny())
                                        .color(t::text_muted())
                                        .strong(),
                                );
                            }
                            hdr(ui, "On");
                            hdr(ui, "Name");
                            hdr(ui, "Mode");
                            hdr(ui, "Pattern");
                            hdr(ui, "Edit");
                            hdr(ui, "User");
                            hdr(ui, "Act");
                            hdr(ui, "Del");
                            hdr(ui, "↑");
                            hdr(ui, "↓");
                            hdr(ui, "Dup");
                            ui.end_row();

                            for (i, filter) in state.filter_records.iter_mut().enumerate() {
                                // Enabled toggle
                                ui.checkbox(&mut filter.enabled, "");

                                // Name text field
                                ui.add(
                                    egui::TextEdit::singleline(&mut filter.name)
                                        .desired_width(72.0)
                                        .min_size(egui::vec2(
                                            72.0,
                                            super::filter_editor::ROW_BTN_HEIGHT,
                                        ))
                                        .hint_text("Name"),
                                );

                                // Mode cycler + pattern cell
                                let mut mode = super::filter_editor::EditorMode::from_filter(
                                    &filter.effective_mode(),
                                );
                                let buf = &mut state.filter_record_bufs[i];
                                let cell = super::filter_editor::render_pattern_cell(
                                    ui,
                                    &mut mode,
                                    buf,
                                    &mut filter.pattern,
                                    "regex, keyword, or expression",
                                    filter.enabled,
                                    compact,
                                );
                                filter.mode = mode.to_filter();
                                filter.is_regex = matches!(
                                    filter.mode,
                                    crust_core::model::filters::FilterMode::Regex
                                );
                                if cell.open_modal {
                                    open_filter_modal_for = Some(i);
                                }

                                // Filter sender toggle
                                let user_col = if filter.filter_sender {
                                    t::bits_orange()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("👤").font(t::tiny()).color(user_col),
                                        )
                                        .min_size(filter_btn_size),
                                    )
                                    .on_hover_text("Filter by username instead of message content")
                                    .clicked()
                                {
                                    filter.filter_sender = !filter.filter_sender;
                                }

                                // Action toggle (Hide/Dim)
                                use crust_core::model::filters::FilterAction;
                                let action_text = match filter.action {
                                    FilterAction::Hide => "🚫",
                                    FilterAction::Dim => "🔅",
                                };
                                let action_col = match filter.action {
                                    FilterAction::Hide => t::red(),
                                    FilterAction::Dim => t::text_secondary(),
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(action_text).font(t::tiny()).color(action_col),
                                        )
                                        .min_size(filter_btn_size),
                                    )
                                    .on_hover_text("Toggle action: Hide vs Dim")
                                    .clicked()
                                {
                                    filter.action = match filter.action {
                                        FilterAction::Hide => FilterAction::Dim,
                                        FilterAction::Dim => FilterAction::Hide,
                                    };
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("🗑").font(t::tiny()).color(t::red()),
                                        )
                                        .min_size(filter_btn_size),
                                    )
                                    .on_hover_text("Delete filter")
                                    .clicked()
                                {
                                    filter_delete_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("↑").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(filter_btn_size),
                                    )
                                    .on_hover_text("Move filter up")
                                    .clicked()
                                {
                                    filter_move_up_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("↓").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(filter_btn_size),
                                    )
                                    .on_hover_text("Move filter down")
                                    .clicked()
                                {
                                    filter_move_down_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("⎘").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(filter_btn_size),
                                    )
                                    .on_hover_text("Duplicate filter")
                                    .clicked()
                                {
                                    filter_duplicate_idx = Some(i);
                                }

                                ui.end_row();
                            }
                        });

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
                        RichText::new("Nickname Aliases")
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
                    let action_btn_size = egui::vec2(26.0, 22.0);

                    egui::Grid::new("nicknames_grid")
                        .num_columns(6)
                        .spacing(egui::vec2(8.0, 6.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new("Login").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Alias").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Channel").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Aa").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("@").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.end_row();

                            for (i, n) in state.nicknames.iter_mut().enumerate() {
                                ui.add(
                                    egui::TextEdit::singleline(&mut n.login)
                                        .desired_width(if compact { 90.0 } else { 140.0 })
                                        .hint_text("login"),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut n.nickname)
                                        .desired_width(if compact { 90.0 } else { 140.0 })
                                        .hint_text("Alias"),
                                );
                                let mut channel_buf = n.channel.clone().unwrap_or_default();
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut channel_buf)
                                        .desired_width(if compact { 80.0 } else { 120.0 })
                                        .hint_text("all channels"),
                                );
                                if resp.changed() {
                                    let trimmed = channel_buf.trim();
                                    n.channel = if trimmed.is_empty() {
                                        None
                                    } else {
                                        Some(trimmed.to_owned())
                                    };
                                }

                                let aa_col = if n.case_sensitive {
                                    t::yellow()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Aa").font(t::tiny()).color(aa_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Case-sensitive login match")
                                    .clicked()
                                {
                                    n.case_sensitive = !n.case_sensitive;
                                }

                                let mention_col = if n.replace_mentions {
                                    t::link()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("@").font(t::tiny()).color(mention_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .on_hover_text("Treat alias as self-mention")
                                    .clicked()
                                {
                                    n.replace_mentions = !n.replace_mentions;
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("").font(t::tiny()).color(t::red()),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    delete_nick_idx = Some(i);
                                }
                                ui.end_row();
                            }
                        });

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
                        RichText::new("Ignored Users")
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

                    let action_btn_size = egui::vec2(26.0, 22.0);
                    let mut delete_user_idx: Option<usize> = None;

                    egui::Grid::new("ignored_users_grid")
                        .num_columns(5)
                        .spacing(egui::vec2(8.0, 6.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new("On").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Login / regex").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Re").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Aa").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.end_row();

                            for (i, u) in state.ignored_users.iter_mut().enumerate() {
                                ui.checkbox(&mut u.enabled, "");
                                let regex_ok = if u.is_regex {
                                    let mut b = regex::RegexBuilder::new(&u.login);
                                    b.case_insensitive(!u.case_sensitive);
                                    b.build().is_ok()
                                } else {
                                    true
                                };
                                let text_color = if !regex_ok {
                                    t::red()
                                } else if u.enabled {
                                    t::text_primary()
                                } else {
                                    t::text_muted()
                                };
                                ui.add(
                                    egui::TextEdit::singleline(&mut u.login)
                                        .desired_width(if compact { 140.0 } else { 200.0 })
                                        .text_color(text_color)
                                        .hint_text("login or regex"),
                                );
                                let re_col = if u.is_regex { t::link() } else { t::text_muted() };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Re").font(t::tiny()).color(re_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    u.is_regex = !u.is_regex;
                                }
                                let aa_col = if u.case_sensitive {
                                    t::yellow()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Aa").font(t::tiny()).color(aa_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    u.case_sensitive = !u.case_sensitive;
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("").font(t::tiny()).color(t::red()),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    delete_user_idx = Some(i);
                                }
                                ui.end_row();
                            }
                        });

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
                        RichText::new("Ignored Phrases")
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

                    egui::Grid::new("ignored_phrases_grid")
                        .num_columns(7)
                        .spacing(egui::vec2(8.0, 6.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new("On").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Pattern").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Re").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Aa").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Action").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Replace").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.end_row();

                            for (i, p) in state.ignored_phrases.iter_mut().enumerate() {
                                ui.checkbox(&mut p.enabled, "");
                                let regex_ok = p.is_regex_valid();
                                let text_color = if !regex_ok {
                                    t::red()
                                } else if p.enabled {
                                    t::text_primary()
                                } else {
                                    t::text_muted()
                                };
                                ui.add(
                                    egui::TextEdit::singleline(&mut p.pattern)
                                        .desired_width(if compact { 120.0 } else { 180.0 })
                                        .text_color(text_color)
                                        .hint_text("pattern"),
                                );
                                let re_col = if p.is_regex { t::link() } else { t::text_muted() };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Re").font(t::tiny()).color(re_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    p.is_regex = !p.is_regex;
                                }
                                let aa_col = if p.case_sensitive {
                                    t::yellow()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Aa").font(t::tiny()).color(aa_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    p.case_sensitive = !p.case_sensitive;
                                }

                                egui::ComboBox::from_id_salt(("phrase_action", i))
                                    .selected_text(match p.action {
                                        crust_core::ignores::IgnoredPhraseAction::Block => "Block",
                                        crust_core::ignores::IgnoredPhraseAction::Replace => "Replace",
                                        crust_core::ignores::IgnoredPhraseAction::HighlightOnly => "Highlight only",
                                        crust_core::ignores::IgnoredPhraseAction::MentionOnly => "Mention only",
                                    })
                                    .width(if compact { 90.0 } else { 120.0 })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut p.action,
                                            crust_core::ignores::IgnoredPhraseAction::Block,
                                            "Block",
                                        );
                                        ui.selectable_value(
                                            &mut p.action,
                                            crust_core::ignores::IgnoredPhraseAction::Replace,
                                            "Replace",
                                        );
                                        ui.selectable_value(
                                            &mut p.action,
                                            crust_core::ignores::IgnoredPhraseAction::HighlightOnly,
                                            "Highlight only",
                                        );
                                        ui.selectable_value(
                                            &mut p.action,
                                            crust_core::ignores::IgnoredPhraseAction::MentionOnly,
                                            "Mention only",
                                        );
                                    });

                                let replace_enabled = matches!(
                                    p.action,
                                    crust_core::ignores::IgnoredPhraseAction::Replace
                                );
                                ui.add_enabled_ui(replace_enabled, |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut p.replace_with)
                                            .desired_width(if compact { 60.0 } else { 80.0 })
                                            .hint_text("***"),
                                    );
                                });
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("").font(t::tiny()).color(t::red()),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    delete_phrase_idx = Some(i);
                                }
                                ui.end_row();
                            }
                        });

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
                    ui.horizontal(|ui| {
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
                        RichText::new("User Cards")
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
                        RichText::new("Channel Points")
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
                    ui.horizontal(|ui| {
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
                    ui.label(
                        RichText::new("Lua Plugins")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
                    let (loaded_count, failed_count) = plugin_status_counts(&state.plugin_statuses);
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Reload plugins").clicked() {
                            state.plugin_reload_requested = true;
                        }
                        ui.label(
                            RichText::new(format!(
                                "{} loaded, {} failed",
                                loaded_count, failed_count
                            ))
                            .font(t::tiny())
                            .color(t::text_muted()),
                        );
                    });
                    if state.plugin_statuses.is_empty() {
                        ui.label(
                            RichText::new("No plugins found in the Crust plugin directory.")
                                .font(t::tiny())
                                .color(t::text_muted()),
                        );
                    } else {
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height(if compact { 220.0 } else { 280.0 })
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 8.0;
                                for status in &state.plugin_statuses {
                                    render_plugin_status_card(ui, status, compact);
                                }
                            });
                    }
                    if !state.plugin_ui.settings_pages.is_empty() {
                        super::plugin_ui::render_plugin_settings_hub(
                            ui,
                            &state.plugin_ui,
                            plugin_ui_session,
                        );
                    }
                    if has_host_panels_for_slot(
                        &state.plugin_ui,
                        PluginUiHostSlot::SettingsIntegrations,
                    ) {
                        ui.add_space(10.0);
                        render_host_panels_for_slot(
                            ui,
                            &state.plugin_ui,
                            plugin_ui_session,
                            PluginUiHostSlot::SettingsIntegrations,
                        );
                    }
                    ui.add_space(8.0);
                    if state.irc_beta_enabled {
                        ui.label(
                            RichText::new("IRC NickServ Auto-Identify")
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

                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("External Tools")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
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

                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Updates")
                            .font(t::small())
                            .strong()
                            .color(t::text_primary()),
                    );
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

                            if ui.button("Test gifted sub alert").clicked() {
                                state.request_test_gifted_sub_alert = true;
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

pub fn show_settings_page(
    ctx: &Context,
    settings_open: &mut bool,
    settings_section: &mut SettingsSection,
    state: &mut SettingsPageState,
    plugin_ui_session: &mut PluginUiSessionState,
    stats: SettingsStats,
) {
    let screen = ctx.screen_rect();
    let settings_default_pos = egui::pos2(
        (screen.center().x - 380.0).max(8.0),
        (screen.center().y - 280.0).max(8.0),
    );
    let min_w = 300.0_f32.min((screen.width() - 16.0).max(160.0));
    let min_h = 280.0_f32.min((screen.height() - 16.0).max(160.0));
    let default_w = 760.0_f32.min((screen.width() - 16.0).max(min_w));
    let default_h = 560.0_f32.min((screen.height() - 16.0).max(min_h));
    egui::Window::new("Settings")
        .open(settings_open)
        .collapsible(false)
        .resizable(true)
        .order(egui::Order::Foreground)
        .default_pos(settings_default_pos)
        .default_size(egui::vec2(default_w, default_h))
        .show(ctx, |ui| {
            ui.set_min_size(egui::vec2(min_w, min_h));
            let compact_layout = ui.available_width() < 720.0;
            let ultra_compact_layout = ui.available_width() < 500.0;
            ui.vertical(|ui| {
                chrome::dialog_header(
                    ui,
                    "Crust Settings",
                    (!ultra_compact_layout).then_some("Changes apply and save automatically."),
                );
            });
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            if compact_layout {
                render_sections_nav(ui, settings_section, stats, true, ultra_compact_layout);
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        render_settings_content(
                            ui,
                            *settings_section,
                            state,
                            plugin_ui_session,
                            true,
                            ultra_compact_layout,
                        );
                    });
            } else {
                ui.columns(2, |cols| {
                    let nav = &mut cols[0];
                    nav.set_min_width(170.0);
                    render_sections_nav(nav, settings_section, stats, false, false);

                    let content = &mut cols[1];
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(content, |ui| {
                            render_settings_content(
                                ui,
                                *settings_section,
                                state,
                                plugin_ui_session,
                                false,
                                false,
                            );
                        });
                });
            }
        });

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
