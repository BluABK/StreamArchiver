//! Shared windows: confirms, ad/meta/recording-properties popups,
//! rename/preset/reorder dialogs, process manager, add/edit-stream form.

use super::*;

/// Deferred-viewport content for `ad_popup_window` — derived once per
/// recording id.
pub(super) struct AdPopupContent {
    pub(super) total: i64,
    pub(super) lines: Vec<String>,
    pub(super) closed: bool,
}

/// Deferred-viewport content for `history_popup_window` — derived once per
/// monitor id (see [`PopupRegistry`]).
pub(super) struct HistoryPopupContent {
    pub(super) title: String,
    pub(super) lines: Vec<String>,
    pub(super) closed: bool,
}

/// Deferred-viewport content for `chapters_popup_window` — derived once per
/// recording id.
pub(super) struct ChaptersPopupContent {
    pub(super) channel_name: String,
    pub(super) output_path: String,
    pub(super) lines: Vec<String>,
    pub(super) closed: bool,
}

/// Deferred-viewport content shared by `vod_info_popup_window` and
/// `remux_info_popup_window` — both just display a channel name + the whole
/// `Recording` the caller already had at click time, no store read needed.
pub(super) struct VodInfoContent {
    pub(super) channel_name: String,
    pub(super) rec: crate::models::Recording,
    pub(super) closed: bool,
}

/// Backing draft for the "Mark hype train" dialog — replaces the old
/// per-field `hype_mark_channel`/`hype_mark_abs` + a plain `show_hype_mark:
/// bool`; the deferred closure mutates this directly instead of the old
/// copy-out/copy-back-in dance a `show_viewport_immediate` closure needed.
/// Backing state for the "Move instance to another channel" dialog.
pub(super) struct MoveInstanceState {
    pub(super) mid: i64,
    /// The ComboBox selection — lives here (not a per-frame local) so it
    /// persists across frames/calls.
    pub(super) dest: Option<i64>,
    pub(super) do_move: bool,
    pub(super) closed: bool,
}

/// Backing state for the "Merge channel into another" dialog.
pub(super) struct MergeChannelState {
    pub(super) src: i64,
    /// The ComboBox selection — lives here (not a per-frame local) so it
    /// persists across frames/calls.
    pub(super) dest: Option<i64>,
    pub(super) do_merge: bool,
    pub(super) closed: bool,
}

/// Backing state for the "Rename recording" dialog.
pub(super) struct RenameDialogState {
    pub(super) rec_id: i64,
    pub(super) draft: String,
    pub(super) preview: String,
    /// Set by the deferred closure on OK; read back by `rename_dialog_window`
    /// next call.
    pub(super) do_rename: bool,
    /// Set by the deferred closure on Cancel/close.
    pub(super) closed: bool,
}

pub(super) struct HypeMarkDraft {
    pub(super) channel: i64,
    pub(super) mins_ago: i64,
    pub(super) abs: String,
    pub(super) dur: i64,
    pub(super) do_mark: bool,
    pub(super) closed: bool,
}

/// Backing state for the "⚙ Sensitivity" per-channel hype-train override
/// editor — replaces the old `hype_override_for: Option<i64>` +
/// `hype_override_draft` pair.
pub(super) struct HypeOverrideState {
    pub(super) channel_id: i64,
    pub(super) name: String,
    pub(super) global: crate::hype::HypeTuning,
    pub(super) draft: crate::hype::HypeOverride,
    pub(super) do_save: bool,
    pub(super) closed: bool,
}

/// Deferred-viewport content for `meta_popup_window` — derived once per
/// popup key ([`MetaPopup::key`]).
pub(super) struct MetaPopupContent {
    pub(super) lines: Vec<String>,
    pub(super) scope: &'static str,
    pub(super) closed: bool,
}

/// What the metadata-change popup shows.
#[derive(Clone)]
pub(super) enum MetaPopup {
    /// A single take's change log (recording id).
    Take(i64),
    /// A whole stream's takes — `(recording id, started_at)`, oldest-first —
    /// aggregated chronologically with the per-take re-baselines omitted.
    Stream(Vec<(i64, i64)>),
}

impl MetaPopup {
    /// Stable identity for dedup + the per-window viewport id: the (first)
    /// recording id it shows.
    pub(super) fn key(&self) -> i64 {
        match self {
            MetaPopup::Take(rid) => *rid,
            MetaPopup::Stream(takes) => takes.first().map(|(rid, _)| *rid).unwrap_or(0),
        }
    }
}

/// One open "Recording properties" window + its editable notes draft.
pub(super) struct RecPropsPopup {
    pub(super) rec_id: i64,
    pub(super) notes: String,
    /// Set by the deferred closure when `notes` changes; the wrapper persists
    /// it (DB + in-memory cache) and resets this next call.
    pub(super) notes_dirty: bool,
    /// Set by the deferred closure on close; read back next call.
    pub(super) closed: bool,
}

/// One open "Schedule event properties" window + its rescan draft (model +
/// effort combo selections, independent per open window so comparing two
/// events at once doesn't share a draft).
pub(super) struct EventPropsPopup {
    pub(super) segment_id: i64,
    pub(super) rescan_model: String,
    pub(super) rescan_effort: String,
    /// Set by the deferred closure on close; read back by
    /// `event_properties_window` next call.
    pub(super) closed: bool,
    pub(super) rescan_clicked: bool,
}

/// Draft state for the "Edit schedule item" dialog. Times are edited as local
/// `YYYY-MM-DD` / `HH:MM` strings; on save they're parsed back to unix seconds and
/// written via [`Store::update_schedule_segment_manual`](crate::store::Store::update_schedule_segment_manual),
/// which flips the row to the protected `"manual"` source so later automatic
/// refreshes don't overwrite the correction.
pub(super) struct EditScheduleDraft {
    /// `schedule_segment.id` of the row being edited.
    pub(super) segment_id: i64,
    /// For the dialog heading.
    pub(super) channel_name: String,
    /// Original source id — shown in the heading so the user sees what they're
    /// overriding (e.g. an OCR'd banner).
    pub(super) source: String,
    pub(super) title: String,
    pub(super) category: String,
    /// Local `YYYY-MM-DD` / `HH:MM` of the start.
    pub(super) date: String,
    pub(super) time: String,
    /// Optional local end — empty strings mean "no end time".
    pub(super) end_date: String,
    pub(super) end_time: String,
    /// Validation message shown in red (empty = none).
    pub(super) error: String,
    /// Set by the deferred closure's button clicks; read back by
    /// `edit_schedule_window` next call.
    pub(super) save: bool,
    pub(super) delete: bool,
    pub(super) closed: bool,
}

/// Backing state for the scheduled-recording Add/Edit dialog (schema v51).
/// Force-starts a recording at a specific time or on a weekly repeat,
/// bypassing Auto — see `Supervisor::scheduled_recordings_tick`.
pub(super) struct ScheduledRecordingForm {
    /// `None` = creating a new rule.
    pub(super) id: Option<i64>,
    pub(super) monitor_id: i64,
    /// For the dialog heading only — not persisted.
    pub(super) channel_name: String,
    pub(super) monitor_url: String,
    pub(super) label: String,
    pub(super) kind: RecurrenceKind,
    /// Local `YYYY-MM-DD` / `HH:MM` — used when `kind == Once`.
    pub(super) date: String,
    pub(super) time: String,
    /// Mon..Sun (index 0..6, matching `DOW_MON..DOW_SUN`) — used when `kind == Weekly`.
    pub(super) days: [bool; 7],
    /// Local `HH:MM` time-of-day — used when `kind == Weekly`.
    pub(super) weekly_time: String,
    /// Optional local end date for the recurrence (inclusive); empty = no end.
    pub(super) until_date: String,
    /// Auto-stop after a fixed duration instead of recording until the stream
    /// ends naturally.
    pub(super) use_duration: bool,
    pub(super) duration_minutes: String,
    pub(super) enabled: bool,
    /// Validation message shown in red (empty = none).
    pub(super) error: String,
}

impl ScheduledRecordingForm {
    pub(super) fn new_for_monitor(monitor_id: i64, channel_name: &str, monitor_url: &str) -> Self {
        ScheduledRecordingForm {
            id: None,
            monitor_id,
            channel_name: channel_name.to_string(),
            monitor_url: monitor_url.to_string(),
            label: String::new(),
            kind: RecurrenceKind::Once,
            date: String::new(),
            time: String::new(),
            days: [false; 7],
            weekly_time: "20:00".to_string(),
            until_date: String::new(),
            use_duration: false,
            duration_minutes: "60".to_string(),
            enabled: true,
            error: String::new(),
        }
    }

    pub(super) fn from_existing(row: &ScheduledRecordingWithNames) -> Self {
        let r = &row.rec;
        let (date, time) = r.start_at.map(split_local_datetime).unwrap_or_default();
        let mut days = [false; 7];
        let bits = r.days_of_week.unwrap_or(0);
        for (i, d) in days.iter_mut().enumerate() {
            *d = bits & (1 << i) != 0;
        }
        let weekly_time = split_time_of_day(r.time_of_day_secs.unwrap_or(0));
        let until_date = r.until.map(|u| split_local_datetime(u).0).unwrap_or_default();
        ScheduledRecordingForm {
            id: Some(r.id),
            monitor_id: r.monitor_id,
            channel_name: row.channel_name.clone(),
            monitor_url: row.monitor_url.clone(),
            label: r.label.clone(),
            kind: r.kind,
            date,
            time,
            days,
            weekly_time,
            until_date,
            use_duration: r.duration_secs.is_some(),
            duration_minutes: (r.duration_secs.unwrap_or(3600) / 60).max(1).to_string(),
            enabled: r.enabled,
            error: String::new(),
        }
    }

    /// Prefilled from a calendar entry (the "📅 Schedule recording…" right-click
    /// action) — a one-off rule at that entry's start time, defaulting the
    /// duration to the entry's own known length when available.
    pub(super) fn from_schedule_entry(s: &UpcomingStream) -> Self {
        let (date, time) = split_local_datetime(s.start_time);
        let (use_duration, duration_minutes) = match s.end_time {
            Some(end) if end > s.start_time => (true, ((end - s.start_time) / 60).max(1).to_string()),
            _ => (false, "60".to_string()),
        };
        ScheduledRecordingForm {
            id: None,
            monitor_id: s.monitor_id,
            channel_name: s.channel_name.clone(),
            monitor_url: s.url.clone(),
            label: s.title.clone(),
            kind: RecurrenceKind::Once,
            date,
            time,
            days: [false; 7],
            weekly_time: "20:00".to_string(),
            until_date: String::new(),
            use_duration,
            duration_minutes,
            enabled: true,
            error: String::new(),
        }
    }
}

/// Draft state for the "Merge schedule events" preview dialog.
pub(super) struct MergePreviewDraft {
    /// Snapshots of the events to merge (2+), sorted highest-priority first.
    /// Index 0 is pre-selected as the primary (can be changed by the user).
    pub(super) segments: Vec<UpcomingStream>,
    /// Which element of `segments` is chosen as the primary (shown in the calendar).
    pub(super) primary_idx: usize,
    /// Validation/error message (empty = none).
    pub(super) error: String,
    /// Set by the deferred closure's button clicks; read back by
    /// `merge_preview_window` next call.
    pub(super) merge: bool,
    pub(super) cancel: bool,
}

pub(super) struct SavePresetDraft {
    /// Template string to be saved.
    pub(super) template: String,
    /// Name the user has typed for this preset.
    pub(super) name: String,
    /// Validation or save error message (empty = none).
    pub(super) error: String,
    /// Set by the deferred closure on Save/Enter; read back by
    /// `save_preset_window` next call.
    pub(super) do_save: bool,
    /// Set by the deferred closure on Cancel/close.
    pub(super) closed: bool,
}

/// Deferred-viewport state for `processes_window`. `rows` is refreshed by the
/// wrapper every call from `self.processes` (cheap — already just an
/// in-memory `Vec`, the actual list-processes work happens off-thread and
/// lands in `self.processes` on its own throttle). `entries` mirrors
/// `self.processes_grid.entries` (synced back after every call, same as
/// every other column-choosable table); `reorder_columns` is set by the
/// header's column-chooser context menu inside the closure (it can't reach
/// `self.reorder_columns` directly) and relayed into the real field by the
/// wrapper next call, mirroring `IssuesPopupState::reorder_columns`. Actions
/// key on `pid` rather than a `Vec` index — the deferred closure that set
/// `act` may have run on a stale snapshot of `self.processes` by the time
/// the wrapper applies it, so a `usize` index could point at the wrong row.
pub(super) struct ProcessesPopupState {
    pub(super) rows: Vec<crate::app_core::ProcInfo>,
    pub(super) entries: Vec<ColumnEntry>,
    /// Mirrors `GridState::last_order`/`widths` — kept here instead since
    /// the deferred closure can't reach `self.processes_grid`. Both are
    /// deliberately session-only even in `GridState` itself (see
    /// `WidthMemory`'s doc), so living only in this popup and never syncing
    /// back to `self` changes nothing observable.
    pub(super) last_order: Option<Vec<usize>>,
    pub(super) widths: grid_columns::WidthMemory,
    pub(super) reorder_columns: Option<Arc<Mutex<ReorderColumnsState>>>,
    pub(super) act: Option<ProcessesAct>,
    pub(super) closed: bool,
}

pub(super) enum ProcessesAct {
    Refresh,
    Stop(u32),
    Kill(u32),
    RevealLog(u32),
    RevealDir(u32),
}

