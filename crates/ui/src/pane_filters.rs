use crust_core::model::{ChannelId, CompiledFilter, FilterRecord};
use egui::{Context, Id, RichText};

use crate::theme as t;

#[derive(Clone, Default)]
pub(super) struct PaneFilters {
    /// None follows all enabled rules; Some selects rules by their saved names.
    pub selected: Option<Vec<String>>,
    pub editor_open: bool,
    compiled: Vec<CompiledFilter>,
}

impl PaneFilters {
    pub fn refresh(&mut self, global: &[CompiledFilter]) {
        self.compiled.clear();
        if let Some(names) = &self.selected {
            self.compiled.extend(
                global
                    .iter()
                    .filter(|rule| names.contains(&rule.name))
                    .cloned(),
            );
        }
    }

    pub fn records<'a>(&'a self, global: &'a [CompiledFilter]) -> &'a [CompiledFilter] {
        if self.selected.is_none() {
            global
        } else {
            &self.compiled
        }
    }

    pub fn show(
        &mut self,
        ctx: &Context,
        channel: &ChannelId,
        pane: Option<u64>,
        records: &[FilterRecord],
        global: &[CompiledFilter],
    ) -> bool {
        if !self.editor_open {
            return false;
        }
        let mut open = self.editor_open;
        let mut changed = false;
        let mut edit_rules = false;
        let id = Id::new("pane_filter_editor")
            .with(channel.as_str())
            .with(pane);
        let bounds = ctx.screen_rect().shrink(8.0);
        let style = ctx.style();
        let frame = egui::Frame::window(&style)
            .fill(t::bg_dialog())
            .stroke(t::stroke_subtle());
        let title_height = ctx
            .fonts(|fonts| fonts.row_height(&egui::TextStyle::Heading.resolve(&style)))
            .max(style.spacing.interact_size.y)
            + frame.inner_margin.sum().y
            + frame.stroke.width;
        let max_size = (bounds.size() - frame.total_margin().sum() - egui::vec2(0.0, title_height))
            .max(egui::Vec2::splat(1.0));
        egui::Window::new(format!("Filters for {}", channel.display_name()))
            .id(id)
            .open(&mut open)
            .default_size(egui::vec2(360.0, 360.0).min(max_size))
            .min_size(egui::vec2(140.0, 120.0).min(max_size))
            .max_size(max_size)
            .resizable(true)
            .collapsible(false)
            .constrain_to(bounds)
            .frame(frame)
            .show(ctx, |ui| {
                let mut use_all = self.selected.is_none();
                if ui
                    .checkbox(&mut use_all, "Use all enabled filters")
                    .changed()
                {
                    self.selected = if use_all {
                        None
                    } else {
                        Some(global.iter().map(|rule| rule.name.clone()).collect())
                    };
                    changed = true;
                }
                ui.label(
                    RichText::new("This selection applies to this chat view.")
                        .font(t::small())
                        .color(t::text_secondary()),
                );
                ui.separator();
                let mut names: Vec<_> = records.iter().map(|rule| rule.name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                let footer_height =
                    ui.spacing().interact_size.y + ui.spacing().item_spacing.y * 3.0;
                egui::ScrollArea::vertical()
                    .id_salt(id.with("rules"))
                    .max_height((ui.available_height() - footer_height).clamp(0.0, 280.0))
                    .min_scrolled_height(0.0)
                    .show(ui, |ui| {
                        if names.is_empty() {
                            ui.label("No saved filters.");
                        }
                        for name in &names {
                            let enabled = records
                                .iter()
                                .any(|rule| rule.name == *name && rule.enabled);
                            let mut selected = self
                                .selected
                                .as_ref()
                                .is_none_or(|selected| selected.iter().any(|value| value == name));
                            let label = if enabled {
                                (*name).to_owned()
                            } else {
                                format!("{name} (disabled)")
                            };
                            if ui
                                .add_enabled(!use_all, egui::Checkbox::new(&mut selected, label))
                                .changed()
                            {
                                let selection = self.selected.as_mut().unwrap();
                                if selected {
                                    selection.push((*name).to_owned());
                                } else {
                                    selection.retain(|value| value != name);
                                }
                                selection.sort_unstable();
                                selection.dedup();
                                changed = true;
                            }
                        }
                        if let Some(selected) = self.selected.as_mut() {
                            let unavailable: Vec<_> = selected
                                .iter()
                                .filter(|name| !names.contains(&name.as_str()))
                                .cloned()
                                .collect();
                            for name in unavailable {
                                let mut checked = true;
                                if ui
                                    .checkbox(&mut checked, format!("{name} (not found)"))
                                    .changed()
                                {
                                    selected.retain(|value| value != &name);
                                    changed = true;
                                }
                            }
                        }
                    });
                ui.separator();
                if ui.button("Edit saved filters").clicked() {
                    edit_rules = true;
                }
            });
        self.editor_open = open && !edit_rules;
        if changed {
            self.refresh(global);
        }
        edit_rules
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_core::model::{compile_filters, FilterScope};

    #[test]
    fn pane_selection_changes_only_its_own_compiled_rules_and_tracks_rule_edits() {
        let mut rules = vec![
            FilterRecord::new("Spam", "spam", FilterScope::Global),
            FilterRecord::new("Links", "https://", FilterScope::Global),
        ];
        let all = compile_filters(&rules);
        let mut selected = PaneFilters {
            selected: Some(vec!["Links".into()]),
            ..Default::default()
        };
        selected.refresh(&all);
        assert_eq!(selected.records(&all).len(), 1);
        assert_eq!(selected.records(&all)[0].name, "Links");
        assert_eq!(PaneFilters::default().records(&all).len(), 2);
        rules[1].enabled = false;
        let updated = compile_filters(&rules);
        selected.refresh(&updated);
        assert!(selected.records(&updated).is_empty());
        assert_eq!(selected.selected.as_ref().unwrap(), &["Links"]);
        rules[1].enabled = true;
        selected.refresh(&compile_filters(&rules));
        assert_eq!(selected.records(&all).len(), 1);
    }

    #[test]
    fn filter_picker_checkboxes_change_only_the_open_pane() {
        let ctx = Context::default();
        let channel = ChannelId::new("room");
        let rules = vec![
            FilterRecord::new("Spam", "spam", FilterScope::Global),
            FilterRecord::new("Links", "https://", FilterScope::Global),
        ];
        let compiled = compile_filters(&rules);
        let mut first = PaneFilters {
            editor_open: true,
            ..Default::default()
        };
        let second = PaneFilters::default();
        let frame = |state: &mut PaneFilters, events: Vec<egui::Event>| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(650.0, 500.0),
                    )),
                    time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    state.show(ctx, &channel, Some(10), &rules, &compiled);
                },
            )
        };
        for label in ["Use all enabled filters", "Spam"] {
            let mut output = egui::FullOutput::default();
            for _ in 0..3 {
                output = frame(&mut first, Vec::new());
            }
            let position = output
                .shapes
                .iter()
                .find_map(|shape| {
                    let egui::Shape::Text(text) = &shape.shape else {
                        return None;
                    };
                    (text.galley.text() == label)
                        .then(|| text.galley.rect.translate(text.pos.to_vec2()).center())
                })
                .expect("the filter checkbox is visible");
            for pressed in [true, false] {
                frame(
                    &mut first,
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
        assert_eq!(first.selected, Some(vec!["Links".into()]));
        assert_eq!(first.records(&compiled).len(), 1);
        assert_eq!(first.records(&compiled)[0].name, "Links");
        assert!(second.selected.is_none());
        assert_eq!(second.records(&compiled).len(), 2);
    }

    #[test]
    fn filter_footer_stays_visible_and_clickable_in_a_short_window() {
        let ctx = Context::default();
        let channel = ChannelId::new("room");
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(220.0, 200.0));
        let rules: Vec<_> = (0..40)
            .map(|index| FilterRecord::new(format!("Rule {index:02}"), "spam", FilterScope::Global))
            .collect();
        let compiled = compile_filters(&rules);
        let mut picker = PaneFilters {
            editor_open: true,
            ..Default::default()
        };
        let frame = |state: &mut PaneFilters, events: Vec<egui::Event>| {
            let mut edit_rules = false;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(viewport),
                    time: Some(ctx.cumulative_pass_nr() as f64 * 0.1),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    edit_rules |= state.show(ctx, &channel, Some(10), &rules, &compiled);
                },
            );
            (output, edit_rules)
        };
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = frame(&mut picker, Vec::new()).0;
        }
        let (label_rect, clip_rect) = output
            .shapes
            .iter()
            .find_map(|shape| {
                let egui::Shape::Text(text) = &shape.shape else {
                    return None;
                };
                (text.galley.text() == "Edit saved filters").then(|| {
                    (
                        text.galley.rect.translate(text.pos.to_vec2()),
                        shape.clip_rect,
                    )
                })
            })
            .expect("the footer is drawn even with a long filter list");
        assert!(
            viewport.contains_rect(label_rect),
            "footer outside viewport: {label_rect:?}"
        );
        assert!(
            clip_rect.contains_rect(label_rect),
            "footer is clipped: {label_rect:?}"
        );
        let position = label_rect.center();
        let mut opened_rules = false;
        for pressed in [true, false] {
            opened_rules |= frame(
                &mut picker,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .1;
        }
        assert!(opened_rules);
        assert!(!picker.editor_open);
    }
}
