use std::collections::{BTreeMap, BTreeSet};

use egui::{self, Color32, RichText, Ui};

use crust_core::events::AppEvent;
use crust_core::plugins::{
    plugin_host, PluginUiHostPanelRegistration, PluginUiHostSlot, PluginUiSettingsPageRegistration,
    PluginUiSnapshot, PluginUiStyle, PluginUiSurfaceKind, PluginUiValue, PluginUiWidget,
};

#[derive(Default, Clone)]
pub struct PluginUiSessionState {
    form_values: BTreeMap<String, BTreeMap<String, PluginUiValue>>,
}

impl PluginUiSessionState {
    pub fn prune_missing_surfaces(&mut self, snapshot: &PluginUiSnapshot) {
        let mut keep = BTreeSet::new();
        for window in &snapshot.windows {
            keep.insert(surface_key(
                &window.plugin_name,
                PluginUiSurfaceKind::Window,
                &window.window.id,
            ));
        }
        for page in &snapshot.settings_pages {
            keep.insert(surface_key(
                &page.plugin_name,
                PluginUiSurfaceKind::SettingsPage,
                &page.page.id,
            ));
        }
        for panel in &snapshot.host_panels {
            keep.insert(surface_key(
                &panel.plugin_name,
                PluginUiSurfaceKind::HostPanel,
                &panel.panel.id,
            ));
        }
        self.form_values.retain(|key, _| keep.contains(key));
    }

    fn surface_values(&self, surface_key: &str) -> BTreeMap<String, PluginUiValue> {
        self.form_values
            .get(surface_key)
            .cloned()
            .unwrap_or_default()
    }

    fn get_value(&self, surface_key: &str, field: &str) -> Option<PluginUiValue> {
        self.form_values
            .get(surface_key)
            .and_then(|values| values.get(field))
            .cloned()
    }

    fn set_value(&mut self, surface_key: &str, field: &str, value: PluginUiValue) {
        self.form_values
            .entry(surface_key.to_owned())
            .or_default()
            .insert(field.to_owned(), value);
    }
}

pub fn show_plugin_windows(
    ctx: &egui::Context,
    snapshot: &PluginUiSnapshot,
    session: &mut PluginUiSessionState,
) {
    for registration in &snapshot.windows {
        if !visible(&registration.window.style) || !registration.window.open {
            continue;
        }
        let surface_key = surface_key(
            &registration.plugin_name,
            PluginUiSurfaceKind::Window,
            &registration.window.id,
        );
        let title = format!(
            "{}: {}",
            registration.plugin_name, registration.window.title
        );
        let mut open = registration.window.open;
        let mut window = egui::Window::new(title)
            .id(egui::Id::new((
                "plugin_window",
                &registration.plugin_name,
                &registration.window.id,
            )))
            .open(&mut open)
            .resizable(registration.window.resizable)
            .default_width(registration.window.default_width.unwrap_or(420.0))
            .default_height(registration.window.default_height.unwrap_or(320.0))
            .order(egui::Order::Foreground);
        if let Some(min_width) = registration.window.min_width {
            window = window.min_width(min_width);
        }
        if let Some(min_height) = registration.window.min_height {
            window = window.min_height(min_height);
        }
        if let Some(max_width) = registration.window.max_width {
            window = window.max_width(max_width);
        }
        if let Some(max_height) = registration.window.max_height {
            window = window.max_height(max_height);
        }
        window.show(ctx, |ui| {
            ui.add_enabled_ui(enabled(&registration.window.style), |ui| {
                if registration.window.scroll {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        render_children(
                            ui,
                            &registration.plugin_name,
                            PluginUiSurfaceKind::Window,
                            &registration.window.id,
                            &registration.window.children,
                            session,
                            &surface_key,
                        );
                    });
                } else {
                    render_children(
                        ui,
                        &registration.plugin_name,
                        PluginUiSurfaceKind::Window,
                        &registration.window.id,
                        &registration.window.children,
                        session,
                        &surface_key,
                    );
                }
            });
        });
        if !open {
            if let Some(host) = plugin_host() {
                host.set_plugin_window_open(
                    &registration.plugin_name,
                    &registration.window.id,
                    false,
                );
                host.dispatch_event(&AppEvent::PluginUiWindowClosed {
                    plugin_name: registration.plugin_name.clone(),
                    window_id: registration.window.id.clone(),
                });
            }
        }
    }
}

