//! Shared windows: confirms, ad/meta/recording-properties popups,
//! rename/preset/reorder dialogs, process manager, add/edit-stream form.

use super::*;

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
}

pub(super) struct SavePresetDraft {
    /// Template string to be saved.
    pub(super) template: String,
    /// Name the user has typed for this preset.
    pub(super) name: String,
    /// Validation or save error message (empty = none).
    pub(super) error: String,
}

impl StreamArchiverApp {
    /// Modal confirmation for deleting a monitor (the only destructive action).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_monitor_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete monitor")
                .with_inner_size([380.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(format!("Delete this capture instance for “{name}”?"));
                    ui.label("Removes the monitor and its settings. Recorded files are kept.");
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
        } else if do_cancel || !open {
            self.confirm_delete = None;
        }
    }

    /// Modal confirmation for deleting a whole channel (and all its instances +
    /// their history rows; recorded files are kept).
    #[allow(deprecated)]
    pub(super) fn confirm_delete_channel_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete_channel.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_channel_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete channel")
                .with_inner_size([400.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(format!("Delete the channel “{name}” and all its instances?"));
                    ui.label("Removes every instance and its history. Recorded files are kept.");
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
        } else if do_cancel || !open {
            self.confirm_delete_channel = None;
        }
    }

