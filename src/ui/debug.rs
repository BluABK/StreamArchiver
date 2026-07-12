//! Debug view and the widget-inspector window.

use super::*;

impl StreamArchiverApp {
    /// Debug view (debug builds, or release builds launched with `--debug`).
    /// Shows a toast tester, build identity, data counts, filesystem paths,
    /// and the live process list.
    pub(super) fn debug_view(&mut self, ui: &mut egui::Ui) {
        if !debug_view_enabled() {
            ui.label(
                "Debug view is only available in debug builds, or when the app \
                 is launched with --debug.",
            );
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            // ── Toast tester ─────────────────────────────────────────────────
            ui.heading("Toast tester");
            ui.separator();

            let monitor_labels: Vec<String> = self
                .rows
                .iter()
                .map(|r| format!("{} [{}]", r.channel.name, r.monitor.platform().label()))
                .collect();

            ui.horizontal(|ui| {
                ui.label("Monitor:");
                let label = monitor_labels
                    .get(self.debug_monitor_idx)
                    .cloned()
                    .unwrap_or_else(|| "— none —".into());
                egui::ComboBox::from_id_salt("dbg_monitor_cb")
                    .selected_text(label)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for (i, name) in monitor_labels.iter().enumerate() {
                            ui.selectable_value(&mut self.debug_monitor_idx, i, name);
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Title:").on_hover_text("Simulated stream title");
                ui.add(egui::TextEdit::singleline(&mut self.debug_test_title).desired_width(280.0));
            });
            ui.horizontal(|ui| {
                ui.label("Game: ").on_hover_text("Simulated game / category");
                ui.add(egui::TextEdit::singleline(&mut self.debug_test_game).desired_width(280.0));
            });
            ui.add_space(4.0);
            if ui.button("Send test toast").clicked() {
                if let Some(row) = self.rows.get(self.debug_monitor_idx) {
                    let platform = row.monitor.platform();
                    crate::notifications::send_test_toast(
                        &row.channel.name,
                        &row.monitor.url,
                        platform,
                        &self.debug_test_title.clone(),
                        &self.debug_test_game.clone(),
                    );
                } else {
                    self.status = "No monitor selected for test toast.".into();
                }
            }

            ui.add_space(16.0);

            // ── Build identity ───────────────────────────────────────────────
            ui.heading("Build");
            ui.separator();
            egui::Grid::new("dbg_build_grid")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Build ID:");
                    ui.monospace(crate::version::build_id());
                    ui.end_row();
                    ui.label("Schema version:");
                    ui.monospace("25");
                    ui.end_row();
                    ui.label("Debug assertions:");
                    ui.monospace("on");
                    ui.end_row();
                });

            ui.add_space(16.0);

            // ── In-memory data counts ────────────────────────────────────────
            ui.heading("Data");
            ui.separator();
            let active_count = {
                let a = self.core.active.lock().unwrap_or_else(|e| e.into_inner());
                a.len()
            };
            let active_video_count = {
                let a = self.core.active_videos.lock().unwrap_or_else(|e| e.into_inner());
                a.len()
            };
            egui::Grid::new("dbg_data_grid")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Channel containers:");
                    ui.monospace(self.channels.len().to_string());
                    ui.end_row();
                    ui.label("Monitors:");
                    ui.monospace(self.rows.len().to_string());
                    ui.end_row();
                    ui.label("Video downloads:");
                    ui.monospace(self.videos.len().to_string());
                    ui.end_row();
                    ui.label("Active recordings:");
                    ui.monospace(active_count.to_string());
                    ui.end_row();
                    ui.label("Active video DLs:");
                    ui.monospace(active_video_count.to_string());
                    ui.end_row();
                    ui.label("Background tasks:");
                    ui.monospace(self.background_tasks.len().to_string());
                    ui.end_row();
                });

            ui.add_space(16.0);

            // ── Filesystem paths ─────────────────────────────────────────────
            ui.heading("Paths");
            ui.separator();
            let db_path = crate::app_paths::db_path();
            let asset_dir = crate::app_paths::asset_cache_dir();
            egui::Grid::new("dbg_paths_grid")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Database:");
                    ui.horizontal(|ui| {
                        ui.monospace(db_path.display().to_string());
                        if ui
                            .small_button("📂")
                            .on_hover_text("Open folder in explorer")
                            .clicked()
                        {
                            if let Some(parent) = db_path.parent() {
                                crate::platform::open_path(parent);
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Asset cache:");
                    ui.horizontal(|ui| {
                        ui.monospace(asset_dir.display().to_string());
                        if ui
                            .small_button("📂")
                            .on_hover_text("Open folder in explorer")
                            .clicked()
                        {
                            crate::platform::open_path(&asset_dir);
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(16.0);

            // ── Live process list ────────────────────────────────────────────
            ui.heading("Active processes");
            ui.separator();
            let procs = self.core.list_processes();
            if procs.is_empty() {
                ui.label("— none —");
            } else {
                egui::Grid::new("dbg_procs_grid")
                    .num_columns(5)
                    .spacing([12.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("PID");
                        ui.strong("Kind");
                        ui.strong("Name");
                        ui.strong("Tool");
                        ui.strong("Build");
                        ui.end_row();
                        for p in &procs {
                            ui.monospace(p.pid.to_string());
                            ui.label(format!("{:?}", p.kind));
                            ui.label(&p.name);
                            ui.label(&p.tool);
                            let build_label = if p.reattached {
                                format!("{} [re-attached]", p.spawn_build)
                            } else {
                                p.spawn_build.clone()
                            };
                            ui.label(build_label);
                            ui.end_row();
                        }
                    });
            }
        });
    }

    /// Task-manager-style dialog listing every spawned download tool process with
    /// The widget inspector (F12): lists widgets instrumented with
    /// [`crate::inspector::Inspectable::inspect`], with per-widget properties,
    /// source location, a highlight overlay, and tabs delegating to egui's
    /// built-in layout/memory/style debug UIs. Displays the previous frame's
    /// snapshot (drained by `end_frame` at the very end of `ui()`).
    #[allow(deprecated)]
    pub(super) fn inspector_window(&mut self, ctx: &egui::Context) {
        if !self.show_inspector {
            return;
        }
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("inspector_vp"),
            egui::ViewportBuilder::default()
                .with_title("🔍 Inspector")
                .with_inner_size([520.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    crate::inspector::ui_contents(ui, &mut self.inspector);
                });
            },
        );
        self.show_inspector = open;
    }
}
