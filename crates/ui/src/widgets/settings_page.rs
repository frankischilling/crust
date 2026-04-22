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
    Channels,
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
            Self::Channels => "Channels",
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
            Self::Channels => "Auto-join channel management",
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
    /// Opt-in fetch of pronouns from alejo.io on user profile popup.
    pub show_pronouns_in_usercard: bool,
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

fn settings_sections() -> [SettingsSection; 9] {
    [
        SettingsSection::Appearance,
        SettingsSection::Chat,
        SettingsSection::Highlights,
        SettingsSection::Filters,
        SettingsSection::Nicknames,
        SettingsSection::Ignores,
        SettingsSection::Channels,
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
                                            RichText::new("❌").font(t::tiny()).color(t::red()),
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
                    // -- Highlight rules table -----------------------------
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
                    let action_btn_size = egui::vec2(26.0, 22.0);

                    egui::Grid::new("highlight_rules_grid")
                        .num_columns(10)
                        .spacing(egui::vec2(8.0, 6.0))
                        .show(ui, |ui| {
                            // Header row
                            ui.label(RichText::new("On").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Pattern").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Re").font(t::tiny()).color(t::text_muted())
                                .strong());
                            ui.label(RichText::new("Aa").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Alert").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Sound").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.end_row();

                            for (i, rule) in state.highlight_rules.iter_mut().enumerate() {
                                // Enabled toggle
                                ui.checkbox(&mut rule.enabled, "");

                                // Pattern text field
                                let buf = &mut state.highlight_rule_bufs[i];
                                let te = egui::TextEdit::singleline(buf)
                                    .desired_width(if compact { 90.0 } else { 140.0 })
                                    .hint_text("keyword")
                                    .text_color(if rule.enabled {
                                        t::text_primary()
                                    } else {
                                        t::text_muted()
                                    });
                                let resp = ui.add(te);
                                if resp.changed() {
                                    rule.pattern = buf.clone();
                                }

                                // Regex toggle ("Re")
                                let re_col = if rule.is_regex {
                                    t::link()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Re").font(t::tiny()).color(re_col),
                                        )
                                        .min_size(action_btn_size),
                                    )
                                    .clicked()
                                {
                                    rule.is_regex = !rule.is_regex;
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

                    // -- Ignored users -------------------------------------
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
                    // -- Filter Records table ------------------------------
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

                    egui::Grid::new("filter_records_grid")
                        .num_columns(10)
                        .spacing(egui::vec2(4.0, 4.0))
                        .show(ui, |ui| {
                            // Header row
                            ui.label(RichText::new("On").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Name").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Pattern").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Re").font(t::tiny()).color(t::text_muted()).strong());
                            ui.label(RichText::new("User").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("Act").font(t::tiny()).color(t::text_muted()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.label(RichText::new("").font(t::tiny()));
                            ui.end_row();

                            for (i, filter) in state.filter_records.iter_mut().enumerate() {
                                // Enabled toggle
                                ui.checkbox(&mut filter.enabled, "");

                                // Name text field
                                ui.add(egui::TextEdit::singleline(&mut filter.name).desired_width(60.0).hint_text("Name"));

                                // Pattern text field
                                let buf = &mut state.filter_record_bufs[i];
                                let te = egui::TextEdit::singleline(buf)
                                    .desired_width(if compact { 90.0 } else { 140.0 })
                                    .hint_text("regex or keyword")
                                    .text_color(if filter.enabled {
                                        t::text_primary()
                                    } else {
                                        t::text_muted()
                                    });
                                let resp = ui.add(te);
                                if resp.changed() {
                                    filter.pattern = buf.clone();
                                }

                                // Regex toggle ("Re")
                                let re_col = if filter.is_regex {
                                    t::link()
                                } else {
                                    t::text_muted()
                                };
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Re").font(t::tiny()).color(re_col),
                                        )
                                        .min_size(egui::vec2(24.0, 20.0)),
                                    )
                                    .clicked()
                                {
                                    filter.is_regex = !filter.is_regex;
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
                                        .min_size(egui::vec2(24.0, 20.0)),
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
                                        .min_size(egui::vec2(24.0, 20.0)),
                                    )
                                    .on_hover_text("Toggle action: Hide vs Dim")
                                    .clicked()
                                {
                                    filter.action = match filter.action {
                                        FilterAction::Hide => FilterAction::Dim,
                                        FilterAction::Dim => FilterAction::Hide,
                                    };
                                }

                                // Delete button
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("🗑").font(t::tiny()).color(
                                                t::red(),
                                            ),
                                        )
                                        .min_size(egui::vec2(20.0, 20.0)),
                                    )
                                    .clicked()
                                {
                                    filter_delete_idx = Some(i);
                                }

                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("↑").font(t::tiny()).color(t::text_secondary()),
                                        )
                                        .min_size(egui::vec2(20.0, 20.0)),
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
                                        .min_size(egui::vec2(20.0, 20.0)),
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
                                        .min_size(egui::vec2(20.0, 20.0)),
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

                    if ui.button("+ Add filter").clicked() {
                        use crust_core::model::filters::{FilterRecord, FilterScope};
                        let new_filter = FilterRecord::new("New Filter", "", FilterScope::Global);
                        state.filter_record_bufs.push(String::new());
                        state.filter_records.push(new_filter);
                    }

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("💡 Tip: Use regex mode for advanced patterns like \\b(spam|scam)\\b")
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
                                            RichText::new("❌").font(t::tiny()).color(t::red()),
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
                                            RichText::new("❌").font(t::tiny()).color(t::red()),
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
                                            RichText::new("❌").font(t::tiny()).color(t::red()),
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
}