    /// Modal confirmation for tombstoning a schedule segment (it won't reappear
    /// on the next refresh).
    #[allow(deprecated)]
    pub(super) fn confirm_delete_segment_window(&mut self, ctx: &egui::Context) {
        let Some(sid) = self.confirm_delete_segment else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_segment_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete schedule item")
                .with_inner_size([400.0, 120.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Permanently delete this schedule item?");
                    ui.label("It will be suppressed and won't reappear on refresh.");
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
            if let Err(e) = self.core.store.delete_schedule_segment(sid) {
                self.status = format!("Error deleting schedule item: {e}");
            } else {
                self.schedule_hidden_segments.remove(&sid);
                self.spawn_reload_schedule();
                self.status = "Schedule item deleted.".into();
            }
            self.confirm_delete_segment = None;
        } else if do_cancel || !open {
            self.confirm_delete_segment = None;
        }
    }

    /// Confirmation dialog for "Quit & stop recordings" tray action.
    #[allow(deprecated)]
    pub(super) fn confirm_quit_stop_window(&mut self, ctx: &egui::Context) {
        if !self.confirm_quit_stop {
            return;
        }
        let mut open = true;
        let mut confirmed = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("confirm_quit_stop_vp"),
            egui::ViewportBuilder::default()
                .with_title("Stop recordings and quit?")
                .with_inner_size([380.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label("This will terminate all active recordings immediately.");
                    ui.label("In-progress captures will be finalized from whatever was written.");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let stop_btn = egui::Button::new("Stop & Quit")
                            .fill(egui::Color32::from_rgb(180, 40, 40));
                        if ui.add(stop_btn).clicked() {
                            confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                });
            },
        );

        if confirmed {
            self.core
                .force_stop_on_quit
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.quitting = true;
            self.confirm_quit_stop = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if !open {
            self.confirm_quit_stop = false;
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
    }

    /// Window listing where ad breaks cause hard cuts in a take's finished file.
    /// Opened by double-clicking an Ads / Ad time cell. Returns true on close.
    #[allow(deprecated)]
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
        let breaks = self.ad_break_cache.get(&rid).cloned().unwrap_or_default();
        let total: i64 = breaks.iter().map(|b| b.duration_secs).sum();
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("ad_breaks_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Ad breaks — cut points (take #{rid})"))
                .with_inner_size([360.0, 260.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if breaks.is_empty() {
                        ui.label("No ad breaks recorded for this take.");
                        return;
                    }
                    ui.label(format!(
                        "{} ad break(s), {} total. Each is a hard cut in the recorded file \
                         (streamlink filters ad segments out).",
                        breaks.len(),
                        fmt_duration(total),
                    ));
                    ui.add_space(6.0);
                    let lines = ad_cut_lines(&breaks);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(lines.join("\n"));
                    }
                });
            },
        );
        !open
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
    }

    /// One "channel history" window (all-time title/category changes for a
    /// monitor, independent of any recording); returns true on close.
    #[allow(deprecated)]
    pub(super) fn history_popup_window(&mut self, ctx: &egui::Context, monitor_id: i64) -> bool {
        self.ensure_history_cached(monitor_id);
        let changes = self.history_change_cache.get(&monitor_id).cloned().unwrap_or_default();
        let channel_name = self
            .core
            .store
            .get_monitor_with_channel(monitor_id)
            .ok()
            .flatten()
            .map(|r| r.channel.name)
            .unwrap_or_default();
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("channel_history_vp", monitor_id)),
            egui::ViewportBuilder::default()
                .with_title(format!("{channel_name} — title & category history"))
                .with_inner_size([480.0, 320.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    let lines = monitor_change_lines(&changes);
                    if lines.is_empty() {
                        ui.label("No title or category changes recorded yet.");
                        return;
                    }
                    ui.label(format!(
                        "{} change(s), newest first — every title/category transition \
                         ever observed for this instance, whether or not it was being \
                         recorded.",
                        lines.len(),
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(lines.join("\n"));
                    }
                });
            },
        );
        !open
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
        self.collab_history = Some(CollabHistoryState { channel_name, sessions });
    }

    /// The "🤝 Collab history" window: one line per stored "Stream Together"
    /// session (newest first) — when, how long, with whom, who hosted, and
    /// whether it came from Shared Chat or a title @mention.
    #[allow(deprecated)]
    pub(super) fn collab_history_window(&mut self, ctx: &egui::Context) {
        let Some(state) = &self.collab_history else { return };
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("collab_history_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("{} — collab history", state.channel_name))
                .with_inner_size([560.0, 360.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if state.sessions.is_empty() {
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
                        state.sessions.len()
                    ));
                    ui.add_space(6.0);
                    let lines = collab_session_lines(&state.sessions);
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
        if !open {
            self.collab_history = None;
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
    }

    /// One title/category-changes window; returns true on close.
    #[allow(deprecated)]
    pub(super) fn meta_popup_window(&mut self, ctx: &egui::Context, popup: MetaPopup) -> bool {
        // Build the change list: one take directly, or a stream's takes merged
        // chronologically with the per-take re-baselines dropped.
        let (changes, multi) = match &popup {
            MetaPopup::Take(rid) => {
                self.ensure_meta_cached(*rid);
                (self.meta_change_cache.get(rid).cloned().unwrap_or_default(), false)
            }
            MetaPopup::Stream(takes) => {
                for (rid, _) in takes {
                    self.ensure_meta_cached(*rid);
                }
                let loaded: Vec<(i64, Vec<StreamMetaChange>)> = takes
                    .iter()
                    .map(|(rid, started)| {
                        (*started, self.meta_change_cache.get(rid).cloned().unwrap_or_default())
                    })
                    .collect();
                (aggregate_stream_changes(&loaded), takes.len() > 1)
            }
        };
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("title_changes_vp", popup.key())),
            egui::ViewportBuilder::default()
                .with_title("Title & category changes")
                .with_inner_size([460.0, 280.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    // Only actual changes (the initial value of each field is the
                    // starting state, not a change); shown as `old → new`.
                    let lines = meta_change_lines(&changes);
                    if lines.is_empty() {
                        ui.label("No title or category changes recorded.");
                        return;
                    }
                    let scope = if multi {
                        "across this stream's takes"
                    } else {
                        "during this recording"
                    };
                    ui.label(format!(
                        "{} change(s) {scope} (offset from the start; each shows the \
                         value before → after).",
                        lines.len(),
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &lines {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(lines.join("\n"));
                    }
                });
            },
        );
        !open
    }

    /// Render every open recording-properties window (one per take).
    pub(super) fn recording_properties_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.rec_props_popups.len() {
            let rid = self.rec_props_popups[i].rec_id;
            if self.recording_properties_window(ctx, i) {
                closed.push(rid);
            }
        }
        if !closed.is_empty() {
            self.rec_props_popups.retain(|p| !closed.contains(&p.rec_id));
        }
    }

    /// Properties dialog for a single recording take.
    /// Opened via right-click → Properties on a history-tree take row.
    /// Returns true when the window should close.
    #[allow(deprecated)]
    pub(super) fn recording_properties_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let rid = self.rec_props_popups[idx].rec_id;
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
        let mut open = true;
        // Collect inter-frame actions so the closure doesn't borrow `self`.
        let mut copy_path: Option<String> = None;
        let mut notes_changed: Option<String> = None;
        // Snapshot the draft so the TextEdit can borrow it inside the closure.
        let mut notes_draft = self.rec_props_popups[idx].notes.clone();

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("recording_props_vp", rid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Recording properties — take #{rid}"))
                .with_inner_size([500.0, 540.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
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
                                        copy_path = Some(rec.output_path.clone());
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
                                if let Some(ended) = rec.ended_at {
                                    ui.label("Ended");
                                    ui.label(fmt_datetime_short(ended));
                                    ui.end_row();
                                }
                                ui.label("Duration");
                                ui.label(fmt_duration(rec.duration_secs(now)));
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
                            egui::TextEdit::multiline(&mut notes_draft)
                                .hint_text("Add notes for this take…")
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        if resp.changed() {
                            notes_changed = Some(notes_draft.clone());
                        }
                    });
                });
            },
        );
        if let Some(path) = copy_path {
            ctx.copy_text(path);
        }
        if let Some(notes) = notes_changed {
            self.rec_props_popups[idx].notes = notes.clone();
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
        !open
    }
    /// The "Edit schedule item" dialog (None = closed). Lets the user correct an
    /// occurrence's time/title/category or delete it; saving marks the row
    /// Rename-recording dialog: shows a text-edit for the new file stem, a live
    /// preview of the final filename, and OK / Cancel buttons.
    #[allow(deprecated)]
    pub(super) fn rename_dialog_window(&mut self, ctx: &egui::Context) {
        if !self.show_rename_dialog {
            return;
        }
        let rec_id = match self.rename_rec_id {
            Some(id) => id,
            None => { self.show_rename_dialog = false; return; }
        };

        let mut open = true;
        let mut do_rename = false;
        // These are local vars captured mutably by the closure.
        let mut new_draft = self.rename_draft.clone();
        let preview = self.rename_preview.clone();

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("rename_recording_vp"),
            egui::ViewportBuilder::default()
                .with_title("Rename recording")
                .with_inner_size([500.0, 160.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.label("New file name (without extension):");
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut new_draft)
                            .desired_width(ui.available_width())
                            .hint_text("new stem"),
                    );
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(format!("→ {preview}.mkv"))
                        .color(egui::Color32::from_rgb(0xa0, 0xa0, 0xa0)));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("✔  OK").clicked() {
                            do_rename = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            open = false;
                        }
                    });
                });
            },
        );

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
                    secs: 0, went_live: 0,
                },
            );
        }

        if do_rename {
            let stem = self.rename_preview.clone();
            self.core.manual(ManualCommand::RenameRecording { rec_id, new_stem: stem });
            self.show_rename_dialog = false;
            self.rename_rec_id = None;
        } else if !open {
            self.show_rename_dialog = false;
            self.rename_rec_id = None;
        }
    }

    /// "🚂 Mark hype train" dialog: records a train the automatic capture
    /// missed (channel + start time + optional duration), then retro-scores
    /// the stored contributions right before the start so the inference can
    /// be loosened toward what it should have caught (the auto-tune's
    /// manual-label path).
    #[allow(deprecated)]
    pub(super) fn hype_mark_window(&mut self, ctx: &egui::Context) {
        if !self.show_hype_mark {
            return;
        }
        let mut open = true;
        let mut do_mark = false;
        let mut channel = self.hype_mark_channel;
        let mut mins_ago = self.hype_mark_mins_ago;
        let mut abs = self.hype_mark_abs.clone();
        let mut dur = self.hype_mark_dur;
        let mut channels: Vec<(i64, String)> =
            self.channels.iter().map(|c| (c.id, c.name.clone())).collect();
        channels.sort_by_key(|(_, n)| n.to_lowercase());

        // Absolute local time wins when parseable; else "minutes ago".
        let parse_abs = |s: &str| -> Option<i64> {
            let dt = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M").ok()?;
            use chrono::offset::LocalResult;
            match dt.and_local_timezone(chrono::Local) {
                LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t.timestamp()),
                LocalResult::None => None,
            }
        };
        let abs_ts = parse_abs(&abs);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("hype_mark_vp"),
            egui::ViewportBuilder::default()
                .with_title("Mark hype train")
                .with_inner_size([460.0, 240.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
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
                            .find(|(id, _)| *id == channel)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| "— pick —".into());
                        egui::ComboBox::from_id_salt("hype_mark_channel")
                            .selected_text(sel)
                            .show_ui(ui, |ui| {
                                for (cid, name) in &channels {
                                    if ui.selectable_label(channel == *cid, name).clicked() {
                                        channel = *cid;
                                    }
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Started:");
                        ui.add(
                            egui::DragValue::new(&mut mins_ago).range(0..=1440).suffix(" min ago"),
                        )
                        .on_hover_text(
                            "How long ago the train kicked off — used when no \
                             absolute time is given below.",
                        );
                        ui.label("or at");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut abs)
                                .desired_width(130.0)
                                .hint_text("YYYY-MM-DD HH:MM"),
                        );
                        resp.on_hover_text(
                            "Absolute local start time — wins over 'minutes ago' \
                             when filled in and parseable.",
                        );
                        if !abs.trim().is_empty() && abs_ts.is_none() {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                                "⚠ format",
                            );
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Duration:");
                        ui.add(egui::DragValue::new(&mut dur).range(0..=240).suffix(" min"))
                            .on_hover_text(
                                "Optional — how long the train ran (0 = unknown). \
                                 Recorded in the event's detail only.",
                            );
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = ui.add_enabled(channel != 0, egui::Button::new("✔  Mark"));
                        if ok
                            .on_hover_text(
                                "Insert the train into the channel's event history \
                                 and (with auto-tune on) loosen the inference if it \
                                 should have fired on the stored contributions.",
                            )
                            .clicked()
                        {
                            do_mark = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            open = false;
                        }
                    });
                });
            },
        );

        self.hype_mark_channel = channel;
        self.hype_mark_mins_ago = mins_ago;
        self.hype_mark_abs = abs;
        self.hype_mark_dur = dur;

        if do_mark && channel != 0 {
            let start = abs_ts.unwrap_or_else(|| now_unix() - mins_ago.max(0) * 60);
            self.record_manual_hype_train(channel, start, dur);
            self.show_hype_mark = false;
        } else if !open {
            self.show_hype_mark = false;
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
        if let Some(p) = &mut self.viewer_stats_popup {
            p.data = None;
        }
        self.status = "Hype train marked".into();
    }

    /// "⚙ Sensitivity" per-channel hype-train override editor (opened from
    /// the Channel Stats controls row; `hype_override_for` = channel id).
    #[allow(deprecated)]
    pub(super) fn hype_override_window(&mut self, ctx: &egui::Context) {
        let Some(channel_id) = self.hype_override_for else { return };
        let name = self
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("channel {channel_id}"));
        let global = crate::hype::load_tuning(&self.core.store);
        let mut draft = self.hype_override_draft;
        let mut open = true;
        let mut do_save = false;

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

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("hype_override_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("{name} — hype sensitivity"))
                .with_inner_size([420.0, 210.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
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
                            do_save = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            open = false;
                        }
                    });
                });
            },
        );

        self.hype_override_draft = draft;
        if do_save {
            crate::hype::save_override(&self.core.store, channel_id, draft);
            self.hype_override_for = None;
        } else if !open {
            self.hype_override_for = None;
        }
    }

    /// Dialog for naming and saving a custom filename-template preset.
    #[allow(deprecated)]
    pub(super) fn save_preset_window(&mut self, ctx: &egui::Context) {
        if self.save_preset_dialog.is_none() {
            return;
        }
        let mut open = true;
        let mut do_save = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("save_preset_vp"),
            egui::ViewportBuilder::default()
                .with_title("Save as preset")
                .with_inner_size([340.0, 120.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                let Some(d) = self.save_preset_dialog.as_mut() else { return; };
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Preset name:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut d.name)
                            .hint_text("e.g. My favourite format")
                            .desired_width(310.0),
                    );
                    if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                        do_save = true;
                    }
                    if !d.error.is_empty() {
                        ui.colored_label(HL_ERROR_TEXT, &d.error);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let can_save = !d.name.trim().is_empty();
                        if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                            do_save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                });
            },
        );

        if do_save {
            if let Some(d) = self.save_preset_dialog.take() {
                let name = d.name.trim().to_string();
                match self.core.store.save_filename_preset(&name, &d.template) {
                    Ok(_) => {
                        self.custom_presets =
                            self.core.store.get_filename_presets().unwrap_or_default();
                        self.status = format!("Preset \"{name}\" saved.");
                    }
                    Err(e) => {
                        self.save_preset_dialog = Some(SavePresetDraft {
                            name: d.name,
                            template: d.template,
                            error: format!("Error saving: {e:#}"),
                        });
                    }
                }
            }
        } else if !open {
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
        let Some(state) = &mut self.reorder_columns else {
            return;
        };
        let table = state.table;
        let mut apply = false;
        let mut cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("reorder_columns_vp", table.key())),
            egui::ViewportBuilder::default()
                .with_title(format!("Reorder columns — {}", table_display_name(table)))
                .with_inner_size([320.0, 480.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    cancel = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Move columns into the order you want, then Apply.");
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        grid_columns::column_chooser_editor(
                            ui, &mut state.draft, columns_for(table), |id| id == "actions", true,
                        );
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("✔  Apply").clicked() {
                            apply = true;
                        }
                        if ui.button("✖  Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            },
        );

        if apply {
            let entries = state.draft.clone();
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
    #[allow(deprecated)]
    pub(super) fn processes_window(&mut self, ctx: &egui::Context) {
        use crate::models::{ContentType, DetachedKind};
        use egui_extras::{Column, TableBuilder};
        use std::time::{Duration, Instant};
        if !self.show_processes {
            return;
        }
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
        let stale = self
            .processes_refreshed
            .map(|t| t.elapsed() >= Duration::from_millis(1500))
            .unwrap_or(true);
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
            // Keep repainting until the load arrives.
            ctx.request_repaint_after(Duration::from_millis(50));
        } else {
            ctx.request_repaint_after(Duration::from_millis(1500));
        }

        let now = now_unix();
        let mut open = true;
        enum Act {
            Refresh,
            Stop(usize),
            Kill(usize),
            RevealLog(usize),
            RevealDir(usize),
        }
        let mut act: Option<Act> = None;
        // Persisted column order/visibility, taken as a local copy (mutated by
        // the header's column-chooser context menu, written back + persisted
        // once after the viewport closure below).
        let mut processes_entries = self.processes_grid.entries.clone();
        let processes_order = grid_columns::effective_order(&PROCESSES_COLUMNS, &processes_entries, |_| true);
        let processes_reset = self.processes_grid.note_order(&processes_order);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("processes_vp"),
            egui::ViewportBuilder::default()
                .with_title("🖥 Processes")
                .with_inner_size([800.0, 440.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} spawned process(es)", self.processes.len()));
                        // Child-viewport instrumentation: proves registration +
                        // highlight painting inside an immediate viewport.
                        if ui
                            .button("⟳ Refresh")
                            .inspect("Processes: Refresh button", &[])
                            .clicked()
                        {
                            act = Some(Act::Refresh);
                        }
                        ui.weak("Stop = graceful (file finalized) · Kill = force-terminate the tree");
                    });
                    ui.separator();
                    if self.processes.is_empty() {
                        ui.weak("No download tool processes are running.");
                        return;
                    }
                    let mut tb = TableBuilder::new(ui)
                        .id_salt(GridTableId::Processes.key())
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                    if processes_reset {
                        tb.reset();
                    }
                    for &i in &processes_order {
                        let c = &PROCESSES_COLUMNS[i];
                        let col = if c.stretch { Column::remainder().clip(true) } else { Column::auto() };
                        tb = tb.column(col);
                    }
                    tb.header(20.0, |mut h| {
                        for &i in &processes_order {
                            let c = &PROCESSES_COLUMNS[i];
                            h.col(|ui| {
                                if grid_header_cell_plain(ui, GridTableId::Processes, c, &mut processes_entries, &PROCESSES_COLUMNS) {
                                    self.reorder_columns = Some(ReorderColumnsState {
                                        table: GridTableId::Processes,
                                        draft: processes_entries.clone(),
                                    });
                                }
                            });
                        }
                    })
                    .body(|mut body| {
                        for (i, p) in self.processes.iter().enumerate() {
                            body.row(22.0, |mut row| {
                                for &ci in &processes_order {
                                    row.col(|ui| match PROCESSES_COLUMNS[ci].id {
                                        "pid" => { ui.monospace(p.pid.to_string()); }
                                        "type" => {
                                            // Map the process role to a content-type icon + label.
                                            // A live capture is "🎥 video"; its DASH companion leg
                                            // gets a "· dash" suffix. An on-demand download is the
                                            // "📼 VOD" so the two video kinds stay distinguishable.
                                            let t = match p.kind {
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
                                            };
                                            ui.label(t);
                                        }
                                        "name" => {
                                            ui.label(&p.name).on_hover_text(&p.capture_path);
                                        }
                                        "tool" => { ui.label(&p.tool); }
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
                                                act = Some(Act::Stop(i));
                                            }
                                            if ui
                                                .small_button("Kill")
                                                .on_hover_text(
                                                    "Force-terminate the whole process tree now — the \
                                                     capture may be left un-finalized.",
                                                )
                                                .clicked()
                                            {
                                                act = Some(Act::Kill(i));
                                            }
                                            if ui.small_button("Log").on_hover_text(&p.log_path).clicked() {
                                                act = Some(Act::RevealLog(i));
                                            }
                                            if ui.small_button("Folder").clicked() {
                                                act = Some(Act::RevealDir(i));
                                            }
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
        if processes_entries != self.processes_grid.entries {
            self.processes_grid.entries = processes_entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Processes, &self.processes_grid.entries);
        }

        if !open {
            self.show_processes = false;
        }
        match act {
            Some(Act::Refresh) => self.processes_refreshed = None,
            Some(Act::Stop(i)) => {
                if let Some(p) = self.processes.get(i) {
                    self.core.stop_process(p);
                    self.status = format!("Stopping pid {} ({})…", p.pid, p.name);
                    self.processes_refreshed = None;
                }
            }
            Some(Act::Kill(i)) => {
                if let Some(p) = self.processes.get(i) {
                    self.core.force_kill(p.pid, &p.job_name);
                    self.status = format!("Killed pid {} ({}).", p.pid, p.name);
                    self.processes_refreshed = None;
                }
            }
            Some(Act::RevealLog(i)) => {
                if let Some(p) = self.processes.get(i) {
                    crate::platform::open_path(std::path::Path::new(&p.log_path));
                }
            }
            Some(Act::RevealDir(i)) => {
                if let Some(p) = self.processes.get(i) {
                    if let Some(dir) = std::path::Path::new(&p.capture_path).parent() {
                        crate::platform::open_path(dir);
                    }
                }
            }
            None => {}
        }
    }
    #[allow(deprecated)]
    pub(super) fn form_window(&mut self, ctx: &egui::Context) {
        if self.form.is_none() {
            return;
        }
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;
        let mut open_format_designer = false;
        let mut browse_req: Option<PendingBrowse> = None;
        let mut form_preset_delete: Option<i64> = None;
        let mut form_preset_save_tmpl: Option<String> = None;

        let f = self.form.as_ref().unwrap();
        let title = if f.monitor_id.is_some() {
            "Edit instance"
        } else if f.channel_id.is_some() {
            "Add instance"
        } else {
            "Add stream (new channel)"
        };

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monitor_form_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.to_string())
                .with_inner_size([700.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                let form = self.form.as_mut().unwrap();
                let platform = Platform::detect(&form.url);
                // When the URL's platform changes, re-apply that platform's
                // defaults (tool, detection, container, quality, poll interval,
                // filename template, output dir). User overrides afterwards stick.
                if form.last_platform != Some(platform) {
                    let md = &self.monitor_defaults;
                    form.tool = md.resolve_tool(platform);
                    form.detection_method = md.resolve_detection(platform);
                    form.container = md.resolve_container(platform);
                    form.quality = md.resolve_quality(platform);
                    form.poll_interval_secs = md.resolve_poll_interval(platform);
                    form.filename_template = md.resolve_filename_template(platform);
                    form.output_dir = md.resolve_output_dir(platform, &self.settings.default_output_dir);
                    form.last_platform = Some(platform);
                }
                // The name belongs to the channel container; it's editable only
                // when creating a new channel. For an instance it's the container's
                // (rename via the channel row's ✏). The URL is per-instance and
                // always editable.
                let name_editable = form.channel_id.is_none();

                egui::Grid::new("form_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        let name_resp =
                            ui.add_enabled(name_editable, egui::TextEdit::singleline(&mut form.name));
                        if !name_editable {
                            name_resp.on_hover_text(
                                "The channel name — rename it from the channel row's ✏.",
                            );
                        }
                        ui.end_row();

                        ui.label("URL");
                        ui.add(egui::TextEdit::singleline(&mut form.url).desired_width(320.0))
                            .on_hover_text("This instance's source URL (platform auto-detected).");
                        ui.end_row();

                        ui.label("Platform");
                        ui.label(platform.label());
                        ui.end_row();

                        ui.label("Tool").on_hover_text(form.tool.tooltip());
                        egui::ComboBox::from_id_salt("tool_cb")
                            .selected_text(form.tool.label())
                            .show_ui(ui, |ui| {
                                for t in Tool::ALL {
                                    ui.selectable_value(&mut form.tool, t, t.label())
                                        .on_hover_text(t.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Detection")
                            .on_hover_text(form.detection_method.tooltip());
                        let methods = platform.detection_methods();
                        if !methods.contains(&form.detection_method) {
                            form.detection_method = platform.default_detection();
                        }
                        egui::ComboBox::from_id_salt("method_cb")
                            .selected_text(form.detection_method.label())
                            .show_ui(ui, |ui| {
                                for &dm in methods {
                                    ui.selectable_value(&mut form.detection_method, dm, dm.label())
                                        .on_hover_text(dm.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Poll interval (s)");
                        ui.add(egui::DragValue::new(&mut form.poll_interval_secs).range(5..=86400));
                        ui.end_row();

                        ui.label("Quality");
                        ui.text_edit_singleline(&mut form.quality);
                        ui.end_row();

                        ui.label("Container");
                        egui::ComboBox::from_id_salt("container_cb")
                            .selected_text(form.container.label())
                            .show_ui(ui, |ui| {
                                for c in Container::ALL {
                                    ui.selectable_value(&mut form.container, c, c.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Audio tracks");
                        ui.text_edit_singleline(&mut form.audio_tracks).on_hover_text(
                            "Audio tracks to capture (streamlink --hls-audio-select). \
                             Empty = the tool's default single track; 'all' (or '*') = \
                             every track; or a comma-separated list of language \
                             codes/names. streamlink-only; ffmpeg copy keeps all tracks.",
                        );
                        ui.end_row();

                        ui.label("Subtitle tracks");
                        ui.text_edit_singleline(&mut form.subtitle_tracks).on_hover_text(
                            "Subtitle tracks to capture (yt-dlp --sub-langs, written as \
                             sidecar files next to the recording). Empty = none; 'all' \
                             (or '*') = every subtitle; or a comma-separated list of \
                             language codes. yt-dlp-only; streamlink can't mux subtitles. \
                             Best-effort for live streams.",
                        );
                        ui.end_row();

                        ui.label("Log chat");
                        ui.checkbox(&mut form.chat_log, "").on_hover_text(
                            "Save chat alongside the recording. Twitch: a built-in \
                             anonymous chat logger writes a .chat.jsonl sidecar. YouTube \
                             (yt-dlp tool): yt-dlp's live_chat writes a .live_chat.json \
                             sidecar. Other platforms/tools don't capture chat.",
                        );
                        ui.end_row();

                        ui.label("Fetch thumbnail");
                        ui.checkbox(&mut form.fetch_thumbnail, "").on_hover_text(
                            "Download the stream thumbnail alongside the recording \
                             ({stem}.thumbnail.jpg). For yt-dlp, passes --write-thumbnail; \
                             for Twitch/Kick/YouTube, fetches the URL from detection metadata.",
                        );
                        ui.end_row();

                        ui.label("Thumbnail in notification");
                        ui.add_enabled(
                            form.fetch_thumbnail,
                            egui::Checkbox::new(&mut form.thumbnail_in_toast, ""),
                        ).on_hover_text(
                            "Use the stream thumbnail as the hero image in the \
                             recording-started notification (instead of the channel's \
                             static banner). Most useful for YouTube, where each stream \
                             has a unique thumbnail. Requires \"Fetch thumbnail\" to be on.",
                        );
                        ui.end_row();

                        ui.label("Fetch chat assets");
                        ui.checkbox(&mut form.fetch_chat_assets, "").on_hover_text(
                            "Download channel icon, offline banner, Twitch badges, and \
                             emotes (including BTTV, FFZ, 7TV) into channel_assets/ \
                             alongside recordings. Needed for full offline chat replay. \
                             Refreshed at most once per 24 hours.",
                        );
                        ui.end_row();

                        ui.label("Capture from start");
                        ui.checkbox(&mut form.capture_from_start, "").on_hover_text(
                            "yt-dlp --live-from-start / streamlink --hls-live-restart",
                        );
                        ui.end_row();

                        if Platform::detect(&form.url) == Platform::YouTube {
                            ui.label("Dual capture (SABR + DASH)");
                            ui.checkbox(&mut form.dual_capture, "").on_hover_text(
                                "YouTube only: also run a second concurrent DASH capture \
                                 (system yt-dlp, live edge) when wanted formats span both SABR \
                                 and DASH. Produces a second recording in the same take. \
                                 Needs Capture-from-start and a configured SABR build.",
                            );
                            ui.end_row();

                            ui.label("Video codec / quality");
                            egui::ComboBox::from_id_salt("form_sabr_codec_pref")
                                .selected_text(form.sabr_codec_pref.label())
                                .show_ui(ui, |ui| {
                                    for &p in &SabrCodecPref::ALL {
                                        ui.selectable_value(
                                            &mut form.sabr_codec_pref,
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
                            if form.sabr_codec_pref == SabrCodecPref::Custom {
                                ui.label("Custom -S sort");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.sabr_codec_custom)
                                        .hint_text("res,fps,vcodec:h264")
                                        .desired_width(f32::INFINITY),
                                )
                                .on_hover_text(
                                    "Raw yt-dlp -S format-sort. Lead with res,fps so \
                                     resolution/fps win and codec/bitrate is only the tiebreak.",
                                );
                                ui.end_row();
                            }
                        }

                        ui.label("Ad-free");
                        ui.checkbox(&mut form.ad_free, "").on_hover_text(
                            "Mark this instance ad-free for your account (YouTube \
                             membership/Premium, Twitch Turbo/sub) so captures won't have \
                             ad-break hard cuts. For Twitch with a connected account, sub \
                             status is also detected automatically.",
                        );
                        ui.end_row();

                        ui.label("Download VOD after end");
                        tristate_combo(ui, "form_vod_download", &mut form.vod_download)
                            .on_hover_text(
                                "Download the platform's published VOD after this instance's \
                                 stream ends. Inherit follows the channel, then the global default.",
                            );
                        ui.end_row();

                        ui.label("Replace with VOD");
                        tristate_combo(ui, "form_vod_replace", &mut form.vod_replace)
                            .on_hover_text(
                                "Replace the live recording with the downloaded VOD when it \
                                 succeeds (never for a muted Twitch VOD). Inherit follows the \
                                 channel, then the global default.",
                            );
                        ui.end_row();

                        ui.label("Fetch new head backfill on new take");
                        tristate_combo(ui, "form_head_backfill_fetch", &mut form.head_backfill_fetch)
                            .on_hover_text(
                                "Capture-from-start only: fetch a fresh head backfill for a retake \
                                 (reconnect mid-broadcast), not just the stream's first take. \
                                 Inherit follows the channel, then the global default.",
                            );
                        ui.end_row();

                        ui.label("Replace old head (if new is undamaged)");
                        tristate_combo(ui, "form_head_backfill_replace", &mut form.head_backfill_replace)
                            .on_hover_text(
                                "Once a fresh head backfill passes its integrity checks, delete \
                                 older takes' now-redundant head files for the same stream. Only \
                                 takes effect when fetching a new head is also on. Inherit follows \
                                 the channel, then the global default.",
                            );
                        ui.end_row();

                        ui.label("After full.mkv join");
                        join_cleanup_combo(ui, "form_join_cleanup", &mut form.join_cleanup)
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
                        disposal_method_combo(ui, "form_disposal_method", &mut form.disposal_method)
                            .on_hover_text(
                                "How automatic media deletions for this instance are executed \
                                 (post-join cleanup, superseded heads, a live capture replaced \
                                 by its VOD): moved to the configured trash folder, sent to the \
                                 Recycle Bin, or deleted permanently. Inherit follows the \
                                 channel, then the global default.",
                            );
                        ui.end_row();

                        ui.label("Pin as preferred platform");
                        ui.checkbox(&mut form.primary_pin, "").on_hover_text(
                            "Always show THIS instance's info on the channel row while it's \
                             live, even if a sibling instance (another platform) went live \
                             earlier or the channel/global preference points elsewhere — the \
                             strongest of the three preference tiers.",
                        );
                        ui.end_row();

                        ui.label("Enabled");
                        ui.checkbox(&mut form.automation_enabled, "")
                            .on_hover_text(
                                "Master switch (same as the Enabled column). Off = fully \
                                 dormant: no detection, recording, or asset/about/posts/schedule \
                                 fetch until you act manually (▶ Start, ⟳ Refetch). Independent \
                                 from Auto below.",
                            );
                        ui.end_row();

                        ui.label("Auto");
                        ui.checkbox(&mut form.enabled, "")
                            .on_hover_text(
                                "Auto-record: automatically record to disk when this channel \
                                 goes live (a disk-space control; same as the Auto column). It \
                                 does NOT gate detection, metadata, posts, schedules or assets — \
                                 those run while the channel is Enabled. Recording only starts \
                                 automatically when this is on; otherwise press ▶ yourself, or a \
                                 trigger word matches the live title/game.",
                            );
                        ui.end_row();

                        ui.label("Auth");
                        egui::ComboBox::from_id_salt("auth_cb")
                            .selected_text(form.auth_kind.label())
                            .show_ui(ui, |ui| {
                                for k in AuthKind::ALL {
                                    ui.selectable_value(&mut form.auth_kind, k, k.label());
                                }
                            });
                        ui.end_row();

                        // Value field depends on the chosen auth kind.
                        match form.auth_kind {
                            AuthKind::CookiesBrowser => {
                                ui.label("Browser");
                                ui.text_edit_singleline(&mut form.auth_value)
                                    .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (blank = global)");
                                ui.end_row();
                            }
                            AuthKind::CookiesFile => {
                                ui.label("Cookies file");
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(&mut form.auth_value);
                                    if ui.button("Browse…").clicked() {
                                        browse_req = Some(spawn_browse_file(
                                            &form.auth_value,
                                            |app, p| { if let Some(f) = &mut app.form { f.auth_value = p; } },
                                        ));
                                    }
                                });
                                ui.end_row();
                            }
                            AuthKind::Token => {
                                ui.label("Auth token");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.auth_value).password(true),
                                )
                                .on_hover_text("Twitch OAuth token (streamlink)");
                                ui.end_row();
                            }
                            AuthKind::Inherit | AuthKind::Disabled => {}
                        }

                        ui.label("Output folder");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.output_dir);
                            if ui.button("Browse…").clicked() {
                                browse_req = Some(spawn_browse_folder(
                                    &form.output_dir,
                                    |app, p| { if let Some(f) = &mut app.form { f.output_dir = p; } },
                                ));
                            }
                        });
                        ui.end_row();

                        let fn_tmpl_hint = "{name} {date} {time} {year} {month} {day} {hour} {minute} {second} {title} {title_trimmed} {games} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {tool} {mode} {platform} {went_live_date} {went_live_time} {timestamp}";
                        ui.label("Filename template").on_hover_text(fn_tmpl_hint);
                        ui.horizontal(|ui| {
                            let custom_presets = self.custom_presets.as_slice();
                            let (del, save) = filename_preset_combo(
                                ui,
                                "monitor_form_tmpl",
                                &mut form.filename_template,
                                custom_presets,
                            );
                            if del.is_some() { form_preset_delete = del; }
                            if save { form_preset_save_tmpl = Some(form.filename_template.clone()); }
                            ui.text_edit_singleline(&mut form.filename_template).on_hover_text(fn_tmpl_hint);
                            if ui.button("Design…").on_hover_text("Open the Format Designer to preview and compose the template").clicked() {
                                open_format_designer = true;
                            }
                        });
                        ui.end_row();

                        ui.label("Extra args");
                        ui.text_edit_singleline(&mut form.extra_args);
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
                });
            },
        );

        if let Some(br) = browse_req {
            self.pending_browse = Some(br);
        }

        if do_save {
            self.save_form();
        } else if do_cancel || !open {
            self.form = None;
        }

        if open_format_designer {
            let tmpl = self.form.as_ref().map(|f| f.filename_template.clone()).unwrap_or_default();
            self.open_format_designer(tmpl, Some(FormatDesignerTarget::MonitorForm));
        }
        if let Some(id) = form_preset_delete {
            if let Err(e) = self.core.store.delete_filename_preset(id) {
                self.status = format!("Error deleting preset: {e:#}");
            } else {
                self.custom_presets = self.core.store.get_filename_presets().unwrap_or_default();
            }
        }
        if let Some(tmpl) = form_preset_save_tmpl {
            self.save_preset_dialog = Some(SavePresetDraft {
                template: tmpl,
                name: String::new(),
                error: String::new(),
            });
        }
    }
}

/// Backing state for the "🤝 Collab history" window (one at a time; opening
/// another channel's history replaces it).
pub(super) struct CollabHistoryState {
    pub(super) channel_name: String,
    pub(super) sessions: Vec<crate::models::CollabSessionRow>,
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
