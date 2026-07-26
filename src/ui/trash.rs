//! Trash view: history of automatic media disposals (`disposal::dispose_media`
//! call sites — post-join cleanup, gap-splice cleanup, VOD replace, superseded
//! old heads), grouped by channel like the Streams grid. A Trash-method
//! ("soft-deleted") row can be restored or permanently deleted; Recycle Bin /
//! permanent-delete rows are history only — Windows owns that recovery path.

use super::*;

use crate::disposal::DisposalRecordState;
use crate::store::DisposalRecordDisplay;

/// `(record id, outcome)` for a finished Restore/Permanently-delete action —
/// see the `trash_action_done` field doc for why this needs to cross threads.
pub(super) type TrashActionOutcome = (i64, Result<(), String>);

impl StreamArchiverApp {
    pub(super) fn ensure_trash_loaded(&mut self) {
        if self.trash_loaded {
            return;
        }
        self.reload_trash();
    }

    pub(super) fn reload_trash(&mut self) {
        self.trash_records = self.core.store.list_disposal_records().unwrap_or_default();
        self.trash_loaded = true;
    }

    /// Drain completed Restore/Permanently-delete outcomes posted by
    /// `core.rt.spawn`'d tasks (see `spawn_trash_restore`/`spawn_trash_permadelete`)
    /// and apply them: clear the row's pending flag, surface any error, and
    /// reload the list so states/paths reflect what actually happened.
    fn drain_trash_action_results(&mut self) {
        let drained: Vec<TrashActionOutcome> =
            std::mem::take(&mut *self.trash_action_done.lock().unwrap());
        if drained.is_empty() {
            return;
        }
        for (id, result) in drained {
            self.trash_action_pending.remove(&id);
            if let Err(e) = result {
                self.trash_action_error = Some(e);
            }
        }
        self.reload_trash();
    }

    fn spawn_trash_restore(&mut self, ctx: &egui::Context, id: i64) {
        self.trash_action_pending.insert(id);
        let store = self.core.store.clone();
        let done = self.trash_action_done.clone();
        let ctx = ctx.clone();
        self.core.rt.spawn(async move {
            let result = crate::disposal::restore_disposal_record(&store, id)
                .await
                .map_err(|e| e.to_string());
            done.lock().unwrap().push((id, result));
            ctx.request_repaint();
        });
    }

    fn spawn_trash_permadelete(&mut self, ctx: &egui::Context, id: i64) {
        self.trash_action_pending.insert(id);
        let store = self.core.store.clone();
        let done = self.trash_action_done.clone();
        let ctx = ctx.clone();
        self.core.rt.spawn(async move {
            let result = crate::disposal::permanently_delete_disposal_record(&store, id)
                .await
                .map_err(|e| e.to_string());
            done.lock().unwrap().push((id, result));
            ctx.request_repaint();
        });
    }

