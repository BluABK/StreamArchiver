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
        // Restoring or permanently deleting changes how many files are left in
        // the trash folders, so re-derive the 🗑 tab badge from the rows we
        // just loaded rather than letting it sit stale for up to a minute
        // waiting on its own throttle.
        self.trash_badge = self
            .trash_records
            .iter()
            .filter(|d| d.row.state == crate::disposal::DisposalRecordState::SoftDeleted)
            .count() as i64;
        // A reload means the world may have shifted under any checked rows
        // (restored, permanently deleted elsewhere, or the whole list simply
        // changed) — drop anything that isn't still a soft-deleted row so a
        // stale id can never sneak into a later "Delete selected" batch.
        self.trash_selected.retain(|id| {
            self.trash_records
                .iter()
                .any(|r| r.row.id == *id && r.row.state == DisposalRecordState::SoftDeleted)
        });
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

    /// Drain a finished "Import history" scan (see `spawn_trash_import`) and
    /// surface its summary in the status bar.
    fn drain_trash_import_result(&mut self) {
        let Some(report) = self.trash_import_done.lock().unwrap().take() else {
            return;
        };
        self.trash_import_running = false;
        self.status = report.summarize();
        if report.imported_total() > 0 {
            self.reload_trash();
        }
    }

    /// One-time scan reconstructing best-effort Trash entries for disposals
    /// that predate this feature (`disposal_backfill::run_historical_backfill`).
    /// Runs on a blocking thread — it does many small `stat` calls on top of
    /// the DB reads, same reasoning as any other bulk `Store` scan.
    fn spawn_trash_import(&mut self, ctx: &egui::Context) {
        self.trash_import_running = true;
        let store = self.core.store.clone();
        let done = self.trash_import_done.clone();
        let ctx = ctx.clone();
        self.core.rt.spawn(async move {
            let report =
                tokio::task::spawn_blocking(move || crate::disposal_backfill::run_historical_backfill(&store))
                    .await
                    .unwrap_or_default();
            *done.lock().unwrap() = Some(report);
            ctx.request_repaint();
        });
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

    /// Permanently delete one or many soft-deleted rows. The per-row 🗑 button
    /// and the toolbar's "Delete selected" both funnel through here — the
    /// only difference is how many ids are in `ids`. Runs sequentially on one
    /// task (these are same-drive renames-turned-deletes, not worth
    /// parallelizing) and posts each outcome as it completes so the UI
    /// updates progressively rather than waiting for the whole batch.
    fn spawn_trash_batch_permadelete(&mut self, ctx: &egui::Context, ids: Vec<i64>) {
        self.trash_action_pending.extend(ids.iter().copied());
        let store = self.core.store.clone();
        let done = self.trash_action_done.clone();
        let ctx = ctx.clone();
        self.core.rt.spawn(async move {
            for id in ids {
                let result = crate::disposal::permanently_delete_disposal_record(&store, id)
                    .await
                    .map_err(|e| e.to_string());
                done.lock().unwrap().push((id, result));
                ctx.request_repaint();
            }
        });
    }

    pub(super) fn trash_view(&mut self, ui: &mut egui::Ui) {
        self.ensure_trash_loaded();
        self.drain_trash_action_results();
        self.drain_trash_import_result();

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
            ui.separator();
            let ctx = ui.ctx().clone();
            if ui
                .add_enabled(!self.trash_import_running, egui::Button::new("⤵ Import history"))
                .on_hover_text(
                    "One-time scan for disposals that happened before this view existed — \
                     reconstructed from surviving DB traces (gap-splice patch paths, VOD-replace \
                     backups) and, for post-join head/live cleanup, a filename-naming guess. \
                     Only imports a candidate whose file is confirmed gone from disk on a \
                     currently-reachable drive. Imported rows show as read-only history \
                     (method/exact time unknown) — safe to re-run any time.",
                )
                .clicked()
            {
                self.spawn_trash_import(&ctx);
            }
            if self.trash_import_running {
                ui.spinner();
            }
            ui.separator();
            let n_selected = self.trash_selected.len();
            if ui
                .add_enabled(n_selected > 0, egui::Button::new(format!("🗑 Delete selected ({n_selected})")))
                .on_hover_text(
                    "Permanently delete every checked row across every channel. This cannot be undone.",
                )
                .clicked()
            {
                let pairs: Vec<(i64, String)> = self
                    .trash_records
                    .iter()
                    .filter(|r| self.trash_selected.contains(&r.row.id))
                    .map(|r| (r.row.id, r.row.trash_path.clone()))
                    .collect();
                if !pairs.is_empty() {
                    self.confirm_permadelete_trash = Some(pairs);
                }
            }
            if n_selected > 0 && ui.button("✕ Clear selection").clicked() {
                self.trash_selected.clear();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Show:");
            ui.checkbox(&mut self.trash_show_soft_deleted, DisposalRecordState::SoftDeleted.label())
                .on_hover_text("Files currently sitting in a trash folder — restorable.");
            ui.checkbox(&mut self.trash_show_permanent, DisposalRecordState::Permanent.label())
                .on_hover_text("Recycled or permanently deleted — history only.");
            ui.checkbox(&mut self.trash_show_restored, DisposalRecordState::Restored.label())
                .on_hover_text("Moved back to their original location.");
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
        let state_shown = |s: DisposalRecordState| match s {
            DisposalRecordState::SoftDeleted => self.trash_show_soft_deleted,
            DisposalRecordState::Permanent => self.trash_show_permanent,
            DisposalRecordState::Restored => self.trash_show_restored,
        };
        let mut by_channel: std::collections::BTreeMap<i64, Vec<usize>> = Default::default();
        for (i, r) in records.iter().enumerate() {
            if !state_shown(r.row.state) {
                continue;
            }
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
            let mut shown_any = false;
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
                shown_any = true;
                let known_bytes: Vec<i64> = idxs.iter().filter_map(|&i| records[i].row.bytes).collect();
                let header = if known_bytes.is_empty() {
                    format!("{display_name}  ({})", idxs.len())
                } else {
                    format!("{display_name}  ({}, {})", idxs.len(), fmt_bytes(known_bytes.iter().sum()))
                };
                egui::CollapsingHeader::new(header)
                    .id_salt(("trash_chan", *cid))
                    .default_open(true)
                    .show(ui, |ui| {
                        self.trash_group_table(ui, &ctx, *cid, &records, idxs);
                    });
                ui.add_space(4.0);
            }
            if !shown_any {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.weak("Nothing matches the current filter/Show selection.");
                });
            }
        });
    }

    /// One channel group's table — `Sel`/`Actions` lead (so a long `Path`
    /// never crowds them off to the right) and `Path` trails as the sole
    /// `Column::remainder()` so it soaks up whatever width is left instead of
    /// getting an arbitrary auto-sized slice. Every column is resizable
    /// (`egui_extras`'s own restart-persisted `TableBuilder` state, keyed by
    /// a per-channel `id_salt` so resizing one channel's columns doesn't
    /// affect another's). Not part of the `grid_columns`/`GridTableId`
    /// framework — this view has no per-column sort/filter/hide need, just a
    /// fixed set the user can resize.
    fn trash_group_table(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        cid: i64,
        records: &[DisposalRecordDisplay],
        idxs: &[usize],
    ) {
        const COLS: [GridCol; 10] = [
            GridCol { id: "sel",     title: "",        tooltip: "", min_width: 22.0,  initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "actions", title: "Actions", tooltip: "", min_width: 118.0, initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "when",    title: "When",    tooltip: "", min_width: 110.0, initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "title",   title: "Title",   tooltip: "The take's title at the time it was last logged.", min_width: 90.0, initial: 200.0, sortable: false, stretch: false },
            GridCol { id: "reason",  title: "Reason",  tooltip: "", min_width: 110.0, initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "method",  title: "Method",  tooltip: "", min_width: 80.0,  initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "state",   title: "State",   tooltip: "", min_width: 80.0,  initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "source",  title: "Source",  tooltip: "", min_width: 80.0,  initial: 0.0,   sortable: false, stretch: false },
            GridCol { id: "size",    title: "Size",    tooltip: "The file's size at the moment it was disposed of. Unknown for rows added by \"Import history\" — those files were already gone from disk by the time the scan ran.", min_width: 70.0, initial: 0.0, sortable: false, stretch: false },
            GridCol { id: "path",    title: "Path",    tooltip: "", min_width: 160.0, initial: 0.0,   sortable: false, stretch: true },
        ];

        // Which of this group's ids are actionable (soft-deleted) — feeds the
        // header's select-all checkbox.
        let group_softdeleted: Vec<i64> = idxs
            .iter()
            .map(|&i| &records[i].row)
            .filter(|r| r.state == DisposalRecordState::SoftDeleted)
            .map(|r| r.id)
            .collect();

        let mut tb = TableBuilder::new(ui)
            .id_salt(("trash_table", cid))
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        for c in &COLS {
            let col = if c.stretch {
                Column::remainder().at_least(c.min_width).clip(true)
            } else if c.initial > 0.0 {
                Column::initial(c.initial).at_least(c.min_width).clip(true)
            } else {
                Column::auto().at_least(c.min_width)
            };
            tb = tb.column(col);
        }
        tb.header(20.0, |mut h| {
            for c in &COLS {
                h.col(|ui| match c.id {
                    "sel" => {
                        if !group_softdeleted.is_empty() {
                            let mut all_selected =
                                group_softdeleted.iter().all(|id| self.trash_selected.contains(id));
                            if ui
                                .checkbox(&mut all_selected, "")
                                .on_hover_text("Select/deselect every soft-deleted row in this channel")
                                .changed()
                            {
                                if all_selected {
                                    self.trash_selected.extend(group_softdeleted.iter().copied());
                                } else {
                                    for id in &group_softdeleted {
                                        self.trash_selected.remove(id);
                                    }
                                }
                            }
                        }
                    }
                    _ if !c.title.is_empty() => {
                        let resp = ui.strong(c.title);
                        if !c.tooltip.is_empty() {
                            resp.on_hover_text(c.tooltip);
                        }
                    }
                    _ => {}
                });
            }
        })
        .body(|mut body| {
            for &i in idxs {
                let rec = &records[i];
                let row = &rec.row;
                let current_path: &str = match row.state {
                    DisposalRecordState::SoftDeleted => &row.trash_path,
                    DisposalRecordState::Restored => &row.original_path,
                    DisposalRecordState::Permanent => {
                        if !row.trash_path.is_empty() { &row.trash_path } else { &row.original_path }
                    }
                };
                body.row(22.0, |mut tr| {
                    for c in &COLS {
                        tr.col(|ui| match c.id {
                            "sel" => {
                                if row.state == DisposalRecordState::SoftDeleted {
                                    let mut checked = self.trash_selected.contains(&row.id);
                                    if ui.checkbox(&mut checked, "").changed() {
                                        if checked {
                                            self.trash_selected.insert(row.id);
                                        } else {
                                            self.trash_selected.remove(&row.id);
                                        }
                                    }
                                }
                            }
                            "actions" => {
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
                                        self.confirm_permadelete_trash =
                                            Some(vec![(row.id, current_path.to_string())]);
                                    }
                                }
                                let path_buf =
                                    (!current_path.is_empty()).then(|| std::path::PathBuf::from(current_path));
                                let file_ok = path_buf.as_ref().is_some_and(|p| self.fs_probes.is_file(p));
                                if ui
                                    .add_enabled(file_ok, egui::Button::new("▶"))
                                    .on_hover_text("Open file")
                                    .clicked()
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
                            }
                            "when" => ts_label(ui, row.disposed_at),
                            "title" => {
                                if rec.take_title.is_empty() {
                                    ui.weak("—");
                                } else {
                                    ui.add(egui::Label::new(&rec.take_title).truncate())
                                        .on_hover_text(&rec.take_title);
                                }
                            }
                            "reason" => {
                                let resp = ui.label(&row.reason);
                                if rec.take_started_at.is_some() || row.rec_id != 0 {
                                    resp.on_hover_ui(|ui| {
                                        ui.label(format!("Recording #{}", row.rec_id));
                                        if let Some(started) = rec.take_started_at {
                                            ui.label("Take started:");
                                            ts_label(ui, started);
                                        }
                                    });
                                }
                            }
                            "method" => {
                                ui.label(row.method.label());
                            }
                            "state" => {
                                ui.label(row.state.label());
                            }
                            "source" => {
                                let source_color = match row.confidence {
                                    crate::disposal::DisposalConfidence::Live => ui.visuals().text_color(),
                                    crate::disposal::DisposalConfidence::HistoricalExact => {
                                        egui::Color32::from_rgb(200, 170, 90)
                                    }
                                    crate::disposal::DisposalConfidence::HistoricalGuess => {
                                        egui::Color32::from_rgb(210, 120, 90)
                                    }
                                };
                                ui.colored_label(source_color, row.confidence.label()).on_hover_text(
                                    match row.confidence {
                                        crate::disposal::DisposalConfidence::Live => {
                                            "Logged the moment this disposal happened — method, path, \
                                             and time are all exact."
                                        }
                                        crate::disposal::DisposalConfidence::HistoricalExact => {
                                            "Reconstructed by \"Import history\" from a DB column that \
                                             still held this exact path, verified absent from disk. The \
                                             method and \"When\" time are unknown — \"When\" shows the \
                                             take's end time as a stand-in, not the real disposal time."
                                        }
                                        crate::disposal::DisposalConfidence::HistoricalGuess => {
                                            "Reconstructed by \"Import history\" from a filename naming \
                                             convention, not a stored path — an educated guess, verified \
                                             absent from disk. The method and \"When\" time are unknown \
                                             — \"When\" shows the take's end time as a stand-in."
                                        }
                                    },
                                );
                            }
                            "size" => match row.bytes {
                                Some(b) => {
                                    ui.label(fmt_bytes(b));
                                }
                                None => {
                                    ui.weak("—").on_hover_text(
                                        "Unknown — not captured for this row (an \"Import history\" \
                                         row, or the stat raced something else removing the file).",
                                    );
                                }
                            },
                            "path" => {
                                ui.add(egui::Label::new(egui::RichText::new(current_path).small().monospace()).truncate())
                                    .on_hover_text(if current_path.is_empty() { "(no path)" } else { current_path });
                            }
                            _ => {}
                        });
                    }
                });
            }
        });
    }

    /// Confirmation dialog for the Trash view's "Permanently delete" action —
    /// the one irreversible step in this view (Restore just moves the file
    /// back; this removes it for good). `items` is one `(id, path)` pair for
    /// the per-row 🗑 button, or every checked row's worth for the toolbar's
    /// "Delete selected".
    #[allow(deprecated)] // CentralPanel::show inside show_viewport_immediate — same as the other confirm_* windows
    pub(super) fn confirm_permadelete_trash_window(&mut self, ctx: &egui::Context) {
        let Some(items) = self.confirm_permadelete_trash.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;
        const MAX_SHOWN: usize = 12;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("confirm_permadelete_trash_vp"),
            egui::ViewportBuilder::default()
                .with_title("Permanently delete")
                .with_inner_size([480.0, if items.len() > 1 { 320.0 } else { 150.0 }])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if items.len() == 1 {
                        ui.label("Permanently delete this file? This cannot be undone.");
                        ui.add_space(4.0);
                        ui.add(egui::Label::new(egui::RichText::new(&items[0].1).small().monospace()).truncate());
                    } else {
                        ui.label(format!(
                            "Permanently delete {} files? This cannot be undone.",
                            items.len()
                        ));
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                            for (_, path) in items.iter().take(MAX_SHOWN) {
                                ui.add(egui::Label::new(egui::RichText::new(path).small().monospace()).truncate());
                            }
                            if items.len() > MAX_SHOWN {
                                ui.weak(format!("(+{} more)", items.len() - MAX_SHOWN));
                            }
                        });
                    }
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
            let ids: Vec<i64> = items.iter().map(|(id, _)| *id).collect();
            self.spawn_trash_batch_permadelete(ctx, ids);
            self.confirm_permadelete_trash = None;
        } else if do_cancel || !open {
            self.confirm_permadelete_trash = None;
        }
    }
}
