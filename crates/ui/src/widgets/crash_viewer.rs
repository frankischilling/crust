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

use tracing::warn;

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
    /// Optional cleanup callback that runs before the viewer calls
    /// `std::process::exit(...)` from the "Restart" button. The app
    /// crate uses this to defuse the active session sentinel so the
    /// relaunched instance doesn't treat this run as abnormal.
    pre_exit_hook: Option<Box<dyn Fn() + Send + Sync + 'static>>,
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

    /// Install a cleanup closure invoked before the "Restart Crust"
    /// button exits the current process. The app crate uses this to
    /// remove the active session sentinel so the relaunched instance
    /// doesn't report this run as an abnormal shutdown.
    pub fn set_pre_exit_hook<F>(&mut self, f: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.pre_exit_hook = Some(Box::new(f));
    }

    /// Render the dialog.  Silently no-ops when closed or empty.
    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open || self.reports.is_empty() {
            return;
        }

        let mut should_close = false;

        egui::Window::new("Recovered crash reports")
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .default_height(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Crust detected crash reports from a previous session.",
                    )
                    .strong(),
                );
                ui.label(
                    egui::RichText::new(
                        "Review the details below, then dismiss to delete the report or \
                         keep the file for later triage.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    if ui
                        .button("Open crash folder")
                        .on_hover_text("Reveal the crashes directory in your file manager.")
                        .clicked()
                    {
                        // Walk back to the "crashes" folder regardless of whether the
                        // first path is a report (directly in it) or a session sentinel
                        // (in the sessions/ subdir).
                        if let Some(first) = self.reports.first() {
                            let target = first
                                .path
                                .parent()
                                .and_then(|p| {
                                    if p.file_name()
                                        .and_then(|s| s.to_str())
                                        == Some("sessions")
                                    {
                                        p.parent()
                                    } else {
                                        Some(p)
                                    }
                                })
                                .map(std::path::Path::to_path_buf);
                            if let Some(dir) = target {
                                open_in_file_manager(&dir);
                            }
                        }
                    }

                    if ui
                        .button("Restart Crust")
                        .on_hover_text("Close this window and relaunch the app.")
                        .clicked()
                    {
                        if let Some(hook) = self.pre_exit_hook.as_ref() {
                            hook();
                        }
                        restart_and_exit();
                    }

                    if ui
                        .button("Dismiss all")
                        .on_hover_text("Delete every recovered crash report on disk.")
                        .clicked()
                    {
                        let mut removed = 0_usize;
                        for r in &self.reports {
                            if std::fs::remove_file(&r.path).is_ok() {
                                removed += 1;
                            }
                        }
                        self.reports.clear();
                        self.selected = None;
                        self.status = Some(format!("{removed} report(s) deleted."));
                        should_close = true;
                    }

                    if ui.button("Close").clicked() {
                        should_close = true;
                    }
                });

                ui.add_space(6.0);
                ui.separator();

                // List of reports ---------------------------------------
                egui::ScrollArea::vertical()
                    .id_salt("crash_report_list")
                    .auto_shrink([false; 2])
                    .max_height(160.0)
                    .show(ui, |ui| {
                        let mut remove: Option<usize> = None;
                        let mut open_idx: Option<usize> = None;
                        for (i, r) in self.reports.iter().enumerate() {
                            let selected = self.selected == Some(i);
                            let abnormal = r.is_abnormal_shutdown();
                            let (summary_color, kind_label) = if abnormal {
                                (t::yellow(), "ABNORMAL EXIT")
                            } else {
                                (t::red(), "PANIC")
                            };
                            egui::Frame::group(ui.style())
                                .fill(if selected {
                                    t::accent_dim()
                                } else {
                                    t::bg_dialog()
                                })
                                .stroke(egui::Stroke::new(
                                    if selected { 1.0 } else { 0.5 },
                                    if selected {
                                        t::accent()
                                    } else {
                                        t::border_subtle()
                                    },
                                ))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new(kind_label)
                                                        .small()
                                                        .strong()
                                                        .color(summary_color),
                                                );
                                                ui.label(
                                                    egui::RichText::new(&r.timestamp)
                                                        .monospace()
                                                        .color(t::text_primary()),
                                                );
                                            });
                                            ui.label(
                                                egui::RichText::new(&r.summary)
                                                    .small()
                                                    .color(summary_color),
                                            );
                                            ui.label(
                                                egui::RichText::new(&r.file_name)
                                                    .small()
                                                    .weak(),
                                            );
                                        });

                                        ui.with_layout(
                                            egui::Layout::right_to_left(
                                                egui::Align::Center,
                                            ),
                                            |ui| {
                                                if ui.button("Delete").clicked() {
                                                    remove = Some(i);
                                                }
                                                let label = if selected {
                                                    "Hide"
                                                } else {
                                                    "View"
                                                };
                                                if ui.button(label).clicked() {
                                                    open_idx = Some(i);
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(4.0);
                        }
                        if let Some(i) = open_idx {
                            self.selected =
                                if self.selected == Some(i) { None } else { Some(i) };
                            self.status = None;
                        }
                        if let Some(i) = remove {
                            if let Some(r) = self.reports.get(i) {
                                match std::fs::remove_file(&r.path) {
                                    Ok(()) => {
                                        self.status =
                                            Some(format!("Deleted {}.", r.file_name));
                                    }
                                    Err(e) => {
                                        self.status = Some(format!(
                                            "Failed to delete {}: {e}",
                                            r.file_name
                                        ));
                                    }
                                }
                            }
                            self.reports.remove(i);
                            self.selected = match self.selected {
                                Some(sel) if sel == i => None,
                                Some(sel) if sel > i => Some(sel - 1),
                                other => other,
                            };
                            if self.reports.is_empty() {
                                should_close = true;
                            }
                        }
                    });

                // Detail pane -----------------------------------------
                if let Some(idx) = self.selected {
                    if let Some(r) = self.reports.get(idx) {
                        ui.add_space(6.0);
                        ui.separator();
                        ui.label(
                            egui::RichText::new("Report contents")
                                .strong()
                                .color(t::text_primary()),
                        );
                        ui.add_space(2.0);

                        let mut body = r.preview.clone();
                        egui::ScrollArea::vertical()
                            .id_salt("crash_report_detail")
                            .auto_shrink([false; 2])
                            .max_height(220.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut body)
                                        .font(egui::TextStyle::Monospace)
                                        .code_editor()
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(12)
                                        .interactive(false),
                                );
                            });

                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button("Copy to clipboard")
                                .on_hover_text(
                                    "Copy the full report text so it can be shared \
                                     with maintainers.",
                                )
                                .clicked()
                            {
                                ui.ctx().copy_text(r.preview.clone());
                                self.status = Some("Report copied to clipboard.".into());
                            }
                            if ui.button("Show file on disk").clicked() {
                                open_in_file_manager(&r.path);
                            }
                        });
                    }
                }

                if let Some(msg) = &self.status {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(msg)
                            .small()
                            .color(t::text_primary()),
                    );
                }
            });

        if should_close {
            self.open = false;
            self.selected = None;
        }
    }
}

/// Best-effort relaunch: spawn a fresh instance of the current
/// executable and exit this one.  On any failure we just log via
/// `tracing` and leave the app running so the user can retry manually.
fn restart_and_exit() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!("crash viewer restart: cannot resolve current_exe: {e}");
            return;
        }
    };
    // Spawn detached so killing this process doesn't take the child
    // with it.  On Unix that's the default once the child has been
    // reparented to init; on Windows `Command::spawn` already creates
    // an independent process group.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(_child) => {
            // Exit with a zero status so the clean-shutdown path has
            // a chance to defuse the current session sentinel.  The
            // new instance will see no orphaned sentinels from this
            // run.
            std::process::exit(0);
        }
        Err(e) => {
            warn!("crash viewer restart: failed to spawn {:?}: {e}", exe);
        }
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