    pub(super) fn trash_view(&mut self, ui: &mut egui::Ui) {
        self.ensure_trash_loaded();
        self.drain_trash_action_results();

        if let Some(err) = self.trash_action_error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::from_rgb(210, 90, 90), format!("⚠ {err}"));
                if ui.small_button("✕").clicked() {
                    self.trash_action_error = None;
                }
            });
            ui.add_space(4.0);
        }

        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(
                egui::TextEdit::singleline(&mut self.trash_filter)
                    .hint_text("channel name")
                    .desired_width(200.0),
            );
            if ui
                .button("⟳ Refresh")
                .on_hover_text("Reload the disposal history.")
                .clicked()
            {
                self.reload_trash();
            }
        });
        ui.add_space(6.0);

        if self.trash_records.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(
                    "Nothing disposed yet — this fills in as automatic cleanup (post-join, \
                     gap splice, VOD replace, superseded heads) removes a file.",
                );
            });
            return;
        }

        // Snapshot so per-row rendering (which needs `&mut self` for the
        // fs-probe cache and action dispatch) doesn't fight a live borrow of
        // `self.trash_records` — same shape as Streams' own frame-invariant
        // cache rebuild.
        let records = self.trash_records.clone();
        let mut by_channel: std::collections::BTreeMap<i64, Vec<usize>> = Default::default();
        for (i, r) in records.iter().enumerate() {
            by_channel.entry(r.channel_id.unwrap_or(-1)).or_default().push(i);
        }
        let mut groups: Vec<(i64, String, Vec<usize>)> = by_channel
            .into_iter()
            .map(|(cid, idxs)| {
                let name = records[idxs[0]].channel_name.clone();
                (cid, name, idxs)
            })
            .collect();
        // Alphabetical by channel name; the "unknown channel" bucket (a
        // recording row that's since vanished from the DB) always sorts last.
        groups.sort_by(|a, b| match (a.0 == -1, b.0 == -1) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
        });

        let filter = self.trash_filter.trim().to_lowercase();
        let ctx = ui.ctx().clone();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (cid, name, idxs) in &groups {
                let display_name = if *cid == -1 {
                    "(unknown channel)".to_string()
                } else if name.is_empty() {
                    format!("Channel #{cid}")
                } else {
                    name.clone()
                };
                if !filter.is_empty() && !display_name.to_lowercase().contains(&filter) {
                    continue;
                }
                egui::CollapsingHeader::new(format!("{display_name}  ({})", idxs.len()))
                    .id_salt(("trash_chan", *cid))
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::Grid::new(("trash_grid", *cid))
                            .num_columns(6)
                            .striped(true)
                            .spacing([10.0, 4.0])
                            .show(ui, |ui| {
                                ui.strong("When");
                                ui.strong("Reason");
                                ui.strong("Method");
                                ui.strong("State");
                                ui.strong("Path");
                                ui.strong("Actions");
                                ui.end_row();
                                for &i in idxs {
                                    self.trash_row(ui, &ctx, &records[i]);
                                    ui.end_row();
                                }
                            });
                    });
                ui.add_space(4.0);
            }
        });
    }

    fn trash_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rec: &DisposalRecordDisplay) {
        let row = &rec.row;
        ts_label(ui, row.disposed_at);
        let reason_resp = ui.label(&row.reason);
        if let Some(started) = rec.take_started_at {
            reason_resp.on_hover_ui(|ui| {
                ui.label("Take started:");
                ts_label(ui, started);
            });
        }
        ui.label(row.method.label());
        ui.label(row.state.label());

        let current_path: &str = match row.state {
            DisposalRecordState::SoftDeleted => &row.trash_path,
            DisposalRecordState::Restored => &row.original_path,
            DisposalRecordState::Permanent => {
                if !row.trash_path.is_empty() { &row.trash_path } else { &row.original_path }
            }
        };
        ui.add(egui::Label::new(egui::RichText::new(current_path).small().monospace()).truncate())
            .on_hover_text(if current_path.is_empty() { "(no path)" } else { current_path });

        ui.horizontal(|ui| {
            let pending = self.trash_action_pending.contains(&row.id);
            if row.state == DisposalRecordState::SoftDeleted {
                if ui
                    .add_enabled(!pending, egui::Button::new("↩"))
                    .on_hover_text("Restore to its original location")
                    .clicked()
                {
                    self.spawn_trash_restore(ctx, row.id);
                }
                if ui
                    .add_enabled(!pending, egui::Button::new("🗑"))
                    .on_hover_text("Permanently delete")
                    .clicked()
                {
                    self.confirm_permadelete_trash = Some((row.id, current_path.to_string()));
                }
            }
            let path_buf = (!current_path.is_empty()).then(|| std::path::PathBuf::from(current_path));
            let file_ok = path_buf.as_ref().is_some_and(|p| self.fs_probes.is_file(p));
            if ui.add_enabled(file_ok, egui::Button::new("▶")).on_hover_text("Open file").clicked()
                && let Some(p) = &path_buf
            {
                crate::platform::open_path(p);
            }
            let dir_ok = path_buf
                .as_ref()
                .and_then(|p| p.parent())
                .is_some_and(|d| self.fs_probes.is_dir(d));
            if ui
                .add_enabled(dir_ok, egui::Button::new("📂"))
                .on_hover_text("Open containing folder")
                .clicked()
                && let Some(dir) = path_buf.as_ref().and_then(|p| p.parent())
            {
                crate::platform::open_path(dir);
            }
            if pending {
                ui.spinner();
            }
        });
    }

    /// Confirmation dialog for the Trash view's "Permanently delete" action —
    /// the one irreversible step in this view (Restore just moves the file
    /// back; this removes it for good).
    #[allow(deprecated)] // CentralPanel::show inside show_viewport_immediate — same as the other confirm_* windows
    pub(super) fn confirm_permadelete_trash_window(&mut self, ctx: &egui::Context) {
        let Some((id, path)) = self.confirm_permadelete_trash.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("confirm_permadelete_trash_vp"),
            egui::ViewportBuilder::default()
                .with_title("Permanently delete")
                .with_inner_size([480.0, 150.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Permanently delete this file? This cannot be undone.");
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(egui::RichText::new(&path).small().monospace()).truncate());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            do_delete = true;
                        }
                        if ui.button("Cancel").clicked() {
                            do_cancel = true;
                        }
                    });
                });
            },
        );

        if do_delete {
            self.spawn_trash_permadelete(ctx, id);
            self.confirm_permadelete_trash = None;
        } else if do_cancel || !open {
            self.confirm_permadelete_trash = None;
        }
    }
}