pub fn render_plugin_settings_hub(
    ui: &mut Ui,
    snapshot: &PluginUiSnapshot,
    session: &mut PluginUiSessionState,
) {
    if snapshot.settings_pages.is_empty() {
        ui.label(
            RichText::new("No plugin settings pages registered.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        return;
    }
    ui.add_space(8.0);
    ui.heading("Plugin Settings");
    ui.label(
        RichText::new("Registered plugin settings pages appear here.")
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);
    for registration in &snapshot.settings_pages {
        if !visible(&registration.page.style) {
            continue;
        }
        render_settings_page_registration(ui, registration, session);
        ui.add_space(8.0);
    }
}

pub fn has_host_panels_for_slot(snapshot: &PluginUiSnapshot, slot: PluginUiHostSlot) -> bool {
    snapshot
        .host_panels
        .iter()
        .any(|panel| panel.panel.slot == slot && visible(&panel.panel.style))
}

pub fn render_host_panels_for_slot(
    ui: &mut Ui,
    snapshot: &PluginUiSnapshot,
    session: &mut PluginUiSessionState,
    slot: PluginUiHostSlot,
) {
    let mut panels: Vec<_> = snapshot
        .host_panels
        .iter()
        .filter(|panel| panel.panel.slot == slot && visible(&panel.panel.style))
        .collect();
    panels.sort_by(|left, right| {
        left.panel
            .order
            .cmp(&right.panel.order)
            .then_with(|| left.plugin_name.cmp(&right.plugin_name))
            .then_with(|| left.panel.id.cmp(&right.panel.id))
    });

    for (index, panel) in panels.into_iter().enumerate() {
        if index > 0 {
            ui.add_space(8.0);
        }
        render_host_panel_registration(ui, panel, session);
    }
}

fn render_settings_page_registration(
    ui: &mut Ui,
    registration: &PluginUiSettingsPageRegistration,
    session: &mut PluginUiSessionState,
) {
    let surface_key = surface_key(
        &registration.plugin_name,
        PluginUiSurfaceKind::SettingsPage,
        &registration.page.id,
    );
    let mut frame = egui::Frame::group(ui.style());
    if let Some(fill) = color_from_style(&registration.page.style, ui) {
        frame.fill = fill;
    }
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            ui.heading(format!(
                "{} / {}",
                registration.plugin_name, registration.page.title
            ));
            if let Some(summary) = registration.page.summary.as_deref() {
                ui.label(
                    RichText::new(summary)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            ui.add_space(6.0);
            ui.add_enabled_ui(enabled(&registration.page.style), |ui| {
                render_children(
                    ui,
                    &registration.plugin_name,
                    PluginUiSurfaceKind::SettingsPage,
                    &registration.page.id,
                    &registration.page.children,
                    session,
                    &surface_key,
                );
            });
        });
    });
}

fn render_host_panel_registration(
    ui: &mut Ui,
    registration: &PluginUiHostPanelRegistration,
    session: &mut PluginUiSessionState,
) {
    let surface_key = surface_key(
        &registration.plugin_name,
        PluginUiSurfaceKind::HostPanel,
        &registration.panel.id,
    );
    let mut frame = egui::Frame::group(ui.style());
    if let Some(fill) = color_from_style(&registration.panel.style, ui) {
        frame.fill = fill;
    }
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            let heading = match registration.panel.title.as_deref() {
                Some(title) if !title.trim().is_empty() => {
                    format!("{} / {}", registration.plugin_name, title)
                }
                _ => registration.plugin_name.clone(),
            };
            ui.label(RichText::new(heading).strong());
            if let Some(summary) = registration.panel.summary.as_deref() {
                if !summary.trim().is_empty() {
                    ui.label(
                        RichText::new(summary)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
            }
            ui.add_space(6.0);
            ui.add_enabled_ui(enabled(&registration.panel.style), |ui| {
                render_children(
                    ui,
                    &registration.plugin_name,
                    PluginUiSurfaceKind::HostPanel,
                    &registration.panel.id,
                    &registration.panel.children,
                    session,
                    &surface_key,
                );
            });
        });
    });
}

