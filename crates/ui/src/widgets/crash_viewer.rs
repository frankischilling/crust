//! Recovered crash report viewer dialog.
//!
//! On every launch the app scans its crash-report directory for leftover
//! reports written by the previous session's panic hook.  When one or
//! more are found, [`CrashViewer::set_pending_reports`] is populated so a
//! simple modal viewer can surface them to the user: read the details,
//! open the containing folder, dismiss individual reports, or wipe them
//! all.  The actual panic-capture lives in the app crate
//! (`crates/app/src/crash.rs`); this widget is purely the presentation
//! layer and depends on nothing but egui + the small [`CrashReportMeta`]
//! value type defined below.

use std::path::PathBuf;

use crate::theme as t;

/// Metadata for a single recovered crash report.
///
/// Produced by the app-side panic handler when it scans the crash
/// directory at startup.  The UI only needs these fields to render the
/// viewer, so the producing code is free to drop redundant/internal
/// fields (backtrace lines, raw file bytes, etc.) onto disk without
/// bloating the widget's struct layout.
#[derive(Debug, Clone)]
pub struct CrashReportMeta {
    /// Absolute path to the on-disk report file (used for delete + open
    /// containing folder).
    pub path: PathBuf,
    /// Just the file name for display (`crash-20260422-231105.txt`).
    pub file_name: String,
    /// Best-effort ISO-like timestamp parsed out of the report header
    /// (falls back to the filename timestamp if the header is missing).
    pub timestamp: String,
    /// First line of the panic message.
    pub summary: String,
    /// Full text contents of the report, shown in the detail pane when
    /// the user expands one.
    pub preview: String,
}

impl CrashReportMeta {
    /// `true` if this entry is synthesized from a leftover session
    /// sentinel (e.g. SIGKILL, power loss, native-code crash) rather
    /// than a Rust panic report.  Used to render the row in amber
    /// instead of red and to omit a "Copy backtrace" affordance.
    fn is_abnormal_shutdown(&self) -> bool {
        self.preview.starts_with("== Abnormal shutdown")
    }
}

/// Retained state for the recovered-crash-report modal.
///
/// Owned by [`crate::CrustApp`] and rendered once per frame via
/// [`CrashViewer::show`]. The dialog auto-opens when non-empty reports
/// are installed via [`CrashViewer::set_pending_reports`] so the user
/// sees it immediately on the next launch after a crash.
#[derive(Default)]
pub struct CrashViewer {
    /// Whether the dialog window is currently visible.
    pub open: bool,
    /// Reports waiting for the user to review.  Newest first.
    reports: Vec<CrashReportMeta>,
    /// Currently expanded report (index into `reports`); `None` shows
    /// the list-only view.
    selected: Option<usize>,
    /// One-shot status line displayed under the list (e.g. "Report
    /// deleted.").  Cleared on the next interaction.
    status: Option<String>,
    /// Requests a restart after the app has completed its normal shutdown.
    restart_handler: Option<Box<dyn Fn() + Send + Sync + 'static>>,
}

impl CrashViewer {
    /// Install a batch of recovered crash reports. Opens the dialog
    /// automatically if at least one report is present; clears the
    /// dialog otherwise.
    pub fn set_pending_reports(&mut self, reports: Vec<CrashReportMeta>) {
        self.reports = reports;
        self.selected = None;
        self.status = None;
        self.open = !self.reports.is_empty();
    }

    /// Returns `true` while at least one recovered report is still
    /// displayed.  Useful for callers that want to know whether the
    /// next frame should keep the viewer visible.
    #[allow(dead_code)]
    pub fn has_reports(&self) -> bool {
        !self.reports.is_empty()
    }

    /// Register the application's restart request handler.
    pub fn set_restart_handler<F>(&mut self, f: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.restart_handler = Some(Box::new(f));
    }

    /// Render recovered reports without covering the window's dismissal controls.
    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open || self.reports.is_empty() {
            return;
        }

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
        let mut open = self.open;
        let mut should_close = false;
        let mut dismiss_all = false;
        let mut restart = false;
        let mut open_folder = false;
        let mut remove = None;
        let mut selected = None;