impl StreamArchiverApp {
    /// Modal confirmation for deleting a monitor (the only destructive action).
    pub(super) fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.confirm_delete.clone() else {
            return;
        };
        let shared = self.popup_shared();
        let result = confirm_dialog_deferred(
            ctx,
            shared,
            egui::ViewportId::from_hash_of("del_monitor_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete monitor")
                .with_inner_size([380.0, 130.0])
                .with_resizable(false),
            &state,
            |ui, (_, name), result| {
                ui.label(format!("Delete this capture instance for “{name}”?"));
                ui.label("Removes the monitor and its settings. Recorded files are kept.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        *result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        *result = Some(false);
                    }
                });
            },
        );

        if result == Some(true) {
            let id = state.lock().unwrap().payload.0;
            // Stop a running capture first so the process isn't orphaned when its
            // history row is cascade-deleted.
            if self.core.active.lock().unwrap().contains_key(&id) {
                self.core.manual(ManualCommand::Stop(id));
            }
            // The channel container is left in place even if this was its last
            // instance (you can add another instance to it).
            match self.core.store.delete_monitor(id) {
                Ok(()) => self.status = "Instance deleted.".into(),
                Err(e) => self.status = format!("Error: {e}"),
            }
            if self.selected_monitor == Some(id) {
                self.selected_monitor = None;
            }
            self.confirm_delete = None;
            self.reload_rows();
        } else if result == Some(false) {
            self.confirm_delete = None;
        }
    }

    /// Modal confirmation for deleting a whole channel (and all its instances +
    /// their history rows; recorded files are kept).
    pub(super) fn confirm_delete_channel_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.confirm_delete_channel.clone() else {
            return;
        };
        let shared = self.popup_shared();
        let result = confirm_dialog_deferred(
            ctx,
            shared,
            egui::ViewportId::from_hash_of("del_channel_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete channel")
                .with_inner_size([400.0, 130.0])
                .with_resizable(false),
            &state,
            |ui, (_, name), result| {
                ui.label(format!("Delete the channel “{name}” and all its instances?"));
                ui.label("Removes every instance and its history. Recorded files are kept.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        *result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        *result = Some(false);
                    }
                });
            },
        );

        if result == Some(true) {
            let id = state.lock().unwrap().payload.0;
            // Stop any of this channel's instances that are recording, so no
            // capture is left running after its rows are cascade-deleted.
            let active: std::collections::HashSet<i64> =
                self.core.active.lock().unwrap().keys().copied().collect();
            for mid in self
                .rows
                .iter()
                .filter(|r| r.channel.id == id && active.contains(&r.monitor.id))
                .map(|r| r.monitor.id)
                .collect::<Vec<_>>()
            {
                self.core.manual(ManualCommand::Stop(mid));
            }
            match self.core.store.delete_channel(id) {
                Ok(()) => self.status = "Channel deleted.".into(),
                Err(e) => self.status = format!("Error: {e}"),
            }
            self.confirm_delete_channel = None;
            self.reload_rows();
        } else if result == Some(false) {
            self.confirm_delete_channel = None;
        }
    }

    /// Modal confirmation for a manual "🗑🔥 Delete file from disk" (take-row
    /// context menu, see `crate::manual_delete`) — the one destructive action
    /// in this app that removes a MEDIA FILE, not just a history row, so it
    /// names the resolved disposal method up front rather than a generic
    /// "are you sure". Reaching this dialog at all already required all three
    /// `manual_delete` gates to be on; this confirm is the last checkpoint.
    pub(super) fn confirm_delete_file_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.confirm_delete_file.clone() else {
            return;
        };
        let shared = self.popup_shared();
        let result = confirm_dialog_deferred(
            ctx,
            shared,
            egui::ViewportId::from_hash_of("del_recfile_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete file from disk")
                .with_inner_size([460.0, 170.0])
                .with_resizable(false),
            &state,
            |ui, cdf, result| {
                ui.label(format!("Delete the captured file for “{}”?", cdf.label));
                ui.label(egui::RichText::new(&cdf.path).weak().small());
                ui.add_space(6.0);
                ui.label(format!(
                    "This will: {} (Settings → Automatic deletion).",
                    cdf.method.label()
                ));
                ui.label(
                    "The take's history row stays — title, stats, chat log, chapters, \
                     notes are all kept.",
                );
                if cdf.method == crate::disposal::DisposalMethod::Delete {
                    ui.colored_label(
                        grid::HL_ERROR_TEXT,
                        "Permanent deletion — this cannot be undone.",
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("Delete").color(grid::HL_ERROR_TEXT))
                        .clicked()
                    {
                        *result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        *result = Some(false);
                    }
                });
            },
        );

        if result == Some(true) {
            self.confirm_delete_file = None;
            let cdf = state.lock().unwrap();
            let (rec_id, channel_id, monitor_id, path) =
                (cdf.payload.rec_id, cdf.payload.channel_id, cdf.payload.monitor_id, cdf.payload.path.clone());
            drop(cdf);
            self.spawn_manual_delete_file(ctx, rec_id, channel_id, monitor_id, path);
        } else if result == Some(false) {
            self.confirm_delete_file = None;
        }
    }

    /// Runs the actual disposal for a confirmed manual "Delete file from
    /// disk" — same `dispose_media` resolution (trash/Recycle Bin/permanent)
    /// automatic cleanup uses, then clears the row's `output_path` on success
    /// so the row stays but every file-dependent action on it goes inert.
    /// `AppEvent::RecordingUpdated` drops the owning monitor's `rec_cache`
    /// entry on the next event-drain pass, same as a recovery/VOD-archive
    /// update — the row picks up the cleared path without waiting for F5.
    fn spawn_manual_delete_file(
        &mut self,
        ctx: &egui::Context,
        rec_id: i64,
        channel_id: i64,
        monitor_id: i64,
        path: String,
    ) {
        self.manual_delete_pending.insert(rec_id);
        let store = self.core.store.clone();
        let events = self.core.events.clone();
        let done = self.manual_delete_done.clone();
        let ctx = ctx.clone();
        self.core.rt.spawn(async move {
            let p = std::path::PathBuf::from(&path);
            let outcome = match crate::disposal::dispose_media(
                &store,
                channel_id,
                monitor_id,
                &p,
                rec_id,
                "manual delete (user action)",
            )
            .await
            {
                Ok(d) => {
                    let _ = store.update_recording_output_path(rec_id, "");
                    let _ = events.send(crate::events::AppEvent::RecordingUpdated { recording_id: rec_id });
                    Ok(d.describe().to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            done.lock().unwrap().push((rec_id, outcome));
            ctx.request_repaint();
        });
    }

    /// Drain completed manual-delete outcomes (see `spawn_manual_delete_file`
    /// and `spawn_manual_delete_stream_files`) and surface them: clear the
    /// pending flags, status-bar the result (same feedback path every other
    /// manual action in this app uses). A single outcome keeps the precise
    /// per-file phrasing; a batch (the bulk stream action, or several
    /// single-file deletes that happened to land in the same drain pass)
    /// gets one summary line instead of the last one silently winning.
    pub(super) fn drain_manual_delete_results(&mut self) {
        let drained: Vec<crate::manual_delete::ManualDeleteOutcome> =
            std::mem::take(&mut *self.manual_delete_done.lock().unwrap());
        if drained.is_empty() {
            return;
        }
        let ok = drained.iter().filter(|(_, r)| r.is_ok()).count();
        let err = drained.len() - ok;
        for (rid, _) in &drained {
            self.manual_delete_pending.remove(rid);
        }
        self.status = if let [(_, result)] = drained.as_slice() {
            match result {
                Ok(desc) => format!("Recording file {desc}."),
                Err(e) => format!("Error deleting file: {e}"),
            }
        } else if err == 0 {
            format!("Deleted {ok} file(s).")
        } else {
            format!("Deleted {ok} file(s), {err} failed.")
        };
    }

    /// Modal confirmation for a bulk "🗑🔥 Delete all take files from disk"
    /// (stream-row context menu, see `crate::manual_delete`) — the
    /// broadcast-level equivalent of `confirm_delete_file_window`. Lists
    /// every take about to lose its file, grouped by resolved disposal
    /// method (a per-recording trigger override can make them differ), plus
    /// the total size being reclaimed.
    pub(super) fn confirm_delete_stream_files_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.confirm_delete_stream_files.clone() else {
            return;
        };
        let shared = self.popup_shared();
        let result = confirm_dialog_deferred(
            ctx,
            shared,
            egui::ViewportId::from_hash_of("del_streamfiles_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete all take files")
                .with_inner_size([460.0, 220.0])
                .with_resizable(false),
            &state,
            |ui, cdsf, result| {
                let total_bytes: i64 = cdsf.items.iter().map(|(_, _, b, _)| *b).sum();
                let mut by_method: Vec<(crate::disposal::DisposalMethod, usize)> = Vec::new();
                for (_, _, _, m) in &cdsf.items {
                    match by_method.iter_mut().find(|(bm, _)| bm == m) {
                        Some((_, n)) => *n += 1,
                        None => by_method.push((*m, 1)),
                    }
                }
                let any_permanent = cdsf
                    .items
                    .iter()
                    .any(|(_, _, _, m)| *m == crate::disposal::DisposalMethod::Delete);

                ui.label(format!(
                    "Delete {} captured file(s) for “{}”? ({})",
                    cdsf.items.len(),
                    cdsf.label,
                    fmt_bytes(total_bytes)
                ));
                ui.add_space(6.0);
                for (method, n) in &by_method {
                    ui.label(format!("{n} file(s): {}", method.label()));
                }
                ui.add_space(6.0);
                ui.label(
                    "Every take's history row stays — title, stats, chat log, chapters, \
                     notes are all kept.",
                );
                if any_permanent {
                    ui.colored_label(
                        grid::HL_ERROR_TEXT,
                        "At least one file is permanently deleted — this cannot be undone.",
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("Delete all").color(grid::HL_ERROR_TEXT))
                        .clicked()
                    {
                        *result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        *result = Some(false);
                    }
                });
            },
        );

        if result == Some(true) {
            self.confirm_delete_stream_files = None;
            let cdsf = state.lock().unwrap();
            let (channel_id, monitor_id, items) =
                (cdsf.payload.channel_id, cdsf.payload.monitor_id, cdsf.payload.items.clone());
            drop(cdsf);
            self.spawn_manual_delete_stream_files(ctx, channel_id, monitor_id, items);
        } else if result == Some(false) {
            self.confirm_delete_stream_files = None;
        }
    }

    /// Runs the actual bulk disposal for a confirmed "Delete all take files
    /// from disk" — one background task, disposing each take's file in
    /// sequence (same `dispose_media` resolution as the single-take action)
    /// so every outcome lands in `manual_delete_done` together and
    /// `drain_manual_delete_results` can report one summary line instead of
    /// one per file.
    fn spawn_manual_delete_stream_files(
        &mut self,
        ctx: &egui::Context,
        channel_id: i64,
        monitor_id: i64,
        items: Vec<(i64, String, i64, crate::disposal::DisposalMethod)>,
    ) {
        for (rec_id, ..) in &items {
            self.manual_delete_pending.insert(*rec_id);
        }
        let store = self.core.store.clone();
        let events = self.core.events.clone();
        let done = self.manual_delete_done.clone();
        let ctx = ctx.clone();
        self.core.rt.spawn(async move {
            let mut outcomes = Vec::with_capacity(items.len());
            for (rec_id, path, ..) in items {
                let p = std::path::PathBuf::from(&path);
                let outcome = match crate::disposal::dispose_media(
                    &store,
                    channel_id,
                    monitor_id,
                    &p,
                    rec_id,
                    "manual delete (user action, bulk stream)",
                )
                .await
                {
                    Ok(d) => {
                        let _ = store.update_recording_output_path(rec_id, "");
                        let _ = events
                            .send(crate::events::AppEvent::RecordingUpdated { recording_id: rec_id });
                        Ok(d.describe().to_string())
                    }
                    Err(e) => Err(e.to_string()),
                };
                outcomes.push((rec_id, outcome));
            }
            done.lock().unwrap().extend(outcomes);
            ctx.request_repaint();
        });
    }

    /// "Move instance to another channel" dialog: pick a destination channel
    /// container for one capture instance. Everything monitor-keyed
    /// (recordings, schedule, stats, chat) moves implicitly; posts/about
    /// history is re-keyed by the store call. See
    /// [`crate::store::Store::move_monitor_to_channel`].
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn move_instance_window(&mut self, ctx: &egui::Context) {
        let Some(popup_state) = self.move_instance_dialog.clone() else {
            return;
        };
        let mid = popup_state.lock().unwrap().mid;
        let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid) else {
            self.move_instance_dialog = None; // instance deleted meanwhile
            return;
        };
        let src_cid = row.channel.id;
        let src_name = row.channel.name.clone();
        let inst = instance_label(&row.monitor.url);
        // (id, name) of every possible destination — cloned up front so the
        // viewport closure doesn't borrow `self`.
        let dests: Vec<(i64, String)> = self
            .channels
            .iter()
            .filter(|c| c.id != src_cid)
            .map(|c| (c.id, c.name.clone()))
            .collect();
        // Cloned again for the closure specifically — `dests`/`inst` are
        // also needed after `show_deferred_popup` returns (post-processing).
        let dests_for_closure = dests.clone();
        let inst_for_closure = inst.clone();

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("move_instance_vp"),
            egui::ViewportBuilder::default()
                .with_title("Move instance")
                .with_inner_size([440.0, 190.0])
                .with_resizable(false),
            popup_state.clone(),
            shared,
            move |ctx, s, _shared| {
                let dests = &dests_for_closure;
                let inst = &inst_for_closure;
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(format!("Move “{src_name}”'s {inst} instance into:"));
                    ui.add_space(4.0);
                    let sel_name = s.dest
                        .and_then(|d| dests.iter().find(|(id, _)| *id == d))
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "Select a channel…".into());
                    egui::ComboBox::from_id_salt("move_instance_dest")
                        .width(260.0)
                        .selected_text(sel_name)
                        .show_ui(ui, |ui| {
                            for (id, name) in dests {
                                ui.selectable_value(&mut s.dest, Some(*id), name);
                            }
                        });
                    if dests.is_empty() {
                        ui.weak("There is no other channel to move it into.");
                    }
                    ui.add_space(6.0);
                    ui.label(
                        "Its recordings, schedule, stats, posts, and about history move \
                         with it; the destination channel's own settings (Auto/Enabled, \
                         color, triggers) apply to it from then on.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(s.dest.is_some(), egui::Button::new("Move"))
                            .clicked()
                        {
                            s.do_move = true;
                        }
                        if ui.button("Cancel").clicked() {
                            s.closed = true;
                        }
                    });
                });
            },
        );

        let (dest, do_move, closed) = {
            let mut s = popup_state.lock().unwrap();
            let result = (s.dest, s.do_move, s.closed);
            s.do_move = false;
            s.closed = false;
            result
        };

        if do_move && let Some(d) = dest {
            let dest_name = dests
                .iter()
                .find(|(id, _)| *id == d)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            match self.core.store.move_monitor_to_channel(mid, d) {
                Ok(()) => self.status = format!("Moved the {inst} instance to “{dest_name}”."),
                Err(e) => self.status = format!("Error: {e}"),
            }
            // Both containers' avatar/colour picks may change with the roster.
            for cid in [src_cid, d] {
                self.channel_icons.remove(&cid);
                self.channel_twitch_colors.remove(&cid);
            }
            self.move_instance_dialog = None;
            self.reload_rows();
        } else if closed {
            self.move_instance_dialog = None;
        }
        // Still open, no action: `popup_state` already sits in
        // `self.move_instance_dialog` unchanged — the selection lives on the
        // SAME `Arc<Mutex<>>`, no write-back needed.
    }

    /// "Merge channel into another" dialog: move ALL of the source channel's
    /// instances to a destination channel, then delete the (now empty) source.
    /// See [`crate::store::Store::merge_channel_into`].
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn merge_channel_window(&mut self, ctx: &egui::Context) {
        let Some(popup_state) = self.merge_channel_dialog.clone() else {
            return;
        };
        let src = popup_state.lock().unwrap().src;
        let Some(src_name) = self
            .channels
            .iter()
            .find(|c| c.id == src)
            .map(|c| c.name.clone())
        else {
            self.merge_channel_dialog = None; // channel deleted meanwhile
            return;
        };
        let ninst = self.rows.iter().filter(|r| r.channel.id == src).count();
        let dests: Vec<(i64, String)> = self
            .channels
            .iter()
            .filter(|c| c.id != src)
            .map(|c| (c.id, c.name.clone()))
            .collect();
        // Cloned again for the closure specifically — `dests`/`src_name` are
        // also needed after `show_deferred_popup` returns (post-processing).
        let dests_for_closure = dests.clone();
        let src_name_for_closure = src_name.clone();

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("merge_channel_vp"),
            egui::ViewportBuilder::default()
                .with_title("Merge channel")
                .with_inner_size([460.0, 210.0])
                .with_resizable(false),
            popup_state.clone(),
            shared,
            move |ctx, s, _shared| {
                let dests = &dests_for_closure;
                let src_name = &src_name_for_closure;
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(format!(
                        "Merge “{src_name}” ({ninst} instance{}) into:",
                        if ninst == 1 { "" } else { "s" }
                    ));
                    ui.add_space(4.0);
                    let sel_name = s.dest
                        .and_then(|d| dests.iter().find(|(id, _)| *id == d))
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "Select a channel…".into());
                    egui::ComboBox::from_id_salt("merge_channel_dest")
                        .width(260.0)
                        .selected_text(sel_name)
                        .show_ui(ui, |ui| {
                            for (id, name) in dests {
                                ui.selectable_value(&mut s.dest, Some(*id), name);
                            }
                        });
                    if dests.is_empty() {
                        ui.weak("There is no other channel to merge into.");
                    }
                    ui.add_space(6.0);
                    ui.label(
                        "Every instance moves over with its recordings, schedule, \
                         stats, posts, and about history; group memberships are \
                         carried too. The emptied source channel is then deleted. \
                         Its channel-level settings (color, triggers, scopes) are \
                         NOT carried over — the destination's apply.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(s.dest.is_some(), egui::Button::new("Merge"))
                            .clicked()
                        {
                            s.do_merge = true;
                        }
                        if ui.button("Cancel").clicked() {
                            s.closed = true;
                        }
                    });
                });
            },
        );

        let (dest, do_merge, closed) = {
            let mut s = popup_state.lock().unwrap();
            let result = (s.dest, s.do_merge, s.closed);
            s.do_merge = false;
            s.closed = false;
            result
        };

        if do_merge && let Some(d) = dest {
            let dest_name = dests
                .iter()
                .find(|(id, _)| *id == d)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            match self.core.store.merge_channel_into(src, d) {
                Ok((moved, deleted)) => {
                    self.status = format!(
                        "Merged “{src_name}” into “{dest_name}”: {moved} instance{} moved{}.",
                        if moved == 1 { "" } else { "s" },
                        if deleted { "" } else { " (source kept — not empty)" },
                    );
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
            for cid in [src, d] {
                self.channel_icons.remove(&cid);
                self.channel_twitch_colors.remove(&cid);
            }
            self.merge_channel_dialog = None;
            self.reload_rows();
        } else if closed {
            self.merge_channel_dialog = None;
        }
    }

    /// Modal confirmation for tombstoning a schedule segment (it won't reappear
    /// on the next refresh).
    pub(super) fn confirm_delete_segment_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.confirm_delete_segment.clone() else {
            return;
        };
        let shared = self.popup_shared();
        let result = confirm_dialog_deferred(
            ctx,
            shared,
            egui::ViewportId::from_hash_of("del_segment_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete schedule item")
                .with_inner_size([400.0, 120.0])
                .with_resizable(false),
            &state,
            |ui, _sid, result| {
                ui.label("Permanently delete this schedule item?");
                ui.label("It will be suppressed and won't reappear on refresh.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        *result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        *result = Some(false);
                    }
                });
            },
        );

        if result == Some(true) {
            let sid = state.lock().unwrap().payload;
            if let Err(e) = self.core.store.delete_schedule_segment(sid) {
                self.status = format!("Error deleting schedule item: {e}");
            } else {
                self.schedule_hidden_segments.remove(&sid);
                self.spawn_reload_schedule();
                self.status = "Schedule item deleted.".into();
            }
            self.confirm_delete_segment = None;
        } else if result == Some(false) {
            self.confirm_delete_segment = None;
        }
    }

    /// Confirmation dialog for "Quit & stop recordings" tray action.
    pub(super) fn confirm_quit_stop_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.confirm_quit_stop.clone() else {
            self.confirm_quit_stop_raised = false;
            return;
        };
        let shared = self.popup_shared();
        let vp_id = egui::ViewportId::from_hash_of("confirm_quit_stop_vp");
        let result = confirm_dialog_deferred(
            ctx,
            shared,
            vp_id,
            egui::ViewportBuilder::default()
                .with_title("Stop recordings and quit?")
                .with_inner_size([380.0, 130.0])
                .with_resizable(false)
                // A quit confirmation must not open BEHIND the main window
                // (observed: it did, and quitting looked wedged). Keep it on
                // top; it's a tiny short-lived dialog.
                .with_always_on_top(),
            &state,
            |ui, (), result| {
                ui.add_space(4.0);
                ui.label("This will terminate all active recordings immediately.");
                ui.label("In-progress captures will be finalized from whatever was written.");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let stop_btn = egui::Button::new("Stop & Quit")
                        .fill(egui::Color32::from_rgb(180, 40, 40));
                    if ui.add(stop_btn).clicked() {
                        *result = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        *result = Some(false);
                    }
                });
            },
        );

        // One-shot focus raise the frame after the viewport is created —
        // always-on-top keeps it visible, this also gives it the keyboard.
        if !self.confirm_quit_stop_raised {
            self.confirm_quit_stop_raised = true;
            ctx.send_viewport_cmd_to(vp_id, egui::ViewportCommand::Focus);
        }

        if result == Some(true) {
            self.core
                .force_stop_on_quit
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.quitting = true;
            // Same watchdog stand-down as the tray Quit path — the
            // stop-recordings exit blocks the UI thread even longer (kill +
            // finalize drain before the runtime shutdown).
            self.heartbeat.set_active(false);
            self.confirm_quit_stop = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if result == Some(false) {
            self.confirm_quit_stop = None;
        }
    }
    /// Render every open ad-breaks window (one per take).
    pub(super) fn ad_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.ad_popups.len() {
            let rid = self.ad_popups[i];
            if self.ad_popup_window(ctx, rid) {
                closed.push(rid);
            }
        }
        if !closed.is_empty() {
            self.ad_popups.retain(|r| !closed.contains(r));
        }
        self.ad_popup_registry.retain(&self.ad_popups);
    }

    /// Window listing where ad breaks cause hard cuts in a take's finished
    /// file. Opened by double-clicking an Ads / Ad time cell. Returns true
    /// once closed.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn ad_popup_window(&mut self, ctx: &egui::Context, rid: i64) -> bool {
        // Reuse the cached cut list (cleared on reload) rather than re-querying
        // every frame the popup is open.
        if !self.ad_break_cache.contains_key(&rid) {
            let v = self
                .core
                .store
                .ad_breaks_for_recording(rid)
                .unwrap_or_default();
            self.ad_break_cache.insert(rid, v);
        }
        let state = self.ad_popup_registry.get_or_init(rid, || {
            let breaks = self.ad_break_cache.get(&rid).cloned().unwrap_or_default();
            let total: i64 = breaks.iter().map(|b| b.duration_secs).sum();
            let lines = ad_cut_lines(&breaks);
            AdPopupContent { total, lines, closed: false }
        });
        if state.lock().unwrap().closed {
            return true;
        }
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("ad_breaks_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Ad breaks — cut points (take #{rid})"))
                .with_inner_size([360.0, 260.0]),
            state,
            shared,
            |ctx, content, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    content.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if content.lines.is_empty() {
                        ui.label("No ad breaks recorded for this take.");
                        return;
                    }
                    ui.label(format!(
                        "{} ad break(s), {} total. Each is a hard cut in the recorded file \
                         (streamlink filters ad segments out).",
                        content.lines.len(),
                        fmt_duration(content.total),
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &content.lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(content.lines.join("\n"));
                    }
                });
            },
        );
        false
    }

    /// Load a recording's metadata-change rows into the cache if absent.
    pub(super) fn ensure_meta_cached(&mut self, rid: i64) {
        if !self.meta_change_cache.contains_key(&rid) {
            let v = self
                .core
                .store
                .meta_changes_for_recording(rid)
                .unwrap_or_default();
            self.meta_change_cache.insert(rid, v);
        }
    }

    /// Load a monitor's all-time change-history rows into the cache if absent.
    pub(super) fn ensure_history_cached(&mut self, monitor_id: i64) {
        if !self.history_change_cache.contains_key(&monitor_id) {
            let v = self
                .core
                .store
                .monitor_stream_changes(monitor_id)
                .unwrap_or_default();
            self.history_change_cache.insert(monitor_id, v);
        }
    }

    /// Render every open "channel history" window (one per monitor).
    pub(super) fn history_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.history_popups.len() {
            let mid = self.history_popups[i];
            if self.history_popup_window(ctx, mid) {
                closed.push(mid);
            }
        }
        if !closed.is_empty() {
            self.history_popups.retain(|m| !closed.contains(m));
        }
        self.history_popup_registry.retain(&self.history_popups);
    }

    /// One "channel history" window (all-time title/category/tags changes for
    /// a monitor, independent of any recording); returns true once the user
    /// has closed it (checked at the START of the next call, one frame after
    /// the deferred closure itself set the flag — the window is destroyed by
    /// the OS asynchronously, and this app-state cleanup doesn't need to be
    /// any more synchronous than that). Content is derived once per monitor
    /// id when first opened (matches the pre-migration behavior, which only
    /// ever populated `history_change_cache` once too via
    /// `ensure_history_cached`'s "if absent" guard — never live-refreshed
    /// while open).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn history_popup_window(&mut self, ctx: &egui::Context, monitor_id: i64) -> bool {
        self.ensure_history_cached(monitor_id);
        let state = self.history_popup_registry.get_or_init(monitor_id, || {
            let changes = self.history_change_cache.get(&monitor_id).cloned().unwrap_or_default();
            let row = self.core.store.get_monitor_with_channel(monitor_id).ok().flatten();
            let channel_name = row.as_ref().map(|r| r.channel.name.clone()).unwrap_or_default();
            // Include the platform (and URL, when this channel has more than
            // one instance on the SAME platform) so opening history for
            // several of a channel's instances at once — e.g. from the
            // channel Properties window's rollup button — doesn't show
            // several identically-titled windows with no way to tell them
            // apart.
            let title = match &row {
                Some(r) => {
                    let siblings = self
                        .rows
                        .iter()
                        .filter(|o| o.channel.id == r.channel.id && o.monitor.platform() == r.monitor.platform())
                        .count();
                    if siblings > 1 {
                        format!(
                            "{channel_name} ({}, {}) — title/category/tags history",
                            r.monitor.platform().tag(),
                            instance_label(&r.monitor.url),
                        )
                    } else {
                        format!("{channel_name} ({}) — title/category/tags history", r.monitor.platform().tag())
                    }
                }
                None => format!("{channel_name} — title/category/tags history"),
            };
            HistoryPopupContent { title, lines: monitor_change_lines(&changes), closed: false }
        });
        if state.lock().unwrap().closed {
            return true;
        }
        let shared = self.popup_shared();
        let title = state.lock().unwrap().title.clone();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("channel_history_vp", monitor_id)),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([480.0, 320.0]),
            state,
            shared,
            |ctx, content, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    content.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if content.lines.is_empty() {
                        ui.label("No title, category, or tags changes recorded yet.");
                        return;
                    }
                    ui.label(format!(
                        "{} change(s), newest first — every title/category/tags transition \
                         ever observed for this instance, whether or not it was being \
                         recorded.",
                        content.lines.len(),
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &content.lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(content.lines.join("\n"));
                    }
                });
            },
        );
        false
    }

    /// Load one recording's (channel name, file path, parsed chapter list)
    /// into the cache if absent — the actually-embedded list from
    /// `Recording.chapters_json`, not a live re-derivation.
    pub(super) fn ensure_chapters_popup_cached(&mut self, rec_id: i64) {
        if self.chapters_popup_cache.contains_key(&rec_id) {
            return;
        }
        let Some(rec) = self.core.store.get_recording(rec_id).ok().flatten() else { return };
        let channel_name = self
            .core
            .store
            .get_monitor_with_channel(rec.monitor_id)
            .ok()
            .flatten()
            .map(|r| r.channel.name)
            .unwrap_or_default();
        let chapters: Vec<crate::chapters::Chapter> =
            serde_json::from_str(&rec.chapters_json).unwrap_or_default();
        self.chapters_popup_cache.insert(rec_id, (channel_name, rec.output_path, chapters));
    }

    /// Render every open chapters-detail window (one per recording) — the
    /// Background view's ℹ button on a Chapters task row.
    pub(super) fn chapters_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.chapters_popups.len() {
            let rid = self.chapters_popups[i];
            if self.chapters_popup_window(ctx, rid) {
                closed.push(rid);
            }
        }
        if !closed.is_empty() {
            self.chapters_popups.retain(|r| !closed.contains(r));
        }
        self.chapters_popup_registry.retain(&self.chapters_popups);
    }

    /// Window showing which stream, which file, and the embedded chapter
    /// list (title + timestamp) for one recording; returns true once closed
    /// (see `history_popup_window`'s doc comment for the async-close shape).
    /// Content derived once per recording id, matching the pre-migration
    /// `chapters_popup_cache`'s own "if absent" load-once behavior.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn chapters_popup_window(&mut self, ctx: &egui::Context, rid: i64) -> bool {
        self.ensure_chapters_popup_cached(rid);
        let state = self.chapters_popup_registry.get_or_init(rid, || {
            let (channel_name, output_path, chapters) =
                self.chapters_popup_cache.get(&rid).cloned().unwrap_or_default();
            let lines: Vec<String> = chapters
                .iter()
                .map(|c| format!("{}  {}", fmt_duration(c.at_secs.round() as i64), c.title))
                .collect();
            ChaptersPopupContent { channel_name, output_path, lines, closed: false }
        });
        if state.lock().unwrap().closed {
            return true;
        }
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("chapters_detail_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("{} — chapters (take #{rid})", state.lock().unwrap().channel_name))
                .with_inner_size([460.0, 320.0]),
            state,
            shared,
            |ctx, content, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    content.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(egui::RichText::new(&content.channel_name).strong());
                    ui.horizontal(|ui| {
                        ui.label("File:");
                        ui.label(egui::RichText::new(&content.output_path).monospace().small());
                        if ui.small_button("📋").on_hover_text("Copy file path").clicked() {
                            ui.ctx().copy_text(content.output_path.clone());
                        }
                    });
                    ui.add_space(6.0);
                    if content.lines.is_empty() {
                        ui.label(
                            "No chapters recorded for this take yet (embedding may still be \
                             in progress, or none of the enabled kinds found anything to mark).",
                        );
                        return;
                    }
                    ui.label(format!("{} chapter(s):", content.lines.len()));
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &content.lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(content.lines.join("\n"));
                    }
                });
            },
        );
        false
    }

    /// Render every open VOD-status popup — Stream History's ℹ VOD button.
    pub(super) fn vod_info_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.vod_info_popups.len() {
            let rid = self.vod_info_popups[i];
            if self.vod_info_popup_window(ctx, rid) {
                closed.push(rid);
            }
        }
        if !closed.is_empty() {
            self.vod_info_popups.retain(|r| !closed.contains(r));
        }
        self.vod_info_popup_registry.retain(&self.vod_info_popups);
    }

    /// Window showing one take's VOD/recovery/archive-download status;
    /// returns true once closed. No store read needed — the caller already
    /// had the full `Recording` at click time (see
    /// `history::stream_history_view`).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn vod_info_popup_window(&mut self, ctx: &egui::Context, rid: i64) -> bool {
        let Some((channel_name, rec)) = self.vod_info_popup_cache.get(&rid).cloned() else {
            return true;
        };
        let state = self
            .vod_info_popup_registry
            .get_or_init(rid, || VodInfoContent { channel_name, rec, closed: false });
        if state.lock().unwrap().closed {
            return true;
        }
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("vod_info_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("{} — VOD status (take #{rid})", state.lock().unwrap().channel_name))
                .with_inner_size([420.0, 280.0]),
            state,
            shared,
            |ctx, content, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    content.closed = true;
                }
                let rec = &content.rec;
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(egui::RichText::new(&content.channel_name).strong());
                    ui.add_space(6.0);
                    egui::Grid::new("vod_info_grid").num_columns(2).spacing([8.0, 4.0]).show(
                        ui,
                        |ui| {
                            ui.label("VOD state");
                            ui.label(rec.vod_state.as_deref().unwrap_or("—"));
                            ui.end_row();
                            ui.label("VOD id");
                            ui.label(rec.vod_id.as_deref().unwrap_or("—"));
                            ui.end_row();
                            ui.label("Muted seconds");
                            ui.label(
                                rec.vod_muted_secs
                                    .map(fmt_duration)
                                    .unwrap_or_else(|| "—".into()),
                            );
                            ui.end_row();
                            ui.label("Views");
                            ui.label(
                                rec.vod_views.map(|v| v.to_string()).unwrap_or_else(|| "—".into()),
                            );
                            ui.end_row();
                            ui.label("Recovery state");
                            ui.label(rec.recovery_state.as_deref().unwrap_or("—"));
                            ui.end_row();
                            ui.label("Archive-download state");
                            ui.label(rec.vod_dl_state.as_deref().unwrap_or("—"));
                            ui.end_row();
                        },
                    );
                    if let Some(p) = &rec.recovered_path {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("Recovered file:");
                            ui.label(egui::RichText::new(p).monospace().small());
                            if ui.small_button("📋").on_hover_text("Copy path").clicked() {
                                ui.ctx().copy_text(p.clone());
                            }
                        });
                    }
                    if let Some(p) = &rec.vod_dl_path {
                        ui.horizontal(|ui| {
                            ui.label("Archived VOD:");
                            ui.label(egui::RichText::new(p).monospace().small());
                            if ui.small_button("📋").on_hover_text("Copy path").clicked() {
                                ui.ctx().copy_text(p.clone());
                            }
                        });
                    }
                });
            },
        );
        false
    }

    /// Render every open remux-status popup — Stream History's ℹ Remux button.
    pub(super) fn remux_info_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.remux_info_popups.len() {
            let rid = self.remux_info_popups[i];
            if self.remux_info_popup_window(ctx, rid) {
                closed.push(rid);
            }
        }
        if !closed.is_empty() {
            self.remux_info_popups.retain(|r| !closed.contains(r));
        }
        self.remux_info_popup_registry.retain(&self.remux_info_popups);
    }

    /// Window showing one take's remux/promote status; returns true once closed.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn remux_info_popup_window(&mut self, ctx: &egui::Context, rid: i64) -> bool {
        let Some((channel_name, rec)) = self.remux_info_popup_cache.get(&rid).cloned() else {
            return true;
        };
        let state = self
            .remux_info_popup_registry
            .get_or_init(rid, || VodInfoContent { channel_name, rec, closed: false });
        if state.lock().unwrap().closed {
            return true;
        }
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("remux_info_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("{} — remux status (take #{rid})", state.lock().unwrap().channel_name))
                .with_inner_size([460.0, 220.0]),
            state,
            shared,
            |ctx, content, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    content.closed = true;
                }
                let rec = &content.rec;
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(egui::RichText::new(&content.channel_name).strong());
                    ui.horizontal(|ui| {
                        ui.label("File:");
                        ui.label(egui::RichText::new(&rec.output_path).monospace().small());
                        if ui.small_button("📋").on_hover_text("Copy file path").clicked() {
                            ui.ctx().copy_text(rec.output_path.clone());
                        }
                    });
                    ui.add_space(6.0);
                    if is_remux_pending(rec) {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 140, 30),
                            "⚠ Still a .ts capture in the cache dir — the automatic remux to \
                             MKV failed.",
                        );
                        ui.label(
                            "Right-click the take in Streams → \"🔄 Re-remux to MKV\" to retry.",
                        );
                    } else if is_stuck_in_cache(rec) {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 140, 30),
                            "⚠ Capture completed but the promote-to-output-dir move never \
                             finished.",
                        );
                    } else {
                        ui.label("Finished in its final container.");
                    }
                });
            },
        );
        false
    }

    // (collab history helpers below; state struct + line formatter at the
    // bottom of this file)

    /// Open the 🤝 collab-history window for a channel (loads its sessions
    /// once; reopening refreshes).
    pub(super) fn open_collab_history(&mut self, channel_id: i64) {
        let sessions = self
            .core
            .store
            .collab_sessions_for_channel(channel_id, 500)
            .unwrap_or_default();
        let channel_name = self
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        self.collab_history =
            Some(Arc::new(Mutex::new(CollabHistoryState { channel_name, sessions, closed: false })));
    }

    /// The "🤝 Collab history" window: one line per stored "Stream Together"
    /// session (newest first) — when, how long, with whom, who hosted, and
    /// whether it came from Shared Chat or a title @mention.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn collab_history_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.collab_history.clone() else { return };
        if state.lock().unwrap().closed {
            self.collab_history = None;
            return;
        }
        let channel_name = state.lock().unwrap().channel_name.clone();
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("collab_history_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("{channel_name} — collab history"))
                .with_inner_size([560.0, 360.0]),
            state,
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if s.sessions.is_empty() {
                        ui.label(
                            "No collabs recorded yet. Sessions appear here once a live \
                             Twitch instance is seen in a \"Stream Together\" shared \
                             chat (or @mentions someone in its title).",
                        );
                        return;
                    }
                    ui.label(format!(
                        "{} session(s), newest first. 💬 = Shared Chat (confirmed), \
                         @ = title mention (heuristic); a duration ending in \"+\" is \
                         still ongoing.",
                        s.sessions.len()
                    ));
                    ui.add_space(6.0);
                    let lines = collab_session_lines(&s.sessions);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui
                        .button("📋  Copy")
                        .on_hover_text("Copy the session list as text")
                        .clicked()
                    {
                        ui.ctx().copy_text(lines.join("\n"));
                    }
                });
            },
        );
    }

    /// Open the "which streams was this collab in" drill-down for `partner`
    /// (the display name from the aggregate Collabs table's Sessions count).
    pub(super) fn open_partner_sessions(&mut self, partner: &str) {
        let rows = self.core.store.collab_sessions_for_partner(partner).unwrap_or_default();
        self.partner_sessions = Some(Arc::new(Mutex::new(PartnerSessionsState {
            partner: partner.to_string(),
            rows,
            closed: false,
            jump: None,
        })));
    }

    /// The "🤝 {partner} — sessions" window: every stored collab session that
    /// partner appeared in, across all monitored channels — the drill-down
    /// from the App Stats Collabs table's Sessions count. Each row can jump
    /// straight to that channel's Streams row.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn partner_sessions_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.partner_sessions.clone() else { return };
        if state.lock().unwrap().closed {
            self.partner_sessions = None;
            return;
        }
        let partner = state.lock().unwrap().partner.clone();
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("partner_sessions_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("🤝 {partner} — sessions"))
                .with_inner_size([560.0, 360.0]),
            state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if s.rows.is_empty() {
                        ui.label("No sessions found for this partner.");
                        return;
                    }
                    ui.label(format!(
                        "{} session(s), newest first. 💬 = Shared Chat (confirmed), \
                         @ = title mention (heuristic); a duration ending in \"+\" is \
                         still ongoing.",
                        s.rows.len()
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                        egui::Grid::new("partner_sessions_grid")
                            .num_columns(5)
                            .striped(true)
                            .spacing([12.0, 4.0])
                            .show(ui, |ui| {
                                ui.strong("Channel");
                                ui.strong("Start");
                                ui.strong("Duration");
                                ui.strong("With");
                                ui.label("");
                                ui.end_row();
                                for r in s.rows.clone() {
                                    ui.label(&r.channel_name).on_hover_text(format!(
                                        "Broadcast (stream id): {}",
                                        if r.stream_id.is_empty() { "unknown" } else { &r.stream_id }
                                    ));
                                    ui.label(fmt_datetime_short(r.first_seen_at));
                                    let span = match r.ended_at {
                                        Some(end) => fmt_duration((end - r.first_seen_at).max(0)),
                                        None => format!(
                                            "{}+",
                                            fmt_duration(
                                                (r.last_seen_at - r.first_seen_at).max(0)
                                            )
                                        ),
                                    };
                                    ui.label(span);
                                    let marker = if r.source == "shared_chat" { "💬" } else { "@" };
                                    let with = if r.co_partners.is_empty() {
                                        marker.to_string()
                                    } else {
                                        format!("{marker} {}", r.co_partners.join(", "))
                                    };
                                    ui.label(with);
                                    if ui
                                        .small_button("Jump")
                                        .on_hover_text(
                                            "Switch to Streams and select this channel's row.",
                                        )
                                        .clicked()
                                    {
                                        s.jump = Some(r.monitor_id);
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                });
            },
        );
        let jump = state.lock().unwrap().jump.take();
        if let Some(mid) = jump {
            self.switch_view(View::Streams);
            self.selected_monitor = Some(mid);
            self.partner_sessions = None;
        }
    }

    /// Render every open title/category-changes window (one per take/stream).
    pub(super) fn meta_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.meta_popups.len() {
            let popup = self.meta_popups[i].clone();
            let key = popup.key();
            if self.meta_popup_window(ctx, popup) {
                closed.push(key);
            }
        }
        if !closed.is_empty() {
            self.meta_popups.retain(|p| !closed.contains(&p.key()));
        }
        let keys: Vec<i64> = self.meta_popups.iter().map(MetaPopup::key).collect();
        self.meta_popup_registry.retain(&keys);
    }

    /// One title/category-changes window; returns true once closed. Content
    /// derived once per popup key (the aggregated change list doesn't change
    /// while the window's open, matching every other popup in this file).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn meta_popup_window(&mut self, ctx: &egui::Context, popup: MetaPopup) -> bool {
        let key = popup.key();
        // Build the change list: one take directly, or a stream's takes merged
        // chronologically with the per-take re-baselines dropped.
        match &popup {
            MetaPopup::Take(rid) => self.ensure_meta_cached(*rid),
            MetaPopup::Stream(takes) => {
                for (rid, _) in takes {
                    self.ensure_meta_cached(*rid);
                }
            }
        }
        let state = self.meta_popup_registry.get_or_init(key, || {
            let (changes, multi) = match &popup {
                MetaPopup::Take(rid) => {
                    (self.meta_change_cache.get(rid).cloned().unwrap_or_default(), false)
                }
                MetaPopup::Stream(takes) => {
                    let loaded: Vec<(i64, Vec<StreamMetaChange>)> = takes
                        .iter()
                        .map(|(rid, started)| {
                            (*started, self.meta_change_cache.get(rid).cloned().unwrap_or_default())
                        })
                        .collect();
                    (aggregate_stream_changes(&loaded), takes.len() > 1)
                }
            };
            // Only actual changes (the initial value of each field is the
            // starting state, not a change); shown as `old → new`.
            let lines = meta_change_lines(&changes);
            let scope = if multi {
                "across this stream's takes"
            } else {
                "during this recording"
            };
            MetaPopupContent { lines, scope, closed: false }
        });
        if state.lock().unwrap().closed {
            return true;
        }
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("title_changes_vp", key)),
            egui::ViewportBuilder::default()
                .with_title("Title & category changes")
                .with_inner_size([460.0, 280.0]),
            state,
            shared,
            |ctx, content, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    content.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if content.lines.is_empty() {
                        ui.label("No title or category changes recorded.");
                        return;
                    }
                    ui.label(format!(
                        "{} change(s) {} (offset from the start; each shows the \
                         value before → after).",
                        content.lines.len(),
                        content.scope,
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &content.lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(content.lines.join("\n"));
                    }
                });
            },
        );
        false
    }

    /// Render every open recording-properties window (one per take).
    pub(super) fn recording_properties_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.rec_props_popups.len() {
            let rid = self.rec_props_popups[i].lock().unwrap().rec_id;
            if self.recording_properties_window(ctx, i) {
                closed.push(rid);
            }
        }
        if !closed.is_empty() {
            self.rec_props_popups.retain(|p| !closed.contains(&p.lock().unwrap().rec_id));
        }
    }

    /// Properties dialog for a single recording take.
    /// Opened via right-click → Properties on a history-tree take row.
    /// Returns true when the window should close.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn recording_properties_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let popup_state = self.rec_props_popups[idx].clone();
        let rid = popup_state.lock().unwrap().rec_id;
        // Pull the recording out of the cache; close if the take was deleted.
        let Some(rec) = self
            .rec_cache
            .values()
            .flat_map(|v| v.iter())
            .find(|r| r.id == rid)
            .cloned()
        else {
            return true;
        };
        let now = crate::models::now_unix();
        // Scoped strictly to this one take (matched by stream id, or by its
        // own time window when the platform never stamped one) — never
        // mixed with another take or another instance of the same channel,
        // see `Store::stream_stats_for_monitor`. Resolved up front (cloned
        // out of `take_stats_cache`) so the viewport closure below doesn't
        // need to borrow `self`.
        let viewer_stats: Option<crate::models::StreamStatRow> = self
            .take_stats_cache
            .get(&rec.monitor_id)
            .and_then(|v| find_take_stats(v, &rec))
            .cloned();

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("recording_props_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Recording properties — take #{rid}"))
                .with_inner_size([500.0, 540.0]),
            popup_state.clone(),
            shared,
            move |ctx, popup, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    popup.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        // ── File ──────────────────────────────────────────
                        ui.strong("File");
                        egui::Grid::new("rp_file")
                            .num_columns(2)
                            .striped(true)
                            .min_col_width(90.0)
                            .show(ui, |ui| {
                                ui.label("Path");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&rec.output_path).monospace(),
                                        )
                                        .truncate(),
                                    );
                                    if !rec.output_path.is_empty()
                                        && ui
                                            .small_button("📋")
                                            .on_hover_text("Copy path")
                                            .clicked()
                                    {
                                        ui.ctx().copy_text(rec.output_path.clone());
                                    }
                                });
                                ui.end_row();
                                ui.label("Size");
                                ui.label(fmt_bytes(rec.bytes));
                                ui.end_row();
                                ui.label("Status");
                                ui.label(&rec.status);
                                ui.end_row();
                                if let Some(code) = rec.exit_code {
                                    ui.label("Exit code");
                                    ui.label(code.to_string());
                                    ui.end_row();
                                }
                                if !rec.trigger_info.is_empty() {
                                    ui.label("Trigger").on_hover_text(
                                        "This recording was started by a trigger-word rule.",
                                    );
                                    ui.label(format!("⚡ {}", rec.trigger_info));
                                    ui.end_row();
                                }
                            });

                        ui.add_space(8.0);
                        // ── Capture timing ────────────────────────────────
                        ui.strong("Capture");
                        egui::Grid::new("rp_timing")
                            .num_columns(2)
                            .striped(true)
                            .min_col_width(90.0)
                            .show(ui, |ui| {
                                ui.label("Started");
                                ui.label(fmt_datetime_short(rec.started_at));
                                ui.end_row();
                                let predates_fix = rec.ended_at_predates_accuracy_fix();
                                let stale_hover = "This take finished before a fix (2026-07-26) that \
                                     stamps this from the capture's real exit time — before then, a \
                                     slow remux queued at the disk gate could push this hours later \
                                     than the broadcast actually ended. The capture itself isn't \
                                     necessarily incomplete; compare against the file's own duration \
                                     (e.g. via a media prober) to see whether — and by how much — \
                                     this is inflated.";
                                if let Some(ended) = rec.ended_at {
                                    ui.label("Ended");
                                    let lbl = ui.label(fmt_datetime_short(ended));
                                    if predates_fix {
                                        lbl.on_hover_text(format!("⚠ {stale_hover}"));
                                    }
                                    ui.end_row();
                                }
                                ui.label("Duration");
                                let dur_lbl = ui.label(fmt_duration(rec.duration_secs(now)));
                                if predates_fix {
                                    dur_lbl.on_hover_text(format!("⚠ {stale_hover}"));
                                }
                                ui.end_row();
                                if let Some(live) = rec.went_live_at {
                                    ui.label("Went live");
                                    let approx =
                                        if rec.went_live_approx { " (approx)" } else { "" };
                                    ui.label(format!(
                                        "{}{}",
                                        fmt_datetime_short(live),
                                        approx
                                    ));
                                    ui.end_row();
                                }
                                if let Some(lost) = rec.lost_secs {
                                    ui.label("Lost footage");
                                    ui.label(format!(
                                        "{} ({})",
                                        fmt_duration(lost),
                                        fmt_duration_secs(lost)
                                    ));
                                    ui.end_row();
                                }
                            });

                        ui.add_space(8.0);
                        // ── Stream info ───────────────────────────────────
                        ui.strong("Stream");
                        egui::Grid::new("rp_stream")
                            .num_columns(2)
                            .striped(true)
                            .min_col_width(90.0)
                            .show(ui, |ui| {
                                if !rec.title.is_empty() {
                                    ui.label("Title");
                                    ui.add(
                                        egui::Label::new(&rec.title).wrap_mode(egui::TextWrapMode::Wrap),
                                    );
                                    ui.end_row();
                                }
                                if !rec.category.is_empty() {
                                    ui.label("Category");
                                    ui.label(&rec.category);
                                    ui.end_row();
                                }
                                if rec.ad_count > 0 {
                                    ui.label("Ad breaks");
                                    ui.label(format!(
                                        "{} break(s), {} total",
                                        rec.ad_count,
                                        fmt_duration(rec.ad_secs)
                                    ));
                                    ui.end_row();
                                }
                                if rec.meta_change_count > 0 {
                                    ui.label("Meta changes");
                                    ui.label(format!("{} change(s)", rec.meta_change_count));
                                    ui.end_row();
                                }
                                if let Some(sid) = &rec.stream_id {
                                    ui.label("Stream ID");
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(sid).monospace())
                                            .truncate(),
                                    );
                                    ui.end_row();
                                }
                                if let Some(tg) = &rec.take_group {
                                    ui.label("Take group");
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(tg).monospace())
                                            .truncate(),
                                    );
                                    ui.end_row();
                                }
                            });

                        // ── Viewer stats ──────────────────────────────────
                        ui.add_space(8.0);
                        ui.strong("Viewer stats").on_hover_text(
                            "Scoped strictly to this take — matched by stream id, or by \
                             its own time window when the platform never stamped one. \
                             Never mixed with another take or another instance of the \
                             same channel.",
                        );
                        match &viewer_stats {
                            Some(s) => {
                                egui::Grid::new("rp_viewers")
                                    .num_columns(2)
                                    .striped(true)
                                    .min_col_width(90.0)
                                    .show(ui, |ui| {
                                        ui.label("Peak");
                                        ui.label(fmt_viewers(s.peak_viewers));
                                        ui.end_row();
                                        ui.label("Average")
                                            .on_hover_text("Airtime-weighted average viewers");
                                        ui.label(fmt_viewers(s.avg_viewers.round() as i64));
                                        ui.end_row();
                                        ui.label("Tracked").on_hover_text(
                                            "Sampled live time (viewer-count polling coverage)",
                                        );
                                        ui.label(fmt_duration(s.live_secs));
                                        ui.end_row();
                                        let [subs, gifted, bits, rin, rout, mods] = s.totals;
                                        if subs > 0 || gifted > 0 {
                                            ui.label("Subs");
                                            ui.label(if gifted > 0 {
                                                format!("{subs} (+{gifted} gifted)")
                                            } else {
                                                subs.to_string()
                                            });
                                            ui.end_row();
                                        }
                                        if bits > 0 {
                                            ui.label("Bits");
                                            ui.label(bits.to_string());
                                            ui.end_row();
                                        }
                                        if rin > 0 || rout > 0 {
                                            ui.label("Raids");
                                            ui.label(format!("{rin} in, {rout} out"));
                                            ui.end_row();
                                        }
                                        if mods > 0 {
                                            ui.label("Mod actions")
                                                .on_hover_text("Deletions + timeouts + bans");
                                            ui.label(mods.to_string());
                                            ui.end_row();
                                        }
                                    });
                            }
                            None => {
                                ui.weak("No viewer stats recorded for this take.").on_hover_text(
                                    "Either it predates viewer-history tracking, was too \
                                     short to sample, or the platform never stamped a \
                                     stream id and this take's window doesn't overlap a \
                                     tracked broadcast.",
                                );
                            }
                        }

                        // ── VOD (Twitch only) ─────────────────────────────
                        if rec.vod_state.is_some() {
                            ui.add_space(8.0);
                            ui.strong("VOD");
                            egui::Grid::new("rp_vod")
                                .num_columns(2)
                                .striped(true)
                                .min_col_width(90.0)
                                .show(ui, |ui| {
                                    ui.label("State");
                                    let (label, color) = match rec.vod_state.as_deref() {
                                        Some("pending") => ("Checking…", egui::Color32::GRAY),
                                        Some("found") => ("Published", egui::Color32::from_rgb(80, 200, 80)),
                                        Some("not_published") => ("Not published", egui::Color32::from_rgb(220, 80, 80)),
                                        _ => ("Unknown", egui::Color32::GRAY),
                                    };
                                    ui.colored_label(color, label);
                                    ui.end_row();
                                    if let Some(vod_url) = rec.vod_url() {
                                        ui.label("VOD URL");
                                        ui.hyperlink_to(&vod_url, &vod_url);
                                        ui.end_row();
                                    }
                                    if let Some(muted) = rec.vod_muted_secs {
                                        ui.label("Muted");
                                        if muted == 0 {
                                            ui.colored_label(egui::Color32::from_rgb(80, 200, 80), "None (clean copy)");
                                        } else {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(220, 160, 30),
                                                format!("{} muted (online copy damaged)", fmt_duration(muted)),
                                            );
                                        }
                                        ui.end_row();
                                    }
                                });
                        }

                        if !rec.log_excerpt.is_empty() {
                            ui.add_space(8.0);
                            ui.strong("Log excerpt");
                            egui::ScrollArea::vertical()
                                .id_salt("rp_log")
                                .max_height(90.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&rec.log_excerpt)
                                                .monospace()
                                                .small(),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Wrap),
                                    );
                                });
                        }

                        // ── Notes (editable) ──────────────────────────────
                        ui.add_space(8.0);
                        ui.strong("Notes");
                        let resp = ui.add(
                            egui::TextEdit::multiline(&mut popup.notes)
                                .hint_text("Add notes for this take…")
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        if resp.changed() {
                            popup.notes_dirty = true;
                        }
                    });
                });
            },
        );
        let (notes, notes_dirty) = {
            let mut p = popup_state.lock().unwrap();
            let result = (p.notes.clone(), p.notes_dirty);
            p.notes_dirty = false;
            result
        };
        if notes_dirty {
            // Update in-memory cache so the draft stays in sync if the dialog
            // is closed and reopened without a full reload.
            for recs in self.rec_cache.values_mut() {
                for r in recs.iter_mut() {
                    if r.id == rid {
                        r.notes = notes.clone();
                    }
                }
            }
            let _ = self.core.store.set_recording_notes(rid, &notes);
        }
        popup_state.lock().unwrap().closed
    }

    pub(super) fn event_properties_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.event_props_popups.len() {
            let sid = self.event_props_popups[i].lock().unwrap().segment_id;
            if self.event_properties_window(ctx, i) {
                closed.push(sid);
            }
        }
        if !closed.is_empty() {
            self.event_props_popups.retain(|p| !closed.contains(&p.lock().unwrap().segment_id));
        }
    }

    /// Properties dialog for a single schedule event. Opened by clicking an
    /// event tile in the Schedule calendar. Shows every field the calendar
    /// knows about this occurrence plus the OCR attribution (model/confidence,
    /// when scanned) and a "rescan this event" action.
    ///
    /// Closes automatically once its `segment_id` no longer resolves in
    /// `schedule_all` — covers both manual deletion AND a successful rescan
    /// (which necessarily replaces the whole source's segment ids, see
    /// [`crate::downloader::supervisor::Supervisor::cmd_rescan_schedule_event`]).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn event_properties_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let popup_state = self.event_props_popups[idx].clone();
        let sid = popup_state.lock().unwrap().segment_id;
        let Some(s) = self.schedule_all.iter().find(|s| s.segment_id == sid).cloned() else {
            return true;
        };

        let rescanning = self.background_tasks.iter().any(|t| {
            matches!(t.kind, crate::events::BackgroundTaskKind::OcrRescan(id) if id == sid)
        });
        let hidden = self.schedule_hidden_segments.contains(&sid);

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("schedule_event_props_vp", sid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Schedule event properties — {}", s.title))
                .with_inner_size([460.0, 460.0]),
            popup_state.clone(),
            shared,
            move |ctx, popup, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    popup.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.strong("Event");
                        egui::Grid::new("evp_event")
                            .num_columns(2)
                            .striped(true)
                            .min_col_width(90.0)
                            .show(ui, |ui| {
                                ui.label("Channel");
                                ui.label(&s.channel_name);
                                ui.end_row();
                                ui.label("Title");
                                ui.add(egui::Label::new(&s.title).wrap_mode(egui::TextWrapMode::Wrap));
                                ui.end_row();
                                if !s.category.is_empty() {
                                    ui.label("Category");
                                    ui.label(&s.category);
                                    ui.end_row();
                                }
                                if !s.collab.is_empty() {
                                    ui.label("With").on_hover_text("Collaborator(s)");
                                    ui.label(&s.collab);
                                    ui.end_row();
                                }
                                ui.label("Start");
                                ui.label(fmt_datetime_short(s.start_time));
                                ui.end_row();
                                if let Some(end) = s.end_time {
                                    ui.label("End");
                                    ui.label(fmt_datetime_short(end));
                                    ui.end_row();
                                }
                                ui.label("Hidden");
                                ui.label(if hidden { "Yes" } else { "No" });
                                ui.end_row();
                            });

                        ui.add_space(8.0);
                        ui.strong("Source");
                        egui::Grid::new("evp_source")
                            .num_columns(2)
                            .striped(true)
                            .min_col_width(90.0)
                            .show(ui, |ui| {
                                ui.label("Source");
                                let label = if s.is_manual() {
                                    "Manually edited".to_string()
                                } else {
                                    crate::schedule_source::ScheduleSourceKind::from_id(&s.source)
                                        .map(|k| k.label().to_string())
                                        .unwrap_or_else(|| s.source.clone())
                                };
                                ui.label(label);
                                ui.end_row();
                                if !s.ocr_model.is_empty() {
                                    ui.label("Scanned with").on_hover_text(
                                        "The CLI model whose output produced this event.",
                                    );
                                    let conf = if s.ocr_confidence.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" (confidence: {})", s.ocr_confidence)
                                    };
                                    let color = if s.ocr_confidence == "low" {
                                        egui::Color32::from_rgb(220, 160, 30)
                                    } else {
                                        ui.visuals().text_color()
                                    };
                                    ui.colored_label(color, format!("{}{conf}", s.ocr_model));
                                    ui.end_row();
                                }
                            });

                        ui.add_space(8.0);
                        ui.strong("Rescan").on_hover_text(
                            "Force a fresh OCR pass over this event's source image with a \
                             different model/effort. Because a source refresh replaces every \
                             upcoming event from that image at once, this window closes \
                             afterward — check the calendar for the updated result.",
                        );
                        if s.ocr_model.is_empty() {
                            ui.weak("Not an OCR-scanned event — nothing to rescan.");
                        } else {
                            egui::Grid::new("evp_rescan")
                                .num_columns(2)
                                .spacing([12.0, 8.0])
                                .show(ui, |ui| {
                                    ui.label("Model");
                                    egui::ComboBox::from_id_salt(("evp_model_combo", sid))
                                        .selected_text(&popup.rescan_model)
                                        .width(120.0)
                                        .show_ui(ui, |ui| {
                                            for m in ["haiku", "sonnet", "opus"] {
                                                ui.selectable_value(&mut popup.rescan_model, m.to_string(), m);
                                            }
                                        })
                                        .response
                                        .on_hover_text(
                                            "CLI model to re-scan this event's source image with.",
                                        );
                                    ui.end_row();
                                    ui.label("Effort");
                                    egui::ComboBox::from_id_salt(("evp_effort_combo", sid))
                                        .selected_text(if popup.rescan_effort.is_empty() {
                                            "default"
                                        } else {
                                            &popup.rescan_effort
                                        })
                                        .width(120.0)
                                        .show_ui(ui, |ui| {
                                            for level in ["", "low", "medium", "high", "xhigh", "max"] {
                                                let label = if level.is_empty() { "default" } else { level };
                                                ui.selectable_value(&mut popup.rescan_effort, level.to_string(), label);
                                            }
                                        })
                                        .response
                                        .on_hover_text(
                                            "Effort level passed as --effort to the CLI. 'default' \
                                             omits the flag entirely.",
                                        );
                                    ui.end_row();
                                });
                            let btn = ui.add_enabled(
                                !rescanning,
                                egui::Button::new(if rescanning { "Rescanning…" } else { "🔄 Rescan this event" }),
                            );
                            let btn = if rescanning {
                                btn.on_hover_text("A rescan for this event is already running.")
                            } else {
                                btn.on_hover_text(
                                    "Re-run OCR on this event's source image now, with the model/effort \
                                     above, and apply the result.",
                                )
                            };
                            if btn.clicked() {
                                popup.rescan_clicked = true;
                            }
                        }
                    });
                });
            },
        );

        let (model, effort, rescan_clicked, closed) = {
            let mut p = popup_state.lock().unwrap();
            let result = (p.rescan_model.clone(), p.rescan_effort.clone(), p.rescan_clicked, p.closed);
            p.rescan_clicked = false;
            result
        };
        if rescan_clicked {
            self.core.manual(ManualCommand::RescanScheduleEvent {
                segment_id: sid,
                model,
                effort,
            });
        }
        closed
    }

    /// The "Edit schedule item" dialog (None = closed). Lets the user correct an
    /// occurrence's time/title/category or delete it; saving marks the row
    /// Rename-recording dialog: shows a text-edit for the new file stem, a live
    /// preview of the final filename, and OK / Cancel buttons.
    #[allow(deprecated)]
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn rename_dialog_window(&mut self, ctx: &egui::Context) {
        if !self.show_rename_dialog {
            self.rename_dialog_popup = None;
            return;
        }
        let rec_id = match self.rename_rec_id {
            Some(id) => id,
            None => {
                self.show_rename_dialog = false;
                return;
            }
        };

        if self.rename_dialog_popup.is_none() {
            self.rename_dialog_popup = Some(Arc::new(Mutex::new(RenameDialogState {
                rec_id,
                draft: self.rename_draft.clone(),
                preview: self.rename_preview.clone(),
                do_rename: false,
                closed: false,
            })));
        }
        let popup_state = self.rename_dialog_popup.clone().unwrap();
        {
            let mut s = popup_state.lock().unwrap();
            s.rec_id = rec_id;
            s.preview = self.rename_preview.clone();
        }

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("rename_recording_vp"),
            egui::ViewportBuilder::default()
                .with_title("Rename recording")
                .with_inner_size([500.0, 160.0])
                .with_resizable(false),
            popup_state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.label("New file name (without extension):");
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut s.draft)
                            .desired_width(ui.available_width())
                            .hint_text("new stem"),
                    );
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(format!("→ {}.mkv", s.preview))
                        .color(egui::Color32::from_rgb(0xa0, 0xa0, 0xa0)));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("✔  OK").clicked() {
                            s.do_rename = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            s.closed = true;
                        }
                    });
                });
            },
        );

        let (new_draft, do_rename, closed) = {
            let mut s = popup_state.lock().unwrap();
            let result = (s.draft.clone(), s.do_rename, s.closed);
            s.do_rename = false;
            s.closed = false;
            result
        };

        // Update draft and recompute preview outside the closure (borrow is released).
        if new_draft != self.rename_draft {
            self.rename_draft = new_draft.clone();
            self.rename_preview = crate::downloader::preview_filename(
                &new_draft,
                &crate::downloader::TemplateVars {
                    name: &new_draft, title: &new_draft, channel: "",
                    video_id: "", quality: "", resolution: "", height: "",
                    width: "", fps: "", vcodec: "", acodec: "",
                    take: "", games: "", tool: "", mode: "", platform: "",
                    secs: 0, went_live: 0, style: None,
                },
            );
        }

        if do_rename {
            let stem = self.rename_preview.clone();
            self.core.manual(ManualCommand::RenameRecording { rec_id, new_stem: stem });
            self.show_rename_dialog = false;
            self.rename_rec_id = None;
            self.rename_dialog_popup = None;
        } else if closed {
            self.show_rename_dialog = false;
            self.rename_rec_id = None;
            self.rename_dialog_popup = None;
        }
    }

    /// "🚂 Mark hype train" dialog: records a train the automatic capture
    /// missed (channel + start time + optional duration), then retro-scores
    /// the stored contributions right before the start so the inference can
    /// be loosened toward what it should have caught (the auto-tune's
    /// manual-label path).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn hype_mark_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.show_hype_mark.clone() else {
            return;
        };
        let mut channels: Vec<(i64, String)> =
            self.channels.iter().map(|c| (c.id, c.name.clone())).collect();
        channels.sort_by_key(|(_, n)| n.to_lowercase());
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("hype_mark_vp"),
            egui::ViewportBuilder::default()
                .with_title("Mark hype train")
                .with_inner_size([460.0, 240.0])
                .with_resizable(false),
            state.clone(),
            shared,
            move |ctx, draft, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    draft.closed = true;
                }
                // Absolute local time wins when parseable; else "minutes ago".
                let parse_abs = |s: &str| -> Option<i64> {
                    let dt = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M").ok()?;
                    use chrono::offset::LocalResult;
                    match dt.and_local_timezone(chrono::Local) {
                        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t.timestamp()),
                        LocalResult::None => None,
                    }
                };
                let abs_ts = parse_abs(&draft.abs);
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        "Record a hype train that ran without being captured. The \
                         contributions stored just before the start teach the \
                         inference what to catch next time.",
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Channel:");
                        let sel = channels
                            .iter()
                            .find(|(id, _)| *id == draft.channel)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| "— pick —".into());
                        egui::ComboBox::from_id_salt("hype_mark_channel")
                            .selected_text(sel)
                            .show_ui(ui, |ui| {
                                for (cid, name) in &channels {
                                    if ui.selectable_label(draft.channel == *cid, name).clicked() {
                                        draft.channel = *cid;
                                    }
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Started:");
                        ui.add(
                            egui::DragValue::new(&mut draft.mins_ago).range(0..=1440).suffix(" min ago"),
                        )
                        .on_hover_text(
                            "How long ago the train kicked off — used when no \
                             absolute time is given below.",
                        );
                        ui.label("or at");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut draft.abs)
                                .desired_width(130.0)
                                .hint_text("YYYY-MM-DD HH:MM"),
                        );
                        resp.on_hover_text(
                            "Absolute local start time — wins over 'minutes ago' \
                             when filled in and parseable.",
                        );
                        if !draft.abs.trim().is_empty() && abs_ts.is_none() {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                                "⚠ format",
                            );
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Duration:");
                        ui.add(egui::DragValue::new(&mut draft.dur).range(0..=240).suffix(" min"))
                            .on_hover_text(
                                "Optional — how long the train ran (0 = unknown). \
                                 Recorded in the event's detail only.",
                            );
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = ui.add_enabled(draft.channel != 0, egui::Button::new("✔  Mark"));
                        if ok
                            .on_hover_text(
                                "Insert the train into the channel's event history \
                                 and (with auto-tune on) loosen the inference if it \
                                 should have fired on the stored contributions.",
                            )
                            .clicked()
                        {
                            draft.do_mark = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            draft.closed = true;
                        }
                    });
                });
            },
        );

        let (closed, do_mark, channel, mins_ago, abs, dur) = {
            let d = state.lock().unwrap();
            (d.closed, d.do_mark, d.channel, d.mins_ago, d.abs.clone(), d.dur)
        };
        // Remembered across the next open, same as the pre-migration code's
        // unconditional per-frame sync back to `self`.
        self.hype_mark_mins_ago = mins_ago;
        self.hype_mark_dur = dur;

        if do_mark && channel != 0 {
            let parse_abs = |s: &str| -> Option<i64> {
                let dt = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M").ok()?;
                use chrono::offset::LocalResult;
                match dt.and_local_timezone(chrono::Local) {
                    LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t.timestamp()),
                    LocalResult::None => None,
                }
            };
            let start = parse_abs(&abs).unwrap_or_else(|| now_unix() - mins_ago.max(0) * 60);
            self.record_manual_hype_train(channel, start, dur);
            self.show_hype_mark = None;
        } else if closed {
            self.show_hype_mark = None;
        }
    }

    /// Insert a manually-marked hype train for `channel_id` starting at
    /// `start` and feed the auto-tune from the contributions stored in the
    /// window before it (mirrors what a GQL confirmation does).
    fn record_manual_hype_train(&mut self, channel_id: i64, start: i64, dur_min: i64) {
        let store = &self.core.store;
        // The train belongs to the channel's Twitch monitor (trains are
        // Twitch-only); fall back to any monitor so the mark never fails.
        let rows = store.list_monitors_with_channels().unwrap_or_default();
        let monitor_id = rows
            .iter()
            .filter(|r| r.channel.id == channel_id)
            .find(|r| r.monitor.platform() == crate::models::Platform::Twitch)
            .or_else(|| rows.iter().find(|r| r.channel.id == channel_id))
            .map(|r| r.monitor.id);
        let Some(monitor_id) = monitor_id else {
            self.status = "Mark failed: channel has no instances".into();
            return;
        };
        let tuning = crate::hype::load_tuning(store);
        let win = tuning.window_secs.max(1);
        let observed =
            crate::hype::observed_burst(store, monitor_id, start - win, start + 60, &tuning);
        let mut detail = format!(
            "marked manually — {} pts / {} contributions / {} chatters on record before kickoff",
            observed.0, observed.1, observed.2
        );
        if dur_min > 0 {
            detail.push_str(&format!(" · ran ~{dur_min} min"));
        }
        let _ = store.record_stream_event(
            monitor_id,
            start,
            "",
            "hype_train",
            "",
            "",
            observed.0,
            &format!("manual:{start}"),
            &detail,
        );
        // Same rule as a GQL confirmation: an inferred row near the start
        // means the inference caught it (superseded, no tuning); otherwise
        // stored contributions become a loosening sample.
        match store.delete_inferred_hype_near(monitor_id, start, win) {
            Ok(n) if n > 0 => {}
            _ => {
                if observed.1 > 0 {
                    crate::hype::loosen_for_missed(store, observed, "a manual mark");
                    self.hype_tuning = crate::hype::load_tuning(store);
                }
            }
        }
        self.chstats_data = None;
        if let Some(p) = &self.viewer_stats_popup {
            p.lock().unwrap().data = None;
        }
        self.status = "Hype train marked".into();
    }

    /// "⚙ Sensitivity" per-channel hype-train override editor (opened from
    /// the Channel Stats controls row; `hype_override_for` = channel id).
    #[allow(deprecated)]
    pub(super) fn hype_override_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.hype_override_for.clone() else { return };

        // One row per gate: a "use global" checkbox + a DragValue that only
        // exists while overridden.
        fn gate_row(
            ui: &mut egui::Ui,
            label: &str,
            hover: &str,
            slot: &mut Option<i64>,
            global: i64,
            range: std::ops::RangeInclusive<i64>,
            suffix: &str,
        ) {
            ui.label(label).on_hover_text(hover.to_string());
            let mut use_global = slot.is_none();
            if ui
                .checkbox(&mut use_global, "use global")
                .on_hover_text(format!("Global value: {global}{suffix}"))
                .changed()
            {
                *slot = if use_global { None } else { Some(global) };
            }
            match slot {
                Some(v) => {
                    ui.add(egui::DragValue::new(v).range(range).suffix(suffix))
                        .on_hover_text("This channel's value — the global tuning is untouched");
                }
                None => {
                    ui.weak(format!("{global}{suffix}"));
                }
            }
            ui.end_row();
        }

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("hype_override_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("{} — hype sensitivity", state.lock().unwrap().name))
                .with_inner_size([420.0, 210.0])
                .with_resizable(false),
            state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                let global = s.global.clone();
                let draft = &mut s.draft;
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        "Override the burst thresholds for this channel only — a \
                         small channel's trains ride on far fewer contributions \
                         than a big one's. Weights and window stay global \
                         (Settings → Maintenance → Hype trains).",
                    );
                    ui.add_space(6.0);
                    egui::Grid::new("hype_override_grid")
                        .num_columns(3)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            gate_row(
                                ui,
                                "Min points",
                                "Summed contribution points needed in the window \
                                 (0 = points gate off for this channel).",
                                &mut draft.min_points,
                                global.min_points,
                                0..=10_000,
                                " pts",
                            );
                            gate_row(
                                ui,
                                "Min contributions",
                                "Separate sub/gift/bits/Hype Chat events needed.",
                                &mut draft.min_events,
                                global.min_events,
                                1..=20,
                                "",
                            );
                            gate_row(
                                ui,
                                "Min chatters",
                                "Distinct contributors needed.",
                                &mut draft.min_actors,
                                global.min_actors,
                                1..=10,
                                "",
                            );
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button("✔  Save")
                            .on_hover_text(
                                "Store the override (checked rows keep following \
                                 the global tuning). Applies to running \
                                 recordings within 5 minutes.",
                            )
                            .clicked()
                        {
                            s.do_save = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            s.closed = true;
                        }
                    });
                });
            },
        );

        let (do_save, closed, channel_id, draft) = {
            let s = state.lock().unwrap();
            (s.do_save, s.closed, s.channel_id, s.draft)
        };
        if do_save {
            crate::hype::save_override(&self.core.store, channel_id, draft);
            self.hype_override_for = None;
        } else if closed {
            self.hype_override_for = None;
        }
    }

    /// Dialog for naming and saving a custom filename-template preset.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn save_preset_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.save_preset_dialog.clone() else {
            return;
        };
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("save_preset_vp"),
            egui::ViewportBuilder::default()
                .with_title("Save as preset")
                .with_inner_size([340.0, 120.0])
                .with_resizable(false),
            state.clone(),
            shared,
            |ctx, d, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    d.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Preset name:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut d.name)
                            .hint_text("e.g. My favourite format")
                            .desired_width(310.0),
                    );
                    if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                        d.do_save = true;
                    }
                    if !d.error.is_empty() {
                        ui.colored_label(HL_ERROR_TEXT, &d.error);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let can_save = !d.name.trim().is_empty();
                        if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                            d.do_save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            d.closed = true;
                        }
                    });
                });
            },
        );

        let (do_save, closed) = {
            let d = state.lock().unwrap();
            (d.do_save, d.closed)
        };
        if do_save {
            let (name, template) = {
                let d = state.lock().unwrap();
                (d.name.trim().to_string(), d.template.clone())
            };
            match self.core.store.save_filename_preset(&name, &template) {
                Ok(_) => {
                    self.custom_presets =
                        self.core.store.get_filename_presets().unwrap_or_default();
                    self.status = format!("Preset \"{name}\" saved.");
                    self.save_preset_dialog = None;
                }
                Err(e) => {
                    let mut d = state.lock().unwrap();
                    d.error = format!("Error saving: {e:#}");
                    d.do_save = false;
                }
            }
        } else if closed {
            self.save_preset_dialog = None;
        }
    }
    /// The "⇕ Reorder columns…" window: edits a working COPY of one table's
    /// entries (checkbox + ▲/▼, reorder enabled unlike the inline header
    /// popup) and only commits — one save, one table reset — on Apply.
    /// Closing the window (✖/native close) discards the draft, same as
    /// Cancel.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn reorder_columns_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.reorder_columns.clone() else {
            return;
        };
        let table = state.lock().unwrap().table;
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of(("reorder_columns_vp", table.key())),
            egui::ViewportBuilder::default()
                .with_title(format!("Reorder columns — {}", table_display_name(table)))
                .with_inner_size([320.0, 480.0]),
            state.clone(),
            shared,
            move |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.cancel = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Move columns into the order you want, then Apply.");
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        grid_columns::column_chooser_editor(
                            ui, &mut s.draft, columns_for(table), |id| id == "actions", true,
                        );
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("✔  Apply").clicked() {
                            s.apply = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            s.cancel = true;
                        }
                    });
                });
            },
        );

        let (apply, cancel) = {
            let s = state.lock().unwrap();
            (s.apply, s.cancel)
        };
        if apply {
            let entries = state.lock().unwrap().draft.clone();
            self.apply_reordered_columns(table, entries);
        }
        if apply || cancel {
            self.reorder_columns = None;
        }
    }

    /// Write a "⇕ Reorder columns…" draft back into the live grid state for
    /// `table` and persist it — the ONE reset this whole flow causes, no
    /// matter how many intermediate moves the user made in the draft window.
    pub(super) fn apply_reordered_columns(&mut self, table: GridTableId, entries: Vec<ColumnEntry>) {
        let target = match table {
            GridTableId::Streams => &mut self.streams_grid.entries,
            GridTableId::Videos => &mut self.videos_grid.entries,
            GridTableId::BgActive => &mut self.bg_active_grid.entries,
            GridTableId::BgRecent => &mut self.bg_recent_grid.entries,
            GridTableId::Processes => &mut self.processes_grid.entries,
            GridTableId::Issues => &mut self.issues_grid.entries,
        };
        *target = entries;
        grid_columns::save_columns(&self.core.store, table, target);
    }
    /// its PID, status, and uptime, plus per-process Stop (graceful) / Kill (force)
    /// and reveal-log/folder actions. Doubles as a live list of spawned processes.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn processes_window(&mut self, ctx: &egui::Context) {
        use std::time::{Duration, Instant};
        // Drain a completed background load first.
        if let Some(rx) = &self.processes_load {
            match rx.try_recv() {
                Ok(procs) => {
                    debug!(count = procs.len(), "list-processes result installed");
                    self.processes = procs;
                    self.processes_refreshed = Some(Instant::now());
                    self.processes_load = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    warn!("list-processes thread disconnected without sending");
                    self.processes_load = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // Throttle the snapshot (each row does a pid_alive + a couple DB reads).
        // Spawn off the UI thread so the store-mutex wait can't freeze the UI.
        // Refreshed on this same throttle even while the window is closed —
        // just on a much longer interval — so the top-bar count badge stays
        // live without paying the open-window 1.5s cadence for it (mirrors
        // the Warnings/notifications bell badges' open/closed throttle).
        let interval =
            if self.show_processes { Duration::from_millis(1500) } else { Duration::from_secs(5) };
        let stale = self.processes_refreshed.map(|t| t.elapsed() >= interval).unwrap_or(true);
        if stale && self.processes_load.is_none() {
            let core = self.core.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            debug!("spawning list-processes thread");
            std::thread::Builder::new()
                .name("list-processes".into())
                .spawn(move || {
                    let t = std::time::Instant::now();
                    let procs = core.list_processes();
                    debug!(elapsed_ms = t.elapsed().as_millis(), count = procs.len(), "list-processes done");
                    let _ = tx.send(procs);
                })
                .ok();
            self.processes_load = Some(rx);
            if self.show_processes {
                // Keep repainting until the load arrives.
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        } else if self.show_processes {
            ctx.request_repaint_after(Duration::from_millis(1500));
        }

        if !self.show_processes {
            self.processes_popup = None;
            return;
        }

        // Ensure a popup instance exists, seeded from the persisted column
        // draft (remembered across a close/reopen, same as before).
        if self.processes_popup.is_none() {
            self.processes_popup = Some(Arc::new(Mutex::new(ProcessesPopupState {
                rows: Vec::new(),
                entries: self.processes_grid.entries.clone(),
                last_order: None,
                widths: grid_columns::WidthMemory::default(),
                reorder_columns: None,
                act: None,
                closed: false,
            })));
        }
        let popup_state = self.processes_popup.clone().unwrap();
        // Cheap — already just an in-memory Vec; the actual list-processes
        // work happened off-thread above, on its own throttle.
        popup_state.lock().unwrap().rows = self.processes.clone();

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("processes_vp"),
            egui::ViewportBuilder::default()
                .with_title("🖥 Processes")
                .with_inner_size([800.0, 440.0]),
            popup_state.clone(),
            shared,
            |ctx, s, _shared| {
                use crate::models::{ContentType, DetachedKind};
                use egui_extras::{Column, TableBuilder};
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                let now = now_unix();
                // Latest per-process I/O sample, keyed by PID — O(1) fetch (unlike
                // `iomon::history()`, which clones the whole ~30 min ring). The
                // sampler ticks every 1s regardless of this window, so this is
                // always fresh to within a second.
                let io_by_pid: std::collections::HashMap<u32, crate::iomon::ProcSample> =
                    crate::iomon::latest()
                        .map(|smp| smp.procs.into_iter().map(|p| (p.pid, p)).collect())
                        .unwrap_or_default();
                let processes_order =
                    grid_columns::effective_order(&PROCESSES_COLUMNS, &s.entries, |_| true);
                // Mirrors `GridState::note_order` — can't call it directly
                // since it needs `&mut self.processes_grid`, unreachable from
                // in here; `last_order`/`widths` live on `s` instead (both
                // deliberately session-only anyway, same as `GridState`'s).
                let processes_reset =
                    s.last_order.as_deref().is_some_and(|prev| prev != processes_order);
                s.last_order = Some(processes_order.clone());
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} spawned process(es)", s.rows.len()));
                        // Child-viewport instrumentation: proves registration +
                        // highlight painting inside a deferred viewport.
                        if ui
                            .button("⟳ Refresh")
                            .inspect("Processes: Refresh button", &[])
                            .clicked()
                        {
                            s.act = Some(ProcessesAct::Refresh);
                        }
                        ui.weak("Stop = graceful (file finalized) · Kill = force-terminate the tree");
                    });
                    ui.separator();
                    if s.rows.is_empty() {
                        ui.weak("No download tool processes are running.");
                        return;
                    }
                    let mut tb = TableBuilder::new(ui)
                        .id_salt(GridTableId::Processes.key())
                        .striped(true)
                        .resizable(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                    if processes_reset {
                        tb.reset();
                    }
                    for &i in &processes_order {
                        let c = &PROCESSES_COLUMNS[i];
                        // A hide/show/reorder-forced reset restores each column to
                        // its last remembered width instead of snapping back to the
                        // declared default — see `WidthMemory` (`grid_columns.rs`).
                        let seed = s.widths.get(c.id);
                        let col = if c.stretch {
                            Column::remainder().at_least(c.min_width).clip(true)
                        } else if processes_reset && let Some(w) = seed {
                            Column::auto_with_initial_suggestion(w).at_least(c.min_width)
                        } else {
                            Column::auto().at_least(c.min_width)
                        };
                        tb = tb.column(col);
                    }
                    tb.header(20.0, |mut h| {
                        for &i in &processes_order {
                            let c = &PROCESSES_COLUMNS[i];
                            let (rect, _) = h.col(|ui| {
                                if grid_header_cell_plain(ui, GridTableId::Processes, c, &mut s.entries, &PROCESSES_COLUMNS) {
                                    s.reorder_columns = Some(Arc::new(Mutex::new(ReorderColumnsState {
                                        table: GridTableId::Processes,
                                        draft: s.entries.clone(),
                                        apply: false,
                                        cancel: false,
                                    })));
                                }
                            });
                            s.widths.note(c.id, rect.width());
                        }
                    })
                    .body(|mut body| {
                        for p in &s.rows {
                            body.row(22.0, |mut row| {
                                for &ci in &processes_order {
                                    row.col(|ui| match PROCESSES_COLUMNS[ci].id {
                                        "pid" => { ui.monospace(p.pid.to_string()); }
                                        "type" => {
                                            // Map the process role to a content-type icon + label.
                                            // A live capture is "🎥 video"; its DASH companion leg
                                            // gets a "· dash" suffix. An on-demand download is the
                                            // "📼 VOD" so the two video kinds stay distinguishable.
                                            // A restart-survival ffmpeg post-processing job (from a
                                            // separate registry entirely — see `ProcInfo::ffmpeg_kind`)
                                            // takes priority over `p.kind`, which is a meaningless
                                            // placeholder for these rows.
                                            let t = if let Some(fk) = p.ffmpeg_kind {
                                                fk.label().to_string()
                                            } else {
                                                match p.kind {
                                                    DetachedKind::Recording => {
                                                        let base = ContentType::Video.tag();
                                                        if p.secondary {
                                                            format!("{base} · dash")
                                                        } else {
                                                            base
                                                        }
                                                    }
                                                    DetachedKind::Video => ContentType::Vod.tag(),
                                                    DetachedKind::Chat => ContentType::Chat.tag(),
                                                }
                                            };
                                            ui.label(t);
                                        }
                                        "name" => {
                                            ui.label(&p.name).on_hover_text(&p.capture_path);
                                        }
                                        "tool" => { ui.label(&p.tool); }
                                        "drive" => {
                                            match crate::iomon::drive_letter(std::path::Path::new(&p.capture_path)) {
                                                Some(d) => { ui.monospace(format!("{d}:")); }
                                                None => { ui.weak("—"); }
                                            }
                                        }
                                        "io" => {
                                            match io_by_pid.get(&p.pid) {
                                                Some(smp) => {
                                                    let tree = if smp.tree.is_empty() {
                                                        smp.tool.clone()
                                                    } else {
                                                        smp.tree.clone()
                                                    };
                                                    // Fixed-width monospace so the column doesn't
                                                    // visibly resize every refresh as the rate
                                                    // crosses digit-count boundaries (e.g. "0 B/s"
                                                    // vs "1.0 MB/s").
                                                    ui.monospace(format!(
                                                        "↓{:>8}/s ↑{:>8}/s",
                                                        fmt_bytes(smp.read_bps as i64),
                                                        fmt_bytes(smp.write_bps as i64),
                                                    ))
                                                    .on_hover_text(format!(
                                                        "{tree}\nTotal since start: ↓{} read, ↑{} written\
                                                         {}",
                                                        fmt_bytes(smp.total_read as i64),
                                                        fmt_bytes(smp.total_write as i64),
                                                        if smp.descendants > 0 {
                                                            format!(
                                                                "\n{} live descendant process(es) rolled in",
                                                                smp.descendants
                                                            )
                                                        } else {
                                                            String::new()
                                                        }
                                                    ));
                                                }
                                                None => {
                                                    ui.weak("—").on_hover_text(
                                                        "No I/O sample yet for this PID (the sampler \
                                                         ticks once a second).",
                                                    );
                                                }
                                            }
                                        }
                                        "progress" => {
                                            // Only populated for a re-attached ffmpeg job (a fresh
                                            // in-session spawn's progress is already visible on its
                                            // Background-tab row instead) — a coarse, size-based
                                            // signal sampled every ~15s (see
                                            // `downloader::ffmpeg_job::poll_size_progress`).
                                            match &p.progress {
                                                Some((Some(frac), info)) => {
                                                    ui.add(
                                                        egui::ProgressBar::new(*frac)
                                                            .text(format!("{:.0}%", frac * 100.0))
                                                            .desired_width(80.0),
                                                    )
                                                    .on_hover_text(info);
                                                }
                                                Some((None, info)) => {
                                                    ui.label(info);
                                                }
                                                None => {
                                                    ui.weak("—");
                                                }
                                            }
                                        }
                                        "status" => {
                                            if p.reattached {
                                                ui.colored_label(
                                                    egui::Color32::from_rgb(0x6c, 0xb0, 0xe0),
                                                    "⛓ re-attached",
                                                )
                                                .on_hover_text(format!(
                                                    "running under a prior build: {}",
                                                    p.spawn_build
                                                ));
                                            } else {
                                                ui.colored_label(
                                                    egui::Color32::from_rgb(0x6c, 0xe0, 0x8c),
                                                    "● running",
                                                );
                                            }
                                        }
                                        "uptime" => {
                                            ui.label(fmt_duration_secs((now - p.started_at).max(0)));
                                        }
                                        "actions" => {
                                            if ui
                                                .small_button("Stop")
                                                .on_hover_text(
                                                    "Graceful: stop the tool and let the app finalize \
                                                     (remux + mark the take stopped).",
                                                )
                                                .clicked()
                                            {
                                                s.act = Some(ProcessesAct::Stop(p.pid));
                                            }
                                            if ui
                                                .small_button("Kill")
                                                .on_hover_text(
                                                    "Force-terminate the whole process tree now — the \
                                                     capture may be left un-finalized.",
                                                )
                                                .clicked()
                                            {
                                                s.act = Some(ProcessesAct::Kill(p.pid));
                                            }
                                            // Some re-attached ffmpeg jobs (chapters/thumbnail embed
                                            // etc. from before this feature tracked a real progress
                                            // file) have no log path at all — opening an empty path
                                            // via `explorer.exe` just pops a "This PC" window, which
                                            // reads as broken. Disable instead.
                                            let has_log = !p.log_path.is_empty();
                                            let log_btn = ui.add_enabled(has_log, egui::Button::new("Log").small());
                                            let log_btn = if has_log {
                                                log_btn.on_hover_text(&p.log_path)
                                            } else {
                                                log_btn.on_disabled_hover_text("No log file for this job")
                                            };
                                            if log_btn.clicked() {
                                                s.act = Some(ProcessesAct::RevealLog(p.pid));
                                            }
                                            if ui.small_button("Folder").clicked() {
                                                s.act = Some(ProcessesAct::RevealDir(p.pid));
                                            }
                                        }
                                        "filename" => {
                                            ui.label(&p.filename).on_hover_text(&p.capture_path);
                                        }
                                        _ => {}
                                    });
                                }
                            });
                        }
                    });
                });
            },
        );

        let (entries, reorder_columns, closed, act) = {
            let mut s = popup_state.lock().unwrap();
            (s.entries.clone(), s.reorder_columns.take(), s.closed, s.act.take())
        };
        if entries != self.processes_grid.entries {
            self.processes_grid.entries = entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Processes, &self.processes_grid.entries);
        }
        if let Some(rc) = reorder_columns {
            self.reorder_columns = Some(rc);
        }

        if closed {
            self.show_processes = false;
            self.processes_popup = None;
        }
        match act {
            Some(ProcessesAct::Refresh) => self.processes_refreshed = None,
            Some(ProcessesAct::Stop(pid)) => {
                if let Some(p) = self.processes.iter().find(|p| p.pid == pid) {
                    self.core.stop_process(p);
                    self.status = format!("Stopping pid {} ({})…", p.pid, p.name);
                    self.processes_refreshed = None;
                }
            }
            Some(ProcessesAct::Kill(pid)) => {
                if let Some(p) = self.processes.iter().find(|p| p.pid == pid) {
                    self.core.force_kill(p.pid, &p.job_name);
                    self.status = format!("Killed pid {} ({}).", p.pid, p.name);
                    self.processes_refreshed = None;
                }
            }
            Some(ProcessesAct::RevealLog(pid)) => {
                if let Some(p) = self.processes.iter().find(|p| p.pid == pid) {
                    crate::platform::open_path(std::path::Path::new(&p.log_path));
                }
            }
            Some(ProcessesAct::RevealDir(pid)) => {
                if let Some(p) = self.processes.iter().find(|p| p.pid == pid) {
                    if let Some(dir) = std::path::Path::new(&p.capture_path).parent() {
                        crate::platform::open_path(dir);
                    }
                }
            }
            None => {}
        }
    }
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn form_window(&mut self, ctx: &egui::Context) {
        let Some(form_arc) = self.form.clone() else {
            return;
        };
        // Re-resolved every call — the deferred closure can't reach `self` to
        // pick up a live Settings/monitor-defaults/preset-list change itself.
        {
            let mut f = form_arc.lock().unwrap();
            f.monitor_defaults = self.monitor_defaults.clone();
            f.default_output_dir = self.settings.default_output_dir.clone();
            f.custom_presets = self.custom_presets.clone();
        }

        let f = form_arc.lock().unwrap();
        let title = if f.monitor_id.is_some() {
            "Edit instance"
        } else if f.channel_id.is_some() {
            "Add instance"
        } else {
            "Add stream (new channel)"
        };
        let title = title.to_string();
        drop(f);

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("monitor_form_vp"),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([820.0, 760.0]),
            form_arc.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::TopBottomPanel::bottom("monitor_form_bottom_bar").show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            s.do_save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            s.closed = true;
                        }
                    });
                    ui.add_space(4.0);
                });
                egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let platform = Platform::detect(&s.url);
                // When the URL's platform changes, re-apply that platform's
                // defaults (tool, detection, container, quality, poll interval,
                // filename template, output dir). User overrides afterwards stick.
                if s.last_platform != Some(platform) {
                    let md = &s.monitor_defaults;
                    s.tool = md.resolve_tool(platform);
                    s.detection_method = md.resolve_detection(platform);
                    s.container = md.resolve_container(platform);
                    s.quality = md.resolve_quality(platform);
                    s.poll_interval_secs = md.resolve_poll_interval(platform);
                    s.filename_template = md.resolve_filename_template(platform);
                    s.last_platform = Some(platform);
                }
                // Output dir depends on the channel name too (via a possible
                // `{name}` token — see `expand_dir_template`), which isn't
                // known yet when a brand-new channel's URL is pasted before
                // its name is typed. Re-resolve on either changing, not just
                // platform, so the folder catches up once the name lands —
                // tracked separately from `last_platform` above since this is
                // the only field with a second trigger.
                if s.output_dir_platform != Some(platform) || s.output_dir_name != s.name {
                    s.output_dir = s.monitor_defaults.resolve_output_dir(
                        platform,
                        &s.name,
                        &s.default_output_dir,
                    );
                    s.output_dir_platform = Some(platform);
                    s.output_dir_name = s.name.clone();
                }
                // The name belongs to the channel container; it's editable only
                // when creating a new channel. For an instance it's the container's
                // (rename via the channel row's ✏). The URL is per-instance and
                // always editable.
                let name_editable = s.channel_id.is_none();

                egui::Grid::new("form_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        let name_resp =
                            ui.add_enabled(name_editable, egui::TextEdit::singleline(&mut s.name));
                        if !name_editable {
                            name_resp.on_hover_text(
                                "The channel name — rename it from the channel row's ✏.",
                            );
                        }
                        ui.end_row();

                        ui.label("URL");
                        ui.add(egui::TextEdit::singleline(&mut s.url).desired_width(320.0))
                            .on_hover_text("This instance's source URL (platform auto-detected).");
                        ui.end_row();

                        ui.label("Platform");
                        ui.label(platform.label());
                        ui.end_row();

                        ui.label("Tool").on_hover_text(s.tool.tooltip());
                        egui::ComboBox::from_id_salt("tool_cb")
                            .selected_text(s.tool.label())
                            .show_ui(ui, |ui| {
                                for t in Tool::ALL {
                                    ui.selectable_value(&mut s.tool, t, t.label())
                                        .on_hover_text(t.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Detection")
                            .on_hover_text(s.detection_method.tooltip());
                        let methods = platform.detection_methods();
                        if !methods.contains(&s.detection_method) {
                            s.detection_method = platform.default_detection();
                        }
                        egui::ComboBox::from_id_salt("method_cb")
                            .selected_text(s.detection_method.label())
                            .show_ui(ui, |ui| {
                                for &dm in methods {
                                    ui.selectable_value(&mut s.detection_method, dm, dm.label())
                                        .on_hover_text(dm.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Poll interval (s)");
                        ui.add(egui::DragValue::new(&mut s.poll_interval_secs).range(5..=86400));
                        ui.end_row();

                        ui.label("Quality");
                        ui.text_edit_singleline(&mut s.quality);
                        ui.end_row();

                        ui.label("Container");
                        egui::ComboBox::from_id_salt("container_cb")
                            .selected_text(s.container.label())
                            .show_ui(ui, |ui| {
                                for c in Container::ALL {
                                    ui.selectable_value(&mut s.container, c, c.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Audio tracks");
                        ui.text_edit_singleline(&mut s.audio_tracks).on_hover_text(
                            "Audio tracks to capture (streamlink --hls-audio-select). \
                             Empty = the tool's default single track; 'all' (or '*') = \
                             every track; or a comma-separated list of language \
                             codes/names. streamlink-only; ffmpeg copy keeps all tracks.",
                        );
                        ui.end_row();

                        ui.label("Subtitle tracks");
                        ui.text_edit_singleline(&mut s.subtitle_tracks).on_hover_text(
                            "Subtitle tracks to capture (yt-dlp --sub-langs, written as \
                             sidecar files next to the recording). Empty = none; 'all' \
                             (or '*') = every subtitle; or a comma-separated list of \
                             language codes. yt-dlp-only; streamlink can't mux subtitles. \
                             Best-effort for live streams.",
                        );
                        ui.end_row();

                    });

                ui.add_space(4.0);
                // Two side-by-side grids instead of one long single-column
                // list — this section alone used to be ~20 rows tall; most of
                // it is short checkboxes/combos that don't need the full
                // dialog width, so splitting them cuts the window's overall
                // height roughly in half instead of relying on the user to
                // resize/scroll through a wall of toggles.
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        egui::Grid::new("form_grid_toggles_left")
                            .num_columns(2)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Log chat");
                                ui.checkbox(&mut s.chat_log, "").on_hover_text(
                                    "Save chat alongside the recording. Twitch: a built-in \
                                     anonymous chat logger writes a .chat.jsonl sidecar. YouTube \
                                     (yt-dlp tool): yt-dlp's live_chat writes a .live_chat.json \
                                     sidecar. Other platforms/tools don't capture chat. \
                                     By default this also applies when the stream ISN'T being \
                                     recorded (Auto-record off) — chat is tiny and unrecoverable \
                                     after the fact; see Settings → Downloads → Chat logging to \
                                     restrict it to actual recordings.",
                                );
                                ui.end_row();

                                ui.label("Fetch thumbnail");
                                ui.checkbox(&mut s.fetch_thumbnail, "").on_hover_text(
                                    "Download the stream thumbnail alongside the recording \
                                     ({stem}.thumbnail.jpg). For yt-dlp, passes --write-thumbnail; \
                                     for Twitch/Kick/YouTube, fetches the URL from detection metadata.",
                                );
                                ui.end_row();

                                ui.label("Thumbnail in notification");
                                ui.add_enabled(
                                    s.fetch_thumbnail,
                                    egui::Checkbox::new(&mut s.thumbnail_in_toast, ""),
                                ).on_hover_text(
                                    "Use the stream thumbnail as the hero image in the \
                                     recording-started notification (instead of the channel's \
                                     static banner). Most useful for YouTube, where each stream \
                                     has a unique thumbnail. Requires \"Fetch thumbnail\" to be on.",
                                );
                                ui.end_row();

                                ui.label("Fetch chat assets");
                                ui.checkbox(&mut s.fetch_chat_assets, "").on_hover_text(
                                    "Download channel icon, offline banner, Twitch badges, and \
                                     emotes (including BTTV, FFZ, 7TV) into channel_assets/ \
                                     alongside recordings. Needed for full offline chat replay. \
                                     Refreshed at most once per 24 hours.",
                                );
                                ui.end_row();

                                ui.label("Capture from start");
                                ui.checkbox(&mut s.capture_from_start, "").on_hover_text(
                                    "yt-dlp --live-from-start / streamlink --hls-live-restart",
                                );
                                ui.end_row();

                                if Platform::detect(&s.url) == Platform::YouTube {
                                    ui.label("Dual capture (SABR + DASH)");
                                    ui.checkbox(&mut s.dual_capture, "").on_hover_text(
                                        "YouTube only: also run a second concurrent DASH capture \
                                         (system yt-dlp, live edge) when wanted formats span both SABR \
                                         and DASH. Produces a second recording in the same take. \
                                         Needs Capture-from-start and a configured SABR build.",
                                    );
                                    ui.end_row();

                                    ui.label("Video codec / quality");
                                    egui::ComboBox::from_id_salt("form_sabr_codec_pref")
                                        .selected_text(s.sabr_codec_pref.label())
                                        .show_ui(ui, |ui| {
                                            for &p in &SabrCodecPref::ALL {
                                                ui.selectable_value(
                                                    &mut s.sabr_codec_pref,
                                                    p,
                                                    p.label(),
                                                );
                                            }
                                        })
                                        .response
                                        .on_hover_text(
                                            "SABR video codec/quality for this instance. Inherit follows \
                                             the global default in Settings. Best-quality/H.264 avoid the \
                                             lower-bitrate VP9/AV1 rendition of the same resolution.",
                                        );
                                    ui.end_row();
                                    if s.sabr_codec_pref == SabrCodecPref::Custom {
                                        ui.label("Custom -S sort");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut s.sabr_codec_custom)
                                                .hint_text("res,fps,vcodec:h264")
                                                .desired_width(180.0),
                                        )
                                        .on_hover_text(
                                            "Raw yt-dlp -S format-sort. Lead with res,fps so \
                                             resolution/fps win and codec/bitrate is only the tiebreak.",
                                        );
                                        ui.end_row();
                                    }
                                }

                                ui.label("Ad-free");
                                ui.checkbox(&mut s.ad_free, "").on_hover_text(
                                    "Mark this instance ad-free for your account (YouTube \
                                     membership/Premium, Twitch Turbo/sub) so captures won't have \
                                     ad-break hard cuts. For Twitch with a connected account, sub \
                                     status is also detected automatically.",
                                );
                                ui.end_row();

                                ui.label("Pin as preferred platform");
                                ui.checkbox(&mut s.primary_pin, "").on_hover_text(
                                    "Always show THIS instance's info on the channel row while it's \
                                     live, even if a sibling instance (another platform) went live \
                                     earlier or the channel/global preference points elsewhere — the \
                                     strongest of the three preference tiers.",
                                );
                                ui.end_row();

                                ui.label("Enabled");
                                ui.checkbox(&mut s.automation_enabled, "")
                                    .on_hover_text(
                                        "Master switch (same as the Enabled column). Off = fully \
                                         dormant: no detection, recording, or asset/about/posts/schedule \
                                         fetch until you act manually (▶ Start, ⟳ Refetch). Independent \
                                         from Auto below.",
                                    );
                                ui.end_row();

                                ui.label("Auto");
                                ui.checkbox(&mut s.enabled, "")
                                    .on_hover_text(
                                        "Auto-record: automatically record to disk when this channel \
                                         goes live (a disk-space control; same as the Auto column). It \
                                         does NOT gate detection, metadata, posts, schedules or assets — \
                                         those run while the channel is Enabled. Recording only starts \
                                         automatically when this is on; otherwise press ▶ yourself, or a \
                                         trigger word matches the live title/game.",
                                    );
                                ui.end_row();
                            });
                    });
                    ui.add_space(24.0);
                    ui.vertical(|ui| {
                        egui::Grid::new("form_grid_toggles_right")
                            .num_columns(2)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Download VOD after end");
                                tristate_combo(ui, "form_vod_download", &mut s.vod_download)
                                    .on_hover_text(
                                        "Download the platform's published VOD after this instance's \
                                         stream ends. Inherit follows the channel, then the global default.",
                                    );
                                ui.end_row();

                                ui.label("Replace with VOD");
                                tristate_combo(ui, "form_vod_replace", &mut s.vod_replace)
                                    .on_hover_text(
                                        "Replace the live recording with the downloaded VOD when it \
                                         succeeds (never for a muted Twitch VOD). Inherit follows the \
                                         channel, then the global default.",
                                    );
                                ui.end_row();

                                ui.label("Fetch new head backfill on new take");
                                tristate_combo(ui, "form_head_backfill_fetch", &mut s.head_backfill_fetch)
                                    .on_hover_text(
                                        "Capture-from-start only: fetch a fresh head backfill for a retake \
                                         (reconnect mid-broadcast), not just the stream's first take. \
                                         Inherit follows the channel, then the global default.",
                                    );
                                ui.end_row();

                                ui.label("Replace old head (if new is undamaged)");
                                tristate_combo(ui, "form_head_backfill_replace", &mut s.head_backfill_replace)
                                    .on_hover_text(
                                        "Once a fresh head backfill passes its integrity checks, delete \
                                         older takes' now-redundant head files for the same stream. Only \
                                         takes effect when fetching a new head is also on. Inherit follows \
                                         the channel, then the global default.",
                                    );
                                ui.end_row();

                                ui.label("After full.mkv join");
                                join_cleanup_combo(ui, "form_join_cleanup", &mut s.join_cleanup)
                                    .on_hover_text(
                                        "Once a verified full.mkv (head + live capture joined) lands for \
                                         a take of this instance: keep both parts (safe, doubles the \
                                         stream's disk cost), delete just the head, or delete both parts \
                                         (the take then points at the full). Deletions follow the \
                                         deletion method below. Inherit follows the channel, then the \
                                         global default (Settings → Downloads → Automatic deletion).",
                                    );
                                ui.end_row();

                                ui.label("Automatic deletes go to");
                                disposal_method_combo(ui, "form_disposal_method", &mut s.disposal_method)
                                    .on_hover_text(
                                        "How automatic media deletions for this instance are executed \
                                         (post-join cleanup, superseded heads, a live capture replaced \
                                         by its VOD): moved to the configured trash folder, sent to the \
                                         Recycle Bin, or deleted permanently. Inherit follows the \
                                         channel, then the global default. Note that \"Trash \
                                         folder\" needs a trash folder configured for the drive in \
                                         Settings → Automatic deletion — without one it quietly \
                                         falls back to the Recycle Bin.",
                                    );
                                ui.end_row();

                                ui.label("Embed chapters");
                                tristate_combo(ui, "form_chapters_enabled", &mut s.chapters_enabled)
                                    .on_hover_text(
                                        "Embed chapter markers (title/category changes, raids, \
                                         recovered/muted gap-splice segments) into finalized recordings \
                                         for this instance. Inherit follows the channel, then the global \
                                         default (Settings → Downloads → Chapters).",
                                    );
                                ui.end_row();

                                ui.label("Title/game coalesce window (s)");
                                ui.add(
                                    egui::TextEdit::singleline(&mut s.chapters_coalesce_secs)
                                        .desired_width(80.0)
                                        .hint_text("Inherit"),
                                )
                                .on_hover_text(
                                    "How many seconds apart a title change and a category/game change \
                                     may land and still merge into one chapter, for this instance. Blank \
                                     inherits the channel, then the global default (Settings → Downloads \
                                     → Chapters).",
                                );
                                ui.end_row();

                                ui.label("Auto-record my raids");
                                tristate_combo(ui, "form_follow_my_raids", &mut s.follow_my_raids)
                                    .on_hover_text(
                                        "When this instance raids out to another Twitch channel, \
                                         auto-record the target (Settings → Follow raid). Inherit \
                                         follows the channel, then the global default there — off unless \
                                         you've turned it on. Independent of \"Auto-play my raids\" below.",
                                    );
                                ui.end_row();

                                ui.label("Auto-play my raids");
                                tristate_combo(ui, "form_follow_my_raids_play", &mut s.follow_my_raids_play)
                                    .on_hover_text(
                                        "When this instance raids out to another Twitch channel, \
                                         auto-open the target at the live edge in your media player — no \
                                         recording, same as the manual \"▷🏃 Follow raid\" button but \
                                         automatic (Settings → Follow raid). Inherit follows the channel, \
                                         then the global default. Independent of \"Auto-record my raids\" \
                                         above.",
                                    );
                                ui.end_row();

                                ui.label("Record me when I'm a raid target");
                                tristate_combo(
                                    ui,
                                    "form_raid_target_record",
                                    &mut s.record_me_as_raid_target,
                                )
                                .on_hover_text(
                                    "Whether Follow raid may auto-RECORD this instance when a followed \
                                     raid lands on it. Always/Never override the \"skip disabled raid \
                                     targets\" default too — set this to Always if you want this \
                                     instance recorded via a raid even while its master switch is off. \
                                     Inherit follows the channel, then the global default (Settings → \
                                     Follow raid).",
                                );
                                ui.end_row();

                                ui.label("Exclude from auto-play");
                                tristate_combo(
                                    ui,
                                    "form_raid_play_exclude",
                                    &mut s.exclude_from_auto_play,
                                )
                                .on_hover_text(
                                    "Set to Always to make sure this instance never gets an auto-opened \
                                     player when a followed raid lands on it. Unlike the record-side \
                                     setting above, auto-play otherwise ignores this instance's disabled \
                                     state entirely — this is the only way to opt it out. Inherit/Never \
                                     both mean \"allowed\".",
                                );
                                ui.end_row();

                                ui.label("Allow deleting files");
                                ui.checkbox(&mut s.allow_delete, "")
                                    .on_hover_text(
                                        "Half of the gate for this instance's take rows' \
                                         \"🗑🔥 Delete file from disk\" action — the OTHER half \
                                         is a per-channel switch (Rename channel), and BOTH \
                                         need the Streams toolbar's own \"Allow deletion\" \
                                         master switch on too. Off by default, on purpose: \
                                         unlike the settings above, this has no inherited \
                                         global default to fall back to.",
                                    );
                                ui.end_row();
                            });
                    });
                });
                ui.add_space(4.0);

                egui::Grid::new("form_grid_footer")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Auth");
                        egui::ComboBox::from_id_salt("auth_cb")
                            .selected_text(s.auth_kind.label())
                            .show_ui(ui, |ui| {
                                for k in AuthKind::ALL {
                                    ui.selectable_value(&mut s.auth_kind, k, k.label());
                                }
                            });
                        ui.end_row();

                        // Value field depends on the chosen auth kind.
                        match s.auth_kind {
                            AuthKind::CookiesBrowser => {
                                ui.label("Browser");
                                ui.text_edit_singleline(&mut s.auth_value)
                                    .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (blank = global)");
                                ui.end_row();
                            }
                            AuthKind::CookiesFile => {
                                ui.label("Cookies file");
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(&mut s.auth_value);
                                    if ui.button("Browse…").clicked() {
                                        s.browse_req = Some(spawn_browse_file(
                                            &s.auth_value,
                                            |app, p| { if let Some(f) = &app.form { f.lock().unwrap().auth_value = p; } },
                                        ));
                                    }
                                });
                                ui.end_row();
                            }
                            AuthKind::Token => {
                                ui.label("Auth token");
                                ui.add(
                                    egui::TextEdit::singleline(&mut s.auth_value).password(true),
                                )
                                .on_hover_text("Twitch OAuth token (streamlink)");
                                ui.end_row();
                            }
                            AuthKind::Inherit | AuthKind::Disabled => {}
                        }

                        ui.label("Output folder").on_hover_text(
                            "Pre-filled by the resolved default; {name}/{platform}/\
                             {platform_short} typed in here directly are also expanded \
                             (once, on Save) if you'd rather template this instance's own \
                             folder than accept the default's.",
                        );
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut s.output_dir);
                            if ui.button("Browse…").clicked() {
                                s.browse_req = Some(spawn_browse_folder(
                                    &s.output_dir,
                                    |app, p| { if let Some(f) = &app.form { f.lock().unwrap().output_dir = p; } },
                                ));
                            }
                        });
                        ui.end_row();

                        let fn_tmpl_hint = "{name} {channel} {date} {time} {year} {month} {day} {hour} {minute} {second} {title} {title_trimmed} {games} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {tool} {mode} {platform} {platform_short} {went_live_date} {went_live_time} {timestamp}";
                        ui.label("Filename template").on_hover_text(fn_tmpl_hint);
                        ui.horizontal(|ui| {
                            let custom_presets = s.custom_presets.as_slice();
                            let (del, save) = filename_preset_combo(
                                ui,
                                "monitor_form_tmpl",
                                &mut s.filename_template,
                                custom_presets,
                            );
                            if del.is_some() { s.preset_delete = del; }
                            if save { s.preset_save_tmpl = Some(s.filename_template.clone()); }
                            ui.text_edit_singleline(&mut s.filename_template).on_hover_text(fn_tmpl_hint);
                            if ui.button("Design…").on_hover_text("Open the Format Designer to preview and compose the template").clicked() {
                                s.open_format_designer = true;
                            }
                        });
                        ui.end_row();

                        ui.label("Extra args");
                        ui.text_edit_singleline(&mut s.extra_args);
                        ui.end_row();
                    });
                });
                });
            },
        );

        let (do_save, closed, open_format_designer, browse_req, preset_delete, preset_save_tmpl) = {
            let mut f = form_arc.lock().unwrap();
            let result = (
                f.do_save,
                f.closed,
                f.open_format_designer,
                f.browse_req.take(),
                f.preset_delete.take(),
                f.preset_save_tmpl.take(),
            );
            // Consume: a failed-validation Save (empty name/URL) must not
            // keep re-triggering every subsequent call, permanently block
            // Cancel, or keep re-opening the Format Designer.
            f.do_save = false;
            f.closed = false;
            f.open_format_designer = false;
            result
        };

        if let Some(br) = browse_req {
            self.pending_browse = Some(br);
        }

        if do_save {
            self.save_form();
        } else if closed {
            self.form = None;
        }

        if open_format_designer {
            let tmpl = self.form.as_ref().map(|f| f.lock().unwrap().filename_template.clone()).unwrap_or_default();
            self.open_format_designer(tmpl, Some(FormatDesignerTarget::MonitorForm));
        }
        if let Some(id) = preset_delete {
            if let Err(e) = self.core.store.delete_filename_preset(id) {
                self.status = format!("Error deleting preset: {e:#}");
            } else {
                self.custom_presets = self.core.store.get_filename_presets().unwrap_or_default();
            }
        }
        if let Some(tmpl) = preset_save_tmpl {
            self.save_preset_dialog = Some(Arc::new(Mutex::new(SavePresetDraft {
                template: tmpl,
                name: String::new(),
                error: String::new(),
                do_save: false,
                closed: false,
            })));
        }
    }
}

