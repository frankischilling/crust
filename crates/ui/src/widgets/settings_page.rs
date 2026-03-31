use std::collections::HashSet;

use egui::{Context, Margin, RichText};

use crate::app::{ChannelLayout, TabVisualStyle};
use crate::theme as t;

use super::chrome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    Appearance,
    Chat,
    Filters,
    Channels,
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
            Self::Filters => "Filters",
            Self::Channels => "Channels",
            Self::Integrations => "Integrations",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Appearance => "Theme and window behavior",
            Self::Chat => "Message rendering and input limits",
            Self::Filters => "Highlights and ignored users",
            Self::Channels => "Auto-join channel management",
            Self::Integrations => "Kick/IRC beta and NickServ",
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
    pub highlights_buf: String,
    pub ignores_buf: String,
    pub auto_join_buf: String,
    pub light_theme: bool,
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

fn settings_sections() -> [SettingsSection; 5] {
    [
        SettingsSection::Appearance,
        SettingsSection::Chat,
        SettingsSection::Filters,
        SettingsSection::Channels,
        SettingsSection::Integrations,
    ]
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
                    ui.add_enabled_ui(state.irc_beta_enabled, |ui| {
                        ui.checkbox(&mut state.irc_status_visible, "Show IRC status panel");
                    });
                    if !state.irc_beta_enabled {
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
                }
                SettingsSection::Chat => {
                    ui.checkbox(&mut state.show_timestamps, "Show message timestamps");
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
                }
                SettingsSection::Filters => {
                    ui.label(
                        RichText::new("Highlight keywords (one per line or comma-separated)")
                            .font(t::small())
                            .color(t::text_secondary()),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut state.highlights_buf)
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
                            "{} keyword(s)",
                            parse_settings_lines(&state.highlights_buf, false).len()
                        ))
                        .font(t::tiny())
                        .color(t::text_muted()),
                    );
                    ui.add_space(8.0);
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
                SettingsSection::Integrations => {
                    ui.checkbox(&mut state.kick_beta_enabled, "Kick compatibility (beta)");
                    ui.checkbox(&mut state.irc_beta_enabled, "IRC chat compatibility (beta)");
                    ui.add_space(8.0);
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
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Enabling beta transports may require restarting Crust.")
                            .font(t::small())
                            .color(t::text_muted()),
                    );
                }
            }
        });
}

pub fn show_settings_page(
    ctx: &Context,
    settings_open: &mut bool,
    settings_section: &mut SettingsSection,
    state: &mut SettingsPageState,
    stats: SettingsStats,
) {
    let settings_default_pos = egui::pos2(
        (ctx.screen_rect().center().x - 380.0).max(8.0),
        (ctx.screen_rect().center().y - 280.0).max(8.0),
    );
    egui::Window::new("Settings")
        .open(settings_open)
        .collapsible(false)
        .resizable(true)
        .order(egui::Order::Foreground)
        .default_pos(settings_default_pos)
        .default_size(egui::vec2(760.0, 560.0))
        .show(ctx, |ui| {
            ui.set_min_size(egui::vec2(300.0, 280.0));
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
                            render_settings_content(ui, *settings_section, state, false, false);
                        });
                });
            }
        });
}