fn render_children(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    children: &[PluginUiWidget],
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    for widget in children {
        render_widget(
            ui,
            plugin_name,
            surface_kind,
            surface_id,
            widget,
            session,
            surface_key_value,
        );
    }
}

fn render_widget(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    if !visible(&widget.style) {
        return;
    }

    match widget.kind.as_str() {
        "column" => {
            ui.vertical(|ui| {
                render_children(
                    ui,
                    plugin_name,
                    surface_kind,
                    surface_id,
                    &widget.children,
                    session,
                    surface_key_value,
                );
            });
        }
        "row" => {
            ui.horizontal_wrapped(|ui| {
                render_children(
                    ui,
                    plugin_name,
                    surface_kind,
                    surface_id,
                    &widget.children,
                    session,
                    surface_key_value,
                );
            });
        }
        "group" | "card" => {
            let mut frame = egui::Frame::group(ui.style());
            if let Some(fill) = color_from_style(&widget.style, ui) {
                frame.fill = fill;
            }
            frame.show(ui, |ui| {
                if let Some(title) = widget.title.as_deref().or(widget.text.as_deref()) {
                    ui.label(styled_text(title, &widget.style, ui));
                    ui.add_space(4.0);
                }
                render_children(
                    ui,
                    plugin_name,
                    surface_kind,
                    surface_id,
                    &widget.children,
                    session,
                    surface_key_value,
                );
            });
        }
        "grid" => {
            let id = widget
                .id
                .clone()
                .unwrap_or_else(|| format!("grid:{plugin_name}:{surface_id}"));
            egui::Grid::new(id).show(ui, |ui| {
                render_children(
                    ui,
                    plugin_name,
                    surface_kind,
                    surface_id,
                    &widget.children,
                    session,
                    surface_key_value,
                );
            });
        }
        "scroll" => {
            egui::ScrollArea::vertical().show(ui, |ui| {
                render_children(
                    ui,
                    plugin_name,
                    surface_kind,
                    surface_id,
                    &widget.children,
                    session,
                    surface_key_value,
                );
            });
        }
        "separator" => {
            ui.separator();
        }
        "spacer" => {
            ui.add_space(
                widget
                    .style
                    .height
                    .or(widget.style.min_height)
                    .unwrap_or(8.0),
            );
        }
        "collapsible" => {
            let title = widget
                .title
                .as_deref()
                .or(widget.text.as_deref())
                .unwrap_or("Details");
            egui::CollapsingHeader::new(title)
                .default_open(widget.open.unwrap_or(true))
                .show(ui, |ui| {
                    render_children(
                        ui,
                        plugin_name,
                        surface_kind,
                        surface_id,
                        &widget.children,
                        session,
                        surface_key_value,
                    );
                });
        }
        "text" | "label" => {
            if let Some(text) = widget.text.as_deref().or(widget.title.as_deref()) {
                ui.label(styled_text(text, &widget.style, ui));
            }
        }
        "heading" => {
            if let Some(text) = widget.text.as_deref().or(widget.title.as_deref()) {
                ui.heading(text);
            }
        }
        "badge" => {
            let text = widget
                .text
                .as_deref()
                .or(widget.title.as_deref())
                .unwrap_or("Badge");
            let fill =
                color_from_style(&widget.style, ui).unwrap_or(ui.visuals().selection.bg_fill);
            egui::Frame::new()
                .fill(fill)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(8, 3))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).strong());
                });
        }
        "image" => {
            if let Some(url) = widget.url.as_deref().or(widget.style.image_url.as_deref()) {
                let image = egui::Image::new(url)
                    .max_width(widget.style.width.unwrap_or(128.0))
                    .max_height(widget.style.height.unwrap_or(128.0));
                ui.add(image);
            }
        }
        "progress" => {
            let progress = widget
                .progress
                .or_else(|| match widget.value.as_ref() {
                    Some(PluginUiValue::Number(value)) => Some(*value as f32),
                    _ => None,
                })
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let mut bar = egui::ProgressBar::new(progress);
            if let Some(text) = widget.text.as_deref() {
                bar = bar.text(text);
            }
            ui.add_sized(
                [
                    widget.style.width.unwrap_or(ui.available_width()),
                    widget.style.height.unwrap_or(18.0),
                ],
                bar,
            );
        }
        "button" | "icon_button" | "link_button" => {
            render_action_widget(
                ui,
                plugin_name,
                surface_kind,
                surface_id,
                widget,
                session,
                surface_key_value,
            );
        }
        "text_input" | "text_area" | "password_input" => {
            render_text_input(
                ui,
                plugin_name,
                surface_kind,
                surface_id,
                widget,
                session,
                surface_key_value,
            );
        }
        "checkbox" | "toggle" => {
            render_bool_input(
                ui,
                plugin_name,
                surface_kind,
                surface_id,
                widget,
                session,
                surface_key_value,
            );
        }
        "radio_group" => {
            render_radio_group(
                ui,
                plugin_name,
                surface_kind,
                surface_id,
                widget,
                session,
                surface_key_value,
            );
        }
        "select" => {
            render_select(
                ui,
                plugin_name,
                surface_kind,
                surface_id,
                widget,
                session,
                surface_key_value,
            );
        }
        "slider" => {
            render_slider(
                ui,
                plugin_name,
                surface_kind,
                surface_id,
                widget,
                session,
                surface_key_value,
            );
        }
        "list" => {
            for item in &widget.items {
                let mut line = format!("• {}", item.label);
                if let Some(value) = item.value.as_deref() {
                    line.push_str(": ");
                    line.push_str(value);
                }
                if let Some(note) = item.note.as_deref() {
                    line.push_str(" — ");
                    line.push_str(note);
                }
                ui.label(line);
            }
        }
        "table" => {
            let id = widget
                .id
                .clone()
                .unwrap_or_else(|| format!("table:{plugin_name}:{surface_id}"));
            egui::Grid::new(id).striped(true).show(ui, |ui| {
                if !widget.columns.is_empty() {
                    for column in &widget.columns {
                        ui.label(RichText::new(&column.title).strong());
                    }
                    ui.end_row();
                }
                for row in &widget.rows {
                    for cell in row {
                        match cell {
                            PluginUiValue::String(value) => {
                                ui.label(value);
                            }
                            PluginUiValue::Bool(value) => {
                                ui.label(if *value { "true" } else { "false" });
                            }
                            PluginUiValue::Number(value) => {
                                ui.label(format!("{value}"));
                            }
                            PluginUiValue::Strings(values) => {
                                ui.label(values.join(", "));
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        }
        _ => {}
    }
}

fn render_action_widget(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    let text = widget
        .text
        .as_deref()
        .or(widget.title.as_deref())
        .or(widget.style.icon.as_deref())
        .unwrap_or("Action");
    let response = if widget.kind == "link_button" {
        ui.add_enabled_ui(enabled(&widget.style), |ui| {
            ui.hyperlink_to(text, widget.url.as_deref().unwrap_or("#"))
        })
        .inner
    } else {
        let button = egui::Button::new(text);
        match (widget.style.width, widget.style.height) {
            (Some(width), Some(height)) => {
                ui.add_enabled_ui(enabled(&widget.style), |ui| {
                    ui.add_sized([width, height], button)
                })
                .inner
            }
            (Some(width), None) => {
                ui.add_enabled_ui(enabled(&widget.style), |ui| {
                    ui.add_sized([width, 24.0], button)
                })
                .inner
            }
            (None, Some(height)) => {
                ui.add_enabled_ui(enabled(&widget.style), |ui| {
                    ui.add_sized([ui.available_width(), height], button)
                })
                .inner
            }
            (None, None) => ui.add_enabled(enabled(&widget.style), button),
        }
    };
    if response.clicked() {
        let event = if widget.submit {
            AppEvent::PluginUiSubmit {
                plugin_name: plugin_name.to_owned(),
                surface_kind,
                surface_id: surface_id.to_owned(),
                widget_id: widget.id.clone(),
                action: widget.action.clone(),
                form_values: session.surface_values(surface_key_value),
            }
        } else {
            AppEvent::PluginUiAction {
                plugin_name: plugin_name.to_owned(),
                surface_kind,
                surface_id: surface_id.to_owned(),
                widget_id: widget.id.clone().unwrap_or_else(|| "action".to_owned()),
                action: widget.action.clone(),
                value: widget.value.clone(),
                form_values: session.surface_values(surface_key_value),
            }
        };
        dispatch_plugin_event(event);
    }
}

fn render_text_input(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    let Some(key) = widget_field_key(widget) else {
        return;
    };
    let mut value = current_string_value(session, surface_key_value, &key, widget);
    let mut edit = if widget.kind == "text_area" {
        egui::TextEdit::multiline(&mut value)
    } else {
        egui::TextEdit::singleline(&mut value)
    };
    if let Some(placeholder) = widget.placeholder.as_deref() {
        edit = edit.hint_text(placeholder);
    }
    if widget.kind == "password_input" {
        edit = edit.password(true);
    }
    let response = if let Some(width) = widget.style.width {
        ui.add_enabled_ui(enabled(&widget.style), |ui| {
            ui.add_sized([width, widget.style.height.unwrap_or(24.0)], edit)
        })
        .inner
    } else {
        ui.add_enabled_ui(enabled(&widget.style), |ui| ui.add(edit))
            .inner
    };
    if response.changed() {
        on_value_changed(
            plugin_name,
            surface_kind,
            surface_id,
            widget,
            PluginUiValue::String(value),
            session,
            surface_key_value,
        );
    }
}

fn render_bool_input(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    let Some(key) = widget_field_key(widget) else {
        return;
    };
    let mut value = current_bool_value(session, surface_key_value, &key, widget);
    let label = widget
        .text
        .as_deref()
        .or(widget.title.as_deref())
        .unwrap_or("");
    let response = if widget.kind == "toggle" {
        ui.add_enabled_ui(enabled(&widget.style), |ui| {
            ui.toggle_value(&mut value, label)
        })
        .inner
    } else {
        ui.add_enabled_ui(enabled(&widget.style), |ui| ui.checkbox(&mut value, label))
            .inner
    };
    if response.changed() {
        on_value_changed(
            plugin_name,
            surface_kind,
            surface_id,
            widget,
            PluginUiValue::Bool(value),
            session,
            surface_key_value,
        );
    }
}

fn render_radio_group(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    let Some(key) = widget_field_key(widget) else {
        return;
    };
    let mut selected = current_string_value(session, surface_key_value, &key, widget);
    ui.add_enabled_ui(enabled(&widget.style), |ui| {
        for option in &widget.options {
            if ui
                .radio_value(&mut selected, option.value.clone(), &option.label)
                .changed()
            {
                on_value_changed(
                    plugin_name,
                    surface_kind,
                    surface_id,
                    widget,
                    PluginUiValue::String(selected.clone()),
                    session,
                    surface_key_value,
                );
            }
        }
    });
}

fn render_select(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    let Some(key) = widget_field_key(widget) else {
        return;
    };
    let mut selected = current_string_value(session, surface_key_value, &key, widget);
    ui.add_enabled_ui(enabled(&widget.style), |ui| {
        egui::ComboBox::from_id_salt(("plugin_select", plugin_name, surface_id, &key))
            .selected_text(
                widget
                    .options
                    .iter()
                    .find(|option| option.value == selected)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| selected.clone()),
            )
            .show_ui(ui, |ui| {
                for option in &widget.options {
                    if ui
                        .selectable_value(&mut selected, option.value.clone(), &option.label)
                        .changed()
                    {
                        on_value_changed(
                            plugin_name,
                            surface_kind,
                            surface_id,
                            widget,
                            PluginUiValue::String(selected.clone()),
                            session,
                            surface_key_value,
                        );
                    }
                }
            });
    });
}

fn render_slider(
    ui: &mut Ui,
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    let Some(key) = widget_field_key(widget) else {
        return;
    };
    let mut value = current_number_value(session, surface_key_value, &key, widget);
    let min = widget.min.unwrap_or(0.0);
    let max = widget.max.unwrap_or(100.0);
    let mut slider = egui::Slider::new(&mut value, min..=max).text(
        widget
            .text
            .as_deref()
            .or(widget.title.as_deref())
            .unwrap_or(""),
    );
    if let Some(step) = slider_step(widget) {
        slider = slider.step_by(step);
    }
    let response = ui
        .add_enabled_ui(enabled(&widget.style), |ui| ui.add(slider))
        .inner;
    if response.changed() {
        on_value_changed(
            plugin_name,
            surface_kind,
            surface_id,
            widget,
            PluginUiValue::Number(value),
            session,
            surface_key_value,
        );
    }
}

fn on_value_changed(
    plugin_name: &str,
    surface_kind: PluginUiSurfaceKind,
    surface_id: &str,
    widget: &PluginUiWidget,
    value: PluginUiValue,
    session: &mut PluginUiSessionState,
    surface_key_value: &str,
) {
    if widget.host_form {
        if let Some(key) = widget_field_key(widget) {
            session.set_value(surface_key_value, &key, value.clone());
        }
    }
    let form_values = session.surface_values(surface_key_value);
    dispatch_plugin_event(AppEvent::PluginUiChange {
        plugin_name: plugin_name.to_owned(),
        surface_kind,
        surface_id: surface_id.to_owned(),
        widget_id: widget.id.clone().unwrap_or_else(|| "value".to_owned()),
        value,
        form_values,
    });
}

fn dispatch_plugin_event(event: AppEvent) {
    if let Some(host) = plugin_host() {
        host.dispatch_event(&event);
    }
}

fn current_string_value(
    session: &PluginUiSessionState,
    surface_key_value: &str,
    key: &str,
    widget: &PluginUiWidget,
) -> String {
    match current_widget_value(session, surface_key_value, key, widget) {
        PluginUiValue::String(value) => value,
        PluginUiValue::Bool(value) => value.to_string(),
        PluginUiValue::Number(value) => value.to_string(),
        PluginUiValue::Strings(values) => values.join(", "),
    }
}

fn current_bool_value(
    session: &PluginUiSessionState,
    surface_key_value: &str,
    key: &str,
    widget: &PluginUiWidget,
) -> bool {
    match current_widget_value(session, surface_key_value, key, widget) {
        PluginUiValue::Bool(value) => value,
        PluginUiValue::String(value) => value == "true",
        PluginUiValue::Number(value) => value != 0.0,
        PluginUiValue::Strings(values) => !values.is_empty(),
    }
}

fn current_number_value(
    session: &PluginUiSessionState,
    surface_key_value: &str,
    key: &str,
    widget: &PluginUiWidget,
) -> f64 {
    match current_widget_value(session, surface_key_value, key, widget) {
        PluginUiValue::Number(value) => value,
        PluginUiValue::String(value) => value.parse().unwrap_or(0.0),
        PluginUiValue::Bool(value) => {
            if value {
                1.0
            } else {
                0.0
            }
        }
        PluginUiValue::Strings(_) => 0.0,
    }
}

fn current_widget_value(
    session: &PluginUiSessionState,
    surface_key_value: &str,
    key: &str,
    widget: &PluginUiWidget,
) -> PluginUiValue {
    session
        .get_value(surface_key_value, key)
        .or_else(|| widget.value.clone())
        .unwrap_or_else(|| PluginUiValue::String(String::new()))
}

fn widget_field_key(widget: &PluginUiWidget) -> Option<String> {
    widget.form_key.clone().or_else(|| widget.id.clone())
}

fn slider_step(widget: &PluginUiWidget) -> Option<f64> {
    widget.step.filter(|step| *step > 0.0)
}

fn surface_key(plugin_name: &str, surface_kind: PluginUiSurfaceKind, surface_id: &str) -> String {
    let kind = match surface_kind {
        PluginUiSurfaceKind::Window => "window",
        PluginUiSurfaceKind::SettingsPage => "settings",
        PluginUiSurfaceKind::HostPanel => "host_panel",
    };
    format!("{plugin_name}:{kind}:{surface_id}")
}

fn visible(style: &PluginUiStyle) -> bool {
    style.visible.unwrap_or(true)
}

fn enabled(style: &PluginUiStyle) -> bool {
    style.enabled.unwrap_or(true)
}

fn styled_text(text: &str, style: &PluginUiStyle, ui: &Ui) -> RichText {
    let mut rich = RichText::new(text);
    if matches!(style.text_role.as_deref(), Some("muted")) {
        rich = rich.color(ui.visuals().weak_text_color());
    }
    if matches!(style.emphasis.as_deref(), Some("strong") | Some("bold")) {
        rich = rich.strong();
    }
    if matches!(style.emphasis.as_deref(), Some("small")) {
        rich = rich.small();
    }
    if let Some(color) = color_from_style(style, ui) {
        rich = rich.color(color);
    }
    rich
}

fn color_from_style(style: &PluginUiStyle, ui: &Ui) -> Option<Color32> {
    if let Some(fill) = style.fill_color.as_deref().and_then(parse_hex_color) {
        return Some(fill);
    }
    match style.severity.as_deref() {
        Some("info") => Some(ui.visuals().selection.bg_fill),
        Some("success") => Some(Color32::from_rgb(44, 160, 88)),
        Some("warning") => Some(Color32::from_rgb(196, 146, 39)),
        Some("danger") | Some("error") => Some(Color32::from_rgb(186, 68, 68)),
        _ => None,
    }
}

fn parse_hex_color(input: &str) -> Option<Color32> {
    let value = input.trim().trim_start_matches('#');
    match value.len() {
        6 => {
            let r = u8::from_str_radix(&value[0..2], 16).ok()?;
            let g = u8::from_str_radix(&value[2..4], 16).ok()?;
            let b = u8::from_str_radix(&value[4..6], 16).ok()?;
            Some(Color32::from_rgb(r, g, b))
        }
        8 => {
            let r = u8::from_str_radix(&value[0..2], 16).ok()?;
            let g = u8::from_str_radix(&value[2..4], 16).ok()?;
            let b = u8::from_str_radix(&value[4..6], 16).ok()?;
            let a = u8::from_str_radix(&value[6..8], 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(r, g, b, a))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::plugins::{PluginUiWindowRegistration, PluginUiWindowSpec};

    fn widget_with_value(value: Option<PluginUiValue>) -> PluginUiWidget {
        PluginUiWidget {
            kind: "text_input".into(),
            id: Some("field".into()),
            value,
            ..PluginUiWidget::default()
        }
    }

    #[test]
    fn session_state_prunes_removed_surfaces() {
        let mut state = PluginUiSessionState::default();
        state.set_value(
            "demo:window:alpha",
            "field",
            PluginUiValue::String("x".into()),
        );
        state.set_value(
            "demo:settings:beta",
            "field",
            PluginUiValue::String("y".into()),
        );

        let snapshot = PluginUiSnapshot {
            windows: vec![PluginUiWindowRegistration {
                plugin_name: "demo".into(),
                window: PluginUiWindowSpec {
                    id: "alpha".into(),
                    title: "Alpha".into(),
                    ..PluginUiWindowSpec::default()
                },
            }],
            settings_pages: vec![],
            host_panels: vec![],
        };

        state.prune_missing_surfaces(&snapshot);

        assert!(state.form_values.contains_key("demo:window:alpha"));
        assert!(!state.form_values.contains_key("demo:settings:beta"));
    }

    #[test]
    fn plugin_ui_session_state_keeps_host_panels() {
        let mut state = PluginUiSessionState::default();
        state.set_value(
            "demo:host_panel:appearance_tools",
            "field",
            PluginUiValue::String("kept".into()),
        );

        let snapshot = PluginUiSnapshot {
            windows: vec![],
            settings_pages: vec![],
            host_panels: vec![crust_core::plugins::PluginUiHostPanelRegistration {
                plugin_name: "demo".into(),
                panel: crust_core::plugins::PluginUiHostPanelSpec {
                    id: "appearance_tools".into(),
                    slot: crust_core::plugins::PluginUiHostSlot::SettingsAppearance,
                    ..crust_core::plugins::PluginUiHostPanelSpec::default()
                },
            }],
        };

        state.prune_missing_surfaces(&snapshot);

        assert!(state
            .form_values
            .contains_key("demo:host_panel:appearance_tools"));
    }

    #[test]
    fn surface_key_matches_window_and_settings_shapes() {
        assert_eq!(
            surface_key("demo", PluginUiSurfaceKind::Window, "alpha"),
            "demo:window:alpha"
        );
        assert_eq!(
            surface_key("demo", PluginUiSurfaceKind::SettingsPage, "beta"),
            "demo:settings:beta"
        );
        assert_eq!(
            surface_key("demo", PluginUiSurfaceKind::HostPanel, "gamma"),
            "demo:host_panel:gamma"
        );
    }

    #[test]
    fn current_widget_value_prefers_session_state_over_retained_value() {
        let mut state = PluginUiSessionState::default();
        state.set_value(
            "demo:window:alpha",
            "field",
            PluginUiValue::String("session".into()),
        );

        let value = current_widget_value(
            &state,
            "demo:window:alpha",
            "field",
            &widget_with_value(Some(PluginUiValue::String("retained".into()))),
        );

        assert_eq!(value, PluginUiValue::String("session".into()));
    }

    #[test]
    fn current_widget_value_falls_back_to_retained_value() {
        let state = PluginUiSessionState::default();

        let value = current_widget_value(
            &state,
            "demo:window:alpha",
            "field",
            &widget_with_value(Some(PluginUiValue::String("retained".into()))),
        );

        assert_eq!(value, PluginUiValue::String("retained".into()));
    }

    #[test]
    fn visibility_and_enabled_helpers_default_true_and_honor_false() {
        assert!(visible(&PluginUiStyle::default()));
        assert!(enabled(&PluginUiStyle::default()));

        assert!(!visible(&PluginUiStyle {
            visible: Some(false),
            ..PluginUiStyle::default()
        }));
        assert!(!enabled(&PluginUiStyle {
            enabled: Some(false),
            ..PluginUiStyle::default()
        }));
    }

    #[test]
    fn slider_step_uses_positive_values_only() {
        let widget = PluginUiWidget {
            kind: "slider".into(),
            step: Some(2.5),
            ..PluginUiWidget::default()
        };
        assert_eq!(slider_step(&widget), Some(2.5));

        let zero = PluginUiWidget {
            kind: "slider".into(),
            step: Some(0.0),
            ..PluginUiWidget::default()
        };
        assert_eq!(slider_step(&zero), None);

        let negative = PluginUiWidget {
            kind: "slider".into(),
            step: Some(-3.0),
            ..PluginUiWidget::default()
        };
        assert_eq!(slider_step(&negative), None);
    }
}