/// Backing state for the "🤝 Collab history" window (one at a time; opening
/// another channel's history replaces it).
pub(super) struct CollabHistoryState {
    pub(super) channel_name: String,
    pub(super) sessions: Vec<crate::models::CollabSessionRow>,
    /// Set by the deferred closure on close; read back next call.
    pub(super) closed: bool,
}

/// Backing state for the "🤝 {partner} — sessions" drill-down window (one at
/// a time; opening another partner's sessions replaces it).
pub(super) struct PartnerSessionsState {
    pub(super) partner: String,
    pub(super) rows: Vec<crate::store::PartnerSessionRow>,
    /// Set by the deferred closure on close; read back next call.
    pub(super) closed: bool,
    /// Set by the deferred closure when "Jump" is clicked; applied by the
    /// wrapper (switches view + selects the monitor).
    pub(super) jump: Option<i64>,
}

/// One line per stored collab session: start, duration (or "ongoing"), source
/// marker (💬 Shared Chat / @ title mention), partners, and the host.
pub(super) fn collab_session_lines(sessions: &[crate::models::CollabSessionRow]) -> Vec<String> {
    sessions
        .iter()
        .map(|s| {
            let start = fmt_datetime_short(s.first_seen_at);
            let span = match s.ended_at {
                Some(end) => fmt_duration((end - s.first_seen_at).max(0)),
                // Still open: show how long it's been running so far.
                None => format!("{}+", fmt_duration((s.last_seen_at - s.first_seen_at).max(0))),
            };
            let names: Vec<String> = s
                .partners
                .iter()
                .map(|p| if p.from_title { format!("@{}", p.name) } else { p.name.clone() })
                .collect();
            let marker = if s.source == "shared_chat" { "💬" } else { "@" };
            let host = if s.source != "shared_chat" || s.host_id.is_empty() {
                String::new()
            } else if let Some(h) = s.partners.iter().find(|p| p.id == s.host_id) {
                format!("  (host: {})", h.name)
            } else {
                "  (host: this channel)".to_string()
            };
            format!("{start}  {span:>8}  {marker} {}{host}", names.join(", "))
        })
        .collect()
}