        egui::Window::new("Crash reports")
            .id(egui::Id::new("crash_report_viewer"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .frame(frame)
            .constrain_to(bounds)
            .min_size(egui::vec2(160.0, 100.0).min(max_size))
            .max_size(max_size)
            .default_size(egui::vec2(560.0, 420.0).min(max_size))
            .default_pos(bounds.center() - egui::vec2(560.0, 420.0).min(max_size) * 0.5)
            .show(ctx, |ui| {
                ui.set_min_height(ui.available_height().min(max_size.y));
                egui::TopBottomPanel::bottom("crash_report_actions")
                    .frame(egui::Frame::NONE)
                    .min_height(0.0)
                    .show_inside(ui, |ui| {
                        if let Some(status) = &self.status {
                            ui.add(egui::Label::new(
                                egui::RichText::new(status).small().color(t::text_primary()),
                            ).truncate()).on_hover_text(status);
                        }
                        ui.separator();
                        ui.horizontal_wrapped(|ui| {
                            should_close = ui.button("Close").clicked();
                            ui.menu_button("More", |ui| {
                                if ui.button("Open crash folder").clicked() {
                                    open_folder = true;
                                    ui.close_menu();
                                }
                                if ui.add_enabled(
                                    self.restart_handler.is_some(),
                                    egui::Button::new("Restart Crust"),
                                ).on_hover_text("Save the current layout and chat history, then restart.")
                                    .clicked()
                                {
                                    restart = true;
                                    ui.close_menu();
                                }
                                if ui.button("Dismiss all")
                                    .on_hover_text("Delete recovered report files. Files that cannot be deleted stay in this list.")
                                    .clicked()
                                {
                                    dismiss_all = true;
                                    ui.close_menu();
                                }
                            }).response.on_hover_text("Report actions");
                        });
                    });
                egui::ScrollArea::vertical()
                    .id_salt("crash_report_body")
                    .max_height(ui.available_height())
                    .min_scrolled_height(0.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new("Crust found reports from a previous session.").strong(),
                        ).wrap());
                        ui.add(egui::Label::new(
                            egui::RichText::new("View a report to copy its contents. Delete removes its file; Close keeps it for later.")
                                .small().weak(),
                        ).wrap());
                        ui.add_space(6.0);

                        for (index, report) in self.reports.iter().enumerate() {
                            let is_selected = self.selected == Some(index);
                            let (color, kind) = if report.is_abnormal_shutdown() {
                                (t::yellow(), "Abnormal exit")
                            } else {
                                (t::red(), "Panic")
                            };
                            ui.push_id(&report.path, |ui| {
                                egui::Frame::group(ui.style())
                                    .fill(if is_selected { t::accent_dim() } else { t::bg_dialog() })
                                    .stroke(egui::Stroke::new(1.0_f32, if is_selected {
                                        t::accent()
                                    } else {
                                        t::border_subtle()
                                    }))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(egui::RichText::new(kind).small().strong().color(color));
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(&report.timestamp).monospace(),
                                            ).wrap());
                                        });
                                        ui.add(egui::Label::new(
                                            egui::RichText::new(&report.summary).small().color(color),
                                        ).wrap());
                                        ui.add(egui::Label::new(
                                            egui::RichText::new(&report.file_name).small().weak(),
                                        ).truncate()).on_hover_text(&report.file_name);
                                        ui.horizontal_wrapped(|ui| {
                                            if ui.button(if is_selected { "Hide" } else { "View" }).clicked() {
                                                selected = Some(index);
                                            }
                                            if ui.button("Delete").clicked() {
                                                remove = Some(index);
                                            }
                                        });
                                    });
                            });
                            ui.add_space(4.0);
                        }

                        if let Some(report) = self.selected.and_then(|index| self.reports.get(index)) {
                            ui.separator();
                            ui.label(egui::RichText::new("Report contents").strong());
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Copy to clipboard").clicked() {
                                    ctx.copy_text(report.preview.clone());
                                    self.status = Some("Report copied to clipboard.".into());
                                }
                                if ui.button("Show file on disk").clicked() {
                                    open_in_file_manager(&report.path);
                                }
                            });
                            // The immutable buffer avoids cloning a large report every frame.
                            let mut body = report.preview.as_str();
                            egui::ScrollArea::both()
                                .id_salt(("crash_report_detail", &report.path))
                                .auto_shrink([false, false])
                                .min_scrolled_height(0.0)
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    ui.add(egui::TextEdit::multiline(&mut body)
                                        .font(egui::TextStyle::Monospace)
                                        .code_editor()
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(12)
                                        .interactive(false));
                                });
                        }
                    });
            });

        if let Some(index) = selected {
            self.selected = if self.selected == Some(index) {
                None
            } else {
                Some(index)
            };
            self.status = None;
        }
        if let Some(index) = remove {
            self.delete_report(index);
        }
        if dismiss_all {
            self.dismiss_all();
        }
        if open_folder {
            if let Some(report) = self.reports.first() {
                if let Some(parent) = report.path.parent() {
                    let folder =
                        if parent.file_name().and_then(|name| name.to_str()) == Some("sessions") {
                            parent.parent().unwrap_or(parent)
                        } else {
                            parent
                        };
                    open_in_file_manager(folder);
                }
            }
        }
        if restart {
            self.request_restart(ctx);
        }
        if should_close || !open {
            self.open = false;
            self.selected = None;
        }
    }

    fn request_restart(&self, ctx: &egui::Context) {
        if let Some(handler) = &self.restart_handler {
            handler();
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
        }
    }

    fn delete_report(&mut self, index: usize) {
        let Some(report) = self.reports.get(index) else {
            return;
        };
        match remove_report_file(&report.path) {
            Ok(()) => {
                self.status = Some(format!("Removed {}.", report.file_name));
                self.reports.remove(index);
                self.selected = match self.selected {
                    Some(selected) if selected == index => None,
                    Some(selected) if selected > index => Some(selected - 1),
                    other => other,
                };
                self.open = !self.reports.is_empty();
            }
            Err(error) => {
                self.status = Some(format!("Could not delete {}: {error}", report.file_name));
            }
        }
    }

    fn dismiss_all(&mut self) {
        let selected = self
            .selected
            .and_then(|index| self.reports.get(index))
            .map(|report| report.path.clone());
        let mut removed = 0;
        let mut first_error = None;
        self.reports
            .retain(|report| match remove_report_file(&report.path) {
                Ok(()) => {
                    removed += 1;
                    false
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| format!("{}: {error}", report.file_name));
                    true
                }
            });
        self.selected =
            selected.and_then(|path| self.reports.iter().position(|report| report.path == path));
        self.open = !self.reports.is_empty();
        self.status = Some(match first_error {
            Some(error) => format!(
                "Removed {removed} reports. {} could not be deleted. {error}",
                self.reports.len()
            ),
            None => format!("Removed {removed} reports."),
        });
    }
}

fn remove_report_file(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Open the given path in the OS file manager.  Best-effort: any
/// failure is silently ignored because the crash viewer shouldn't
/// destabilise the process when, for example, `xdg-open` isn't in
/// `PATH`.
fn open_in_file_manager(path: &std::path::Path) {
    // Ensure the target exists so the OS handler doesn't complain.
    // If it's a file, spawn a "reveal parent + select" command where
    // supported, otherwise just open the containing directory.
    #[cfg(target_os = "windows")]
    {
        if path.is_file() {
            let _ = std::process::Command::new("explorer.exe")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        } else {
            let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        if path.is_file() {
            let _ = std::process::Command::new("open")
                .args(["-R", &path.display().to_string()])
                .spawn();
        } else {
            let _ = std::process::Command::new("open").arg(path).spawn();
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "crust-crash-viewer-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn report(path: PathBuf) -> CrashReportMeta {
        CrashReportMeta {
            file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path,
            timestamp: "2026-11-15T12:00:00+00:00".into(),
            summary: "A recovered error with a long explanation and Unicode 🙂. ".repeat(12),
            preview: "Frame in the recovered backtrace with Unicode 🙂.\n".repeat(300),
        }
    }

    fn frame(
        ctx: &egui::Context,
        viewer: &mut CrashViewer,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| viewer.show(ctx),
        )
    }

    fn label_rect(output: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                let rect = text.galley.rect.translate(text.pos.to_vec2());
                shape
                    .clip_rect
                    .expand(0.5)
                    .contains_rect(rect)
                    .then_some(rect)
            }
            _ => None,
        })
    }

    #[test]
    fn failed_deletion_keeps_the_report_selected_and_available_for_retry() {
        let dir = TestDirectory::new();
        let blocked = dir.0.join("report.txt");
        std::fs::create_dir(&blocked).unwrap();
        let mut viewer = CrashViewer::default();
        viewer.set_pending_reports(vec![report(blocked.clone())]);
        viewer.selected = Some(0);
        viewer.delete_report(0);
        assert!(viewer.open);
        assert_eq!(viewer.reports.len(), 1);
        assert_eq!(viewer.selected, Some(0));
        assert!(viewer
            .status
            .as_deref()
            .unwrap()
            .starts_with("Could not delete"));
        std::fs::remove_dir(&blocked).unwrap();
        std::fs::write(&blocked, "recovered report").unwrap();
        viewer.delete_report(0);
        assert!(viewer.reports.is_empty());
        assert!(!viewer.open);
        assert!(!blocked.exists());
    }

    #[test]
    fn dismiss_all_retains_failed_files_and_their_selection() {
        let dir = TestDirectory::new();
        let good = dir.0.join("good.txt");
        let blocked = dir.0.join("blocked.txt");
        let missing = dir.0.join("already-removed.txt");
        std::fs::write(&good, "recovered report").unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let mut viewer = CrashViewer::default();
        viewer.set_pending_reports(vec![
            report(good.clone()),
            report(blocked.clone()),
            report(missing),
        ]);
        viewer.selected = Some(1);
        viewer.dismiss_all();
        assert!(!good.exists());
        assert!(viewer.open);
        assert_eq!(viewer.reports.len(), 1);
        assert_eq!(viewer.reports[0].path, blocked);
        assert_eq!(viewer.selected, Some(0));
        assert!(viewer
            .status
            .as_deref()
            .unwrap()
            .contains("1 could not be deleted"));
    }

    #[test]
    fn recovery_footer_stays_visible_and_close_works_in_small_windows() {
        for (size, font_size) in [
            (egui::vec2(220.0, 200.0), 14.0),
            (egui::vec2(320.0, 240.0), 24.0),
            (egui::vec2(640.0, 480.0), 14.0),
        ] {
            let ctx = egui::Context::default();
            let mut style = (*ctx.style()).clone();
            style.override_font_id = Some(egui::FontId::proportional(font_size));
            ctx.set_style(style);
            let mut viewer = CrashViewer::default();
            viewer.set_pending_reports(vec![report(PathBuf::from("recovered-long-report.txt"))]);
            viewer.selected = Some(0);
            let mut output = egui::FullOutput::default();
            for _ in 0..4 {
                output = frame(&ctx, &mut viewer, size, vec![]);
            }
            let bounds = ctx
                .memory(|memory| memory.area_rect(egui::Id::new("crash_report_viewer")))
                .unwrap();
            assert!(
                egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                    .expand(1.0)
                    .contains_rect(bounds),
                "recovery dialog {bounds:?} escaped {size:?}"
            );
            assert!(
                label_rect(&output, "More").is_some(),
                "report actions clipped at {size:?}"
            );
            let close = label_rect(&output, "Close")
                .expect("close button is visible")
                .center();
            for pressed in [true, false] {
                frame(
                    &ctx,
                    &mut viewer,
                    size,
                    vec![
                        egui::Event::PointerMoved(close),
                        egui::Event::PointerButton {
                            pos: close,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(!viewer.open);
            assert_eq!(
                viewer.reports.len(),
                1,
                "Close must preserve the recovered report"
            );
        }
    }

    #[test]
    fn restart_requests_normal_root_shutdown_without_exiting_in_the_widget() {
        let ctx = egui::Context::default();
        let requested = Arc::new(AtomicUsize::new(0));
        let signal = Arc::clone(&requested);
        let mut viewer = CrashViewer::default();
        viewer.set_restart_handler(move || {
            signal.fetch_add(1, Ordering::Relaxed);
        });
        let output = ctx.run(egui::RawInput::default(), |ctx| viewer.request_restart(ctx));
        assert_eq!(requested.load(Ordering::Relaxed), 1);
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
    }
}
