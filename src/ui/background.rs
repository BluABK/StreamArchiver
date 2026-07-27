//! Background-tasks view and the stats view.

use super::*;

impl StreamArchiverApp {
    pub(super) fn background_view(&mut self, ui: &mut egui::Ui) {
        use egui_extras::{Column, TableBuilder};
        // Both elapsed-time and next-run-countdown labels update every second —
        // request a repaint so they tick continuously without needing mouse input.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        let now = now_unix();
        // Next-run estimates, plus the editable enable/disable state for each job.
        let reg = self.core.jobs.lock().unwrap().clone();
        let mut toggles: Vec<(&'static str, &'static str, bool)> = crate::events::TOGGLEABLE_JOBS
            .iter()
            .map(|(name, key)| (*name, *key, self.job_toggles.get(*key).copied().unwrap_or(true)))
            .collect();
        let before: Vec<bool> = toggles.iter().map(|t| t.2).collect();
        // Persisted column order/visibility for the two Background tables, taken
        // as local copies (mutated by each header's column-chooser context
        // menu, written back + persisted once after the ScrollArea below).
        let mut bg_active_entries = self.bg_active_grid.entries.clone();
        let bg_active_order = grid_columns::effective_order(&BG_ACTIVE_COLUMNS, &bg_active_entries, |_| true);
        let bg_active_reset = self.bg_active_grid.note_order(&bg_active_order);
        let mut bg_recent_entries = self.bg_recent_grid.entries.clone();
        let bg_recent_order = grid_columns::effective_order(&BG_RECENT_COLUMNS, &bg_recent_entries, |_| true);
        let bg_recent_reset = self.bg_recent_grid.note_order(&bg_recent_order);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(8.0);

            // ── Scheduled (periodic jobs) ────────────────────────────────
            egui::CollapsingHeader::new("Scheduled")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Recurring background jobs. Untick to disable — turning off Live poll \
                             pauses all detection/recording.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(4.0);
                    egui::Grid::new("bg_scheduled_grid")
                        .num_columns(4)
                        .striped(true)
                        .spacing([16.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("On");
                            ui.strong("Job");
                            ui.strong("Every");
                            ui.strong("Next run");
                            ui.end_row();
                            for (name, _key, en) in toggles.iter_mut() {
                                ui.checkbox(en, "");
                                ui.label(*name);
                                let r = reg.iter().find(|j| j.name == *name);
                                ui.label(
                                    r.map(|j| fmt_duration_secs(j.interval_secs))
                                        .unwrap_or_else(|| "—".into()),
                                );
                                if !*en {
                                    ui.weak("disabled");
                                } else {
                                    ui.label(
                                        r.map(|j| fmt_relative_future(j.next_run_at - now))
                                            .unwrap_or_else(|| "pending".into()),
                                    );
                                }
                                ui.end_row();
                            }
                        });
                });

            ui.add_space(12.0);

            // ── GVS PO token server (managed helper process) ─────────────
            {
                use crate::pot_server::PotMode;
                let st = crate::pot_server::status();
                ui.horizontal(|ui| {
                    let (txt, color) = match &st.mode {
                        PotMode::Managed { pid } => (
                            format!(
                                "running (managed) · pid {pid}{}",
                                st.last_ping
                                    .as_ref()
                                    .map(|p| {
                                        let s = p.uptime_secs as u64;
                                        format!(" · up {}:{:02}:{:02} · v{}", s / 3600, (s % 3600) / 60, s % 60, p.version)
                                    })
                                    .unwrap_or_default()
                            ),
                            egui::Color32::from_rgb(0x39, 0xb0, 0x54),
                        ),
                        PotMode::External => (
                            "running (external)".to_string(),
                            egui::Color32::from_rgb(0x39, 0xb0, 0x54),
                        ),
                        PotMode::Starting => ("starting…".to_string(), egui::Color32::from_rgb(0xd9, 0xa4, 0x06)),
                        PotMode::Down if st.desired == crate::pot_server::Desired::ForcedOff => {
                            ("stopped by user".to_string(), egui::Color32::GRAY)
                        }
                        PotMode::Down => ("down — restarting".to_string(), egui::Color32::from_rgb(0xd9, 0x53, 0x4f)),
                        PotMode::Disabled => ("not managed".to_string(), egui::Color32::GRAY),
                        PotMode::Failed { reason } => (
                            format!("failed: {reason}"),
                            egui::Color32::from_rgb(0xd9, 0x53, 0x4f),
                        ),
                    };
                    ui.strong("🎫 PO token server:").on_hover_text(
                        "The bgutil GVS PO token provider server, managed by the app — \
                         YouTube SABR captures fail without it. Auto-launched at startup, \
                         health-checked every 30s, restarted on crash, started on demand \
                         when a capture dies for lack of a token. Configure under \
                         Settings → Downloads → GVS PO token server.",
                    );
                    ui.label(egui::RichText::new(txt).color(color)).on_hover_text(format!(
                        "Health-checked via GET {}/ping. \"External\" = a server the app \
                         didn't spawn is answering there (used as-is, never killed).",
                        st.base_url
                    ));
                    if ui
                        .small_button("📜 Log")
                        .on_hover_text("Open a live tail of the server's log window.")
                        .clicked()
                    {
                        self.show_pot_server_log = true;
                    }
                });
            }

            ui.add_space(12.0);

            // ── Planned (queued head backfills) ─────────────────────────
            // One-off, per-take work items awaiting `head_backfill_job`'s
            // fixed settle wait — distinct from the recurring jobs above.
            // Disappears once the take moves to Active (fetching) or resolves
            // with nothing to do; see `Recording::head_backfill_state`.
            let planned = self.core.store.queued_head_backfills().unwrap_or_default();
            if !planned.is_empty() {
                egui::CollapsingHeader::new(format!("Planned ({})", planned.len()))
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::Grid::new("bg_planned_grid")
                            .num_columns(3)
                            .striped(true)
                            .spacing([16.0, 6.0])
                            .show(ui, |ui| {
                                ui.strong("Channel");
                                ui.strong("Job");
                                ui.strong("Starts");
                                ui.end_row();
                                for p in &planned {
                                    ui.label(&p.channel);
                                    ui.label("Head backfill");
                                    let eta = p.started_at + crate::downloader::HEAD_BACKFILL_SETTLE_SECS - now;
                                    ui.label(fmt_relative_future(eta)).on_hover_text(
                                        "Waiting for the CDN's live-VOD folder to appear and \
                                         streamlink's own rewind (if any) to settle before checking \
                                         whether anything needs backfilling.",
                                    );
                                    ui.end_row();
                                }
                            });
                    });
                ui.add_space(12.0);
            }

            // ── Active tasks ─────────────────────────────────────────────
            egui::CollapsingHeader::new(format!("Active ({})", self.background_tasks.len()))
                .default_open(true)
                .show(ui, |ui| {
                    // Live disk-gate status, ONE LINE PER DRIVE: bulk passes
                    // (remux/merge/concat/embed) run one at a time per disk — this is
                    // the authoritative "what is actually running right now, and how
                    // many are queued behind it" line for those jobs. Per-drive (not
                    // one global summary) so the emergency Pause/Kill controls below
                    // apply to the right disk during an actual crisis.
                    {
                        let disk_cfg = crate::io_gate::disk_limits_config();
                        let effective_paused = |drive: &str| {
                            disk_cfg.drives.get(drive).map(|d| d.paused).unwrap_or(disk_cfg.default.paused)
                        };
                        let active_drives = crate::io_gate::local_gate_status_by_drive();
                        // A paused drive with nothing left to show (I/O has fully
                        // drained) stays in the list anyway — a pause with no visible
                        // activity left is exactly the state you could otherwise
                        // forget about and never un-pause; it must stay reachable
                        // from here. Covers both an explicit per-drive override AND
                        // a paused Default row (checked against every drive that's
                        // done bulk I/O this session, same set Settings' Default row
                        // itself uses for its live readout).
                        let mut drives: Vec<String> = active_drives.iter().map(|(d, ..)| d.clone()).collect();
                        for letter in crate::io_gate::active_gate_letters() {
                            if effective_paused(&letter) {
                                drives.push(letter);
                            }
                        }
                        drives.sort();
                        drives.dedup();
                        for drive in drives {
                            let (holders, waiting) = active_drives
                                .iter()
                                .find(|(d, ..)| *d == drive)
                                .map(|(_, h, w)| (h.clone(), *w))
                                .unwrap_or_default();
                            let paused = effective_paused(&drive);
                            let resp = ui
                                .horizontal(|ui| {
                                    ui.label(format!("🖴 Disk gate [{drive}:]:"));
                                    if holders.is_empty() {
                                        ui.weak(if paused { "paused" } else { "turning over…" });
                                    }
                                    if waiting > 0 {
                                        ui.weak(format!("· {waiting} queued"));
                                        let toggle =
                                            if self.bg_show_gate_queue { "▼ Hide queue" } else { "▶ View queue" };
                                        if ui.small_button(toggle).clicked() {
                                            self.bg_show_gate_queue = !self.bg_show_gate_queue;
                                        }
                                    }
                                    // Emergency controls. Pause blocks new concat/
                                    // remux/embed passes on THIS drive only — gap
                                    // recovery, head-backfill fetch, VOD recovery, and
                                    // live captures use a separate gate and are never
                                    // affected. Kill force-terminates whatever's
                                    // running right now (pause alone can't — nothing
                                    // preempts an in-flight pass).
                                    let pause_label = if paused { "▶ Resume" } else { "⏸ Pause" };
                                    if ui
                                        .small_button(pause_label)
                                        .on_hover_text(if paused {
                                            "Let new concat/remux/embed passes start on this drive again."
                                        } else {
                                            "Block new concat/remux/embed passes on this drive — gap \
                                             recovery/head-backfill fetch/VOD recovery/live captures are \
                                             unaffected (separate gate). Doesn't stop anything already \
                                             running; use Kill current for that."
                                        })
                                        .clicked()
                                    {
                                        let drive = drive.clone();
                                        crate::io_gate::modify_disk_limits(|cfg| {
                                            let default = cfg.default.clone();
                                            let entry = cfg.drives.entry(drive.clone()).or_insert(default);
                                            entry.paused = !paused;
                                        });
                                        self.status = format!(
                                            "{} local passes (concat/remux/embeds) on drive {drive}:",
                                            if paused { "Resumed" } else { "Paused" }
                                        );
                                    }
                                    if !holders.is_empty()
                                        && ui
                                            .small_button("🗑 Kill current")
                                            .on_hover_text(
                                                "Force-terminate whatever's running on this drive's \
                                                 local-pass gate right now (discards its progress — the \
                                                 source files are untouched, it just restarts from \
                                                 scratch later).",
                                            )
                                            .clicked()
                                    {
                                        let n = crate::io_gate::kill_local_holder(&drive);
                                        self.status =
                                            format!("Killed {n} pass(es) on drive {drive}: — they'll retry later.");
                                    }
                                })
                                .response;
                            let all: String = holders
                                .iter()
                                .map(|(l, h)| format!("{l} — running {}", fmt_duration(*h as i64)))
                                .collect::<Vec<_>>()
                                .join("\n");
                            resp.on_hover_text(format!(
                                "Bulk local passes take turns per disk (permits per Settings → \
                                 Recording → Disk I/O limits). Queued passes list their wait in \
                                 their own task row.\n\n{all}"
                            ));
                            // Every pass CURRENTLY holding a permit on this drive, one
                            // indented line each — this drive allows more than one
                            // concurrent pass whenever its permit count is above 1
                            // (Settings → Recording → Disk I/O limits, static or
                            // Dynamic), so all of them can be genuinely running at
                            // once, not just the longest-running one.
                            for (label, held) in &holders {
                                ui.horizontal(|ui| {
                                    ui.add_space(24.0);
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        format!("{label} — running {}", fmt_duration(*held as i64)),
                                    );
                                });
                            }
                            // The queue itself: every pass waiting for a gate on THIS
                            // drive, in line order — includes passes that have no
                            // task row of their own (batch re-remux items, embeds,
                            // head joins).
                            if self.bg_show_gate_queue && waiting > 0 {
                                for (i, (label, secs)) in crate::io_gate::local_gate_queue()
                                    .into_iter()
                                    .filter(|(_, d, _)| *d == drive)
                                    .map(|(l, _, s)| (l, s))
                                    .enumerate()
                                {
                                    ui.horizontal(|ui| {
                                        ui.add_space(24.0);
                                        ui.weak(format!(
                                            "{}. {label} — waiting {}",
                                            i + 1,
                                            fmt_duration(secs as i64)
                                        ));
                                    });
                                }
                            }
                        }
                    }
                    ui.add_space(4.0);

                    if self.background_tasks.is_empty() {
                        ui.weak("No tasks running.");
                    } else {
                        ui.push_id("bg_active", |ui| {
                            let mut tb = TableBuilder::new(ui)
                                .id_salt(GridTableId::BgActive.key())
                                .striped(true)
                                .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                            if bg_active_reset {
                                tb.reset();
                            }
                            for &i in &bg_active_order {
                                let c = &BG_ACTIVE_COLUMNS[i];
                                let col = if c.stretch { Column::remainder().clip(true) } else { Column::auto() };
                                tb = tb.column(col);
                            }
                            tb.header(20.0, |mut h| {
                                for &i in &bg_active_order {
                                    let c = &BG_ACTIVE_COLUMNS[i];
                                    h.col(|ui| {
                                        if grid_header_cell_plain(ui, GridTableId::BgActive, c, &mut bg_active_entries, &BG_ACTIVE_COLUMNS) {
                                            self.reorder_columns = Some(ReorderColumnsState {
                                                table: GridTableId::BgActive,
                                                draft: bg_active_entries.clone(),
                                            });
                                        }
                                    });
                                }
                            })
                            .body(|mut body| {
                                for task in &self.background_tasks {
                                    body.row(20.0, |mut row| {
                                        for &i in &bg_active_order {
                                            row.col(|ui| match BG_ACTIVE_COLUMNS[i].id {
                                                "channel" => { ui.label(&task.label); }
                                                "task" => { ui.label(task.kind.label()); }
                                                "detail" => {
                                                    // Show live ffmpeg stats when available; fall back to static detail.
                                                    let text = task.progress_info.as_deref().unwrap_or(&task.detail);
                                                    if let Some(p) = task.progress {
                                                        ui.add(egui::ProgressBar::new(p).show_percentage().desired_width(90.0));
                                                        ui.label(text);
                                                    } else {
                                                        ui.label(text);
                                                    }
                                                    if let crate::events::BackgroundTaskKind::Chapters(rec_id) = task.kind
                                                        && ui
                                                            .small_button("ℹ")
                                                            .on_hover_text(
                                                                "Which stream, which file, and which \
                                                                 chapters at which timestamp.",
                                                            )
                                                            .clicked()
                                                        && !self.chapters_popups.contains(&rec_id)
                                                    {
                                                        self.chapters_popups.push(rec_id);
                                                    }
                                                }
                                                "elapsed" => {
                                                    ui.label(format!(
                                                        "⏳ {}",
                                                        fmt_duration_secs(now - task.started_at)
                                                    ));
                                                }
                                                _ => {}
                                            });
                                        }
                                    });
                                }
                            });
                        });
                    }
                });

            ui.add_space(12.0);

            // ── Queued (chapters embed / gap-splice backlog) ─────────────
            // `sweep_pending_chapters`/`sweep_pending_gap_splices` work
            // through their "fresh" (never-attempted) candidates strictly
            // one at a time — awaited in sequence, oldest first — to avoid
            // flooding the shared disk gate with a pile of concurrent
            // full-file ffmpeg passes. Everything behind the one currently
            // running has no disk-gate entry and no Active-panel row of its
            // own, so a large backlog (e.g. a bulk re-embed, or the first
            // run of a new feature against an existing library) would
            // otherwise be entirely invisible until its turn comes.
            {
                let active_chapters: std::collections::HashSet<i64> = self
                    .background_tasks
                    .iter()
                    .filter_map(|t| match t.kind {
                        crate::events::BackgroundTaskKind::Chapters(id) => Some(id),
                        _ => None,
                    })
                    .collect();
                let active_splices: std::collections::HashSet<i64> = self
                    .background_tasks
                    .iter()
                    .filter_map(|t| match t.kind {
                        crate::events::BackgroundTaskKind::GapSplice(id) => Some(id),
                        _ => None,
                    })
                    .collect();
                let queued_chapters: Vec<_> = self
                    .core
                    .store
                    .queued_chapters_embeds()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|q| !active_chapters.contains(&q.rec_id))
                    .collect();
                let queued_splices: Vec<_> = self
                    .core
                    .store
                    .queued_gap_splices()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|q| !active_splices.contains(&q.rec_id))
                    .collect();
                const MAX_QUEUED_ROWS: usize = 15;
                let total_queued = queued_chapters.len() + queued_splices.len();
                if total_queued > 0 {
                    egui::CollapsingHeader::new(format!("Queued ({total_queued})"))
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(
                                    "Waiting for the sequential embed/splice backlog sweep to reach \
                                     them — processed one at a time, oldest first, so a large backlog \
                                     doesn't flood the disk gate.",
                                )
                                .small()
                                .weak(),
                            );
                            ui.add_space(4.0);
                            egui::Grid::new("bg_queued_grid")
                                .num_columns(8)
                                .striped(true)
                                .spacing([16.0, 6.0])
                                .show(ui, |ui| {
                                    ui.strong("Channel");
                                    ui.strong("Take");
                                    ui.strong("Job");
                                    ui.strong("Position");
                                    ui.strong("Drive");
                                    ui.strong("Added to queue");
                                    ui.strong("Time in queue");
                                    ui.strong("Title");
                                    ui.end_row();
                                    const TITLE_MAX_CHARS: usize = 40;
                                    let recorded_hover = |started_at: i64| -> String {
                                        use chrono::{Local, TimeZone};
                                        Local
                                            .timestamp_opt(started_at, 0)
                                            .single()
                                            .map(|dt| format!("Recorded {}", dt.format("%Y-%m-%d %H:%M")))
                                            .unwrap_or_default()
                                    };
                                    let drive_label = |q: &crate::models::QueuedEmbedJob| -> String {
                                        crate::iomon::drive_letter(std::path::Path::new(&q.output_path))
                                            .map(|d| format!("{d}:"))
                                            .unwrap_or_else(|| "—".to_string())
                                    };
                                    let queued_row = |ui: &mut egui::Ui, q: &crate::models::QueuedEmbedJob, job: &str, pos: usize, total: usize| {
                                        ui.label(&q.channel).on_hover_text(recorded_hover(q.started_at));
                                        ui.label(format!("Take {}", q.take_number));
                                        ui.label(job);
                                        ui.label(format!("{pos} of {total}"));
                                        ui.label(drive_label(q));
                                        ui.label(fmt_datetime_short(q.queued_at));
                                        ui.label(fmt_duration_secs((now - q.queued_at).max(0)));
                                        if q.title.is_empty() {
                                            ui.weak("—");
                                        } else {
                                            let short = super::chat::truncate_label(&q.title, TITLE_MAX_CHARS);
                                            let label = ui.label(&short);
                                            if short != q.title {
                                                label.on_hover_text(&q.title);
                                            }
                                        }
                                        ui.end_row();
                                    };
                                    for (i, q) in queued_chapters.iter().take(MAX_QUEUED_ROWS).enumerate() {
                                        queued_row(ui, q, "Chapters embed", i + 1, queued_chapters.len());
                                    }
                                    if queued_chapters.len() > MAX_QUEUED_ROWS {
                                        ui.weak(format!("(+{} more)", queued_chapters.len() - MAX_QUEUED_ROWS));
                                        ui.end_row();
                                    }
                                    for (i, q) in queued_splices.iter().take(MAX_QUEUED_ROWS).enumerate() {
                                        queued_row(ui, q, "Gap splice", i + 1, queued_splices.len());
                                    }
                                    if queued_splices.len() > MAX_QUEUED_ROWS {
                                        ui.weak(format!("(+{} more)", queued_splices.len() - MAX_QUEUED_ROWS));
                                        ui.end_row();
                                    }
                                });
                        });
                    ui.add_space(12.0);
                }
            }
            ui.add_space(12.0);

            // ── Recent completed / failed ────────────────────────────────
            egui::CollapsingHeader::new(format!("Recent ({})", self.finished_tasks.len()))
                .default_open(true)
                .show(ui, |ui| {
                    if self.finished_tasks.is_empty() {
                        ui.weak("No completed tasks yet.");
                    } else {
                        ui.push_id("bg_recent", |ui| {
                            let mut tb = TableBuilder::new(ui)
                                .id_salt(GridTableId::BgRecent.key())
                                .striped(true)
                                .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                            if bg_recent_reset {
                                tb.reset();
                            }
                            for &i in &bg_recent_order {
                                let c = &BG_RECENT_COLUMNS[i];
                                let col = if c.stretch { Column::remainder().clip(true) } else { Column::auto() };
                                tb = tb.column(col);
                            }
                            tb.header(20.0, |mut h| {
                                for &i in &bg_recent_order {
                                    let c = &BG_RECENT_COLUMNS[i];
                                    h.col(|ui| {
                                        if grid_header_cell_plain(ui, GridTableId::BgRecent, c, &mut bg_recent_entries, &BG_RECENT_COLUMNS) {
                                            self.reorder_columns = Some(ReorderColumnsState {
                                                table: GridTableId::BgRecent,
                                                draft: bg_recent_entries.clone(),
                                            });
                                        }
                                    });
                                }
                            })
                            .body(|mut body| {
                                for (task, outcome, finished_at) in &self.finished_tasks {
                                    let dur = fmt_duration_secs(finished_at - task.started_at);
                                    body.row(20.0, |mut row| {
                                        for &i in &bg_recent_order {
                                            row.col(|ui| match BG_RECENT_COLUMNS[i].id {
                                                "channel" => { ui.label(&task.label); }
                                                "task" => { ui.label(task.kind.label()); }
                                                "detail" => {
                                                    ui.label(&task.detail);
                                                    if let crate::events::BackgroundTaskKind::Chapters(rec_id) = task.kind
                                                        && ui
                                                            .small_button("ℹ")
                                                            .on_hover_text(
                                                                "Which stream, which file, and which \
                                                                 chapters at which timestamp.",
                                                            )
                                                            .clicked()
                                                        && !self.chapters_popups.contains(&rec_id)
                                                    {
                                                        self.chapters_popups.push(rec_id);
                                                    }
                                                }
                                                "outcome" => {
                                                    match outcome {
                                                        crate::events::TaskOutcome::Completed => {
                                                            ui.label(format!("✔ OK ({dur})"));
                                                        }
                                                        crate::events::TaskOutcome::CompletedWithNote(note) => {
                                                            // "0 events" is a soft-warn (OCR ran but found
                                                            // nothing); anything else is a normal success.
                                                            let zero = note.starts_with("0 ");
                                                            let text = format!("{} ({dur})", note);
                                                            if zero {
                                                                ui.colored_label(
                                                                    egui::Color32::from_rgb(200, 160, 50),
                                                                    format!("⚠ {text}"),
                                                                );
                                                            } else {
                                                                ui.colored_label(
                                                                    egui::Color32::from_rgb(80, 200, 120),
                                                                    format!("✔ {text}"),
                                                                );
                                                            }
                                                        }
                                                        crate::events::TaskOutcome::Failed(e) => {
                                                            ui.colored_label(
                                                                egui::Color32::from_rgb(220, 80, 80),
                                                                format!("✘ {e}"),
                                                            );
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            });
                                        }
                                    });
                                }
                            });
                        });
                    }
                });

            ui.add_space(8.0);
        });

        // Persist any toggle changes (after the closure releases its borrows).
        for ((_, key, en), was) in toggles.iter().zip(before.iter()) {
            if en != was {
                self.job_toggles.insert((*key).to_string(), *en);
                let _ = self.core.store.set_setting(key, if *en { "1" } else { "0" });
            }
        }
        if bg_active_entries != self.bg_active_grid.entries {
            self.bg_active_grid.entries = bg_active_entries;
            grid_columns::save_columns(&self.core.store, GridTableId::BgActive, &self.bg_active_grid.entries);
        }
        if bg_recent_entries != self.bg_recent_grid.entries {
            self.bg_recent_grid.entries = bg_recent_entries;
            grid_columns::save_columns(&self.core.store, GridTableId::BgRecent, &self.bg_recent_grid.entries);
        }
    }

    /// Reload every grid table's in-memory column entries from the store —
    /// used after the Settings "Reset all columns" / "Reset all column
    /// positions" buttons write new values directly to the store, so the
    /// running app reflects the reset immediately rather than waiting for
    /// each table's own next save-on-change cycle.
    pub(super) fn reload_all_grid_entries(&mut self) {
        self.streams_grid.entries =
            grid_columns::load_columns(&self.core.store, GridTableId::Streams, &STREAM_COLUMNS);
        self.videos_grid.entries =
            grid_columns::load_columns(&self.core.store, GridTableId::Videos, &VIDEO_COLUMNS);
        self.bg_active_grid.entries =
            grid_columns::load_columns(&self.core.store, GridTableId::BgActive, &BG_ACTIVE_COLUMNS);
        self.bg_recent_grid.entries =
            grid_columns::load_columns(&self.core.store, GridTableId::BgRecent, &BG_RECENT_COLUMNS);
        self.processes_grid.entries =
            grid_columns::load_columns(&self.core.store, GridTableId::Processes, &PROCESSES_COLUMNS);
        self.issues_grid.entries =
            grid_columns::load_columns(&self.core.store, GridTableId::Issues, &ISSUES_COLUMNS);
    }
    pub(super) fn stats_view(&mut self, ui: &mut egui::Ui) {
        use crate::schedule_ocr::load_ocr_stats;

        // Load on first render of this tab; also re-loadable via the Refresh button.
        if self.stats_snapshot.is_none() {
            let ocr = load_ocr_stats(self.core.store.as_ref());
            let global = self.core.store.global_stats().unwrap_or_default();
            let poll = crate::scheduler::load_poll_stats(self.core.store.as_ref());
            self.stats_snapshot = Some((ocr, global, poll));
            self.stats_capture_health = Some((
                self.core.store.alert_daily_stats(30).unwrap_or_default(),
                self.core.store.alert_health_totals().unwrap_or_default(),
            ));
            self.stats_recordings_daily =
                Some(self.core.store.recordings_daily_stats().unwrap_or_default());
        }
        let (ocr, global, poll) = match self.stats_snapshot.clone() {
            Some(s) => s,
            None => (OcrStats::default(), GlobalStats::default(), PollStats::default()),
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(8.0);

            // ── Claude OCR ───────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.heading("Claude OCR");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳  Refresh").clicked() {
                        self.stats_snapshot = None;
                        self.stats_history = None;
                    }
                    if ui.button("🗑  Reset").on_hover_text("Clear all accumulated OCR stats").clicked() {
                        let _ = self.core.store.set_setting(K_OCR_STATS, "{}");
                        self.stats_snapshot = None;
                    }
                });
            });
            ui.separator();

            egui::Grid::new("ocr_stats_grid")
                .num_columns(4)
                .spacing([32.0, 6.0])
                .show(ui, |ui| {
                    let total_calls = ocr.calls + ocr.cli_failures + ocr.parse_failures;

                    ui.label("Total invocations");
                    ui.strong(format!("{total_calls}"));
                    ui.label("Cache hits (skipped)");
                    ui.strong(format!("{}", ocr.cache_hits));
                    ui.end_row();

                    ui.label("Successful calls");
                    ui.strong(format!("{}", ocr.calls));
                    ui.label("CLI failures");
                    ui.strong(format!("{}", ocr.cli_failures));
                    ui.end_row();

                    ui.label("Parse failures");
                    ui.strong(format!("{}", ocr.parse_failures));
                    ui.label("Last call");
                    ui.strong(match ocr.last_call_at {
                        Some(t) => {
                            use chrono::{Local, TimeZone};
                            Local.timestamp_opt(t, 0)
                                .single()
                                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| "—".into())
                        }
                        None => "Never".into(),
                    });
                    ui.end_row();
                });

            ui.add_space(8.0);

            // Token / cost row
            egui::Grid::new("ocr_token_grid")
                .num_columns(4)
                .spacing([32.0, 6.0])
                .show(ui, |ui| {
                    let fmt_n = |n: u64| -> String {
                        // simple thousands-separator formatting
                        let s = n.to_string();
                        let mut out = String::new();
                        for (i, c) in s.chars().rev().enumerate() {
                            if i > 0 && i % 3 == 0 { out.push(','); }
                            out.push(c);
                        }
                        out.chars().rev().collect()
                    };

                    ui.label("Input tokens");
                    ui.strong(fmt_n(ocr.input_tokens));
                    ui.label("Output tokens");
                    ui.strong(fmt_n(ocr.output_tokens));
                    ui.end_row();

                    ui.label("Cache-read tokens");
                    ui.strong(fmt_n(ocr.cache_read_tokens));
                    ui.label("Cache-create tokens");
                    ui.strong(fmt_n(ocr.cache_creation_tokens));
                    ui.end_row();

                    ui.label("Total cost");
                    ui.strong(format!("${:.4}", ocr.cost_usd));
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                });

            // Per-model breakdown table
            if !ocr.by_model.is_empty() {
                ui.add_space(10.0);
                ui.label("Per model:");
                ui.add_space(4.0);

                let mut models: Vec<_> = ocr.by_model.iter().collect();
                models.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));

                egui::Grid::new("ocr_model_grid")
                    .num_columns(5)
                    .spacing([24.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("Model");
                        ui.strong("Calls");
                        ui.strong("Input tok");
                        ui.strong("Output tok");
                        ui.strong("Cost");
                        ui.end_row();

                        let fmt_n = |n: u64| -> String {
                            let s = n.to_string();
                            let mut out = String::new();
                            for (i, c) in s.chars().rev().enumerate() {
                                if i > 0 && i % 3 == 0 { out.push(','); }
                                out.push(c);
                            }
                            out.chars().rev().collect()
                        };

                        for (model, m) in &models {
                            ui.label(model.as_str());
                            ui.label(m.calls.to_string());
                            ui.label(fmt_n(m.input_tokens));
                            ui.label(fmt_n(m.output_tokens));
                            ui.label(format!("${:.4}", m.cost_usd));
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(16.0);

            // ── YouTube Data API quota ────────────────────────────────────────
            ui.heading("YouTube Data API");
            ui.separator();
            {
                let quota_today = self.yt_quota_today;
                let cutoff = self.yt_quota_cutoff;
                let search_today = self.yt_search_today;
                let search_cutoff = self.yt_search_cutoff;
                egui::Grid::new("quota_grid")
                    .num_columns(4)
                    .spacing([32.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Units used today");
                        ui.strong(format!("{quota_today}"));
                        ui.label("Units cutoff");
                        ui.strong(format!("{cutoff}"));
                        ui.end_row();
                        ui.label("search.list calls today");
                        ui.strong(format!("{search_today}"));
                        ui.label("Search cutoff");
                        ui.strong(format!("{search_cutoff}"));
                        ui.end_row();
                    });
                let frac = (quota_today as f32 / cutoff as f32).clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!("{quota_today} / {cutoff} units")),
                );
                let search_frac = (search_today as f32 / search_cutoff.max(1) as f32).clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(search_frac)
                        .text(format!("{search_today} / {search_cutoff} search queries")),
                );

                ui.add_space(6.0);
                let ep_search = self.yt_ep_search_today;
                let ep_videos = self.yt_ep_videos_today;
                let ep_channels = self.yt_ep_channels_today;
                ui.label(egui::RichText::new("Units spent by call type today").strong())
                    .on_hover_text(
                        "Where the total above is actually going. search.list costs 100 \
                         units/call (by far the most expensive) — videos.list and \
                         channels.list cost 1 unit/call each. A monitor added by @handle \
                         (rather than a /channel/UC… URL) pays an extra channels.list call \
                         on every poll to resolve the handle, on top of the poll's own \
                         search.list/videos.list calls.",
                    );
                egui::Grid::new("quota_breakdown_grid")
                    .num_columns(6)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("search.list");
                        ui.strong(format!("{ep_search}")).on_hover_text(
                            "Live-detection polls and the upcoming-schedule refresh — 100 units/call.",
                        );
                        ui.label("videos.list");
                        ui.strong(format!("{ep_videos}")).on_hover_text(
                            "Title/scheduled-start/actual-start lookups by video id — 1 unit/call.",
                        );
                        ui.label("channels.list");
                        ui.strong(format!("{ep_channels}")).on_hover_text(
                            "Handle-to-channel-id resolution for @handle URLs — 1 unit/call.",
                        );
                        ui.end_row();
                    });
            }

            ui.add_space(16.0);

            // ── Detection / API requests ────────────────────────────────────
            // Per-platform poll/detect request health (all detection methods —
            // Twitch Helix, WebSub/scrape fallback, YouTube/Kick API, generic
            // probe) so recurring instability (auth failures, DNS/network
            // blips, rate limiting) is visible here instead of only in the log.
            ui.horizontal(|ui| {
                ui.heading("Detection / API requests");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("🗑  Reset")
                        .on_hover_text("Clear all accumulated request stats")
                        .clicked()
                    {
                        let _ = self.core.store.set_setting(crate::models::K_POLL_STATS, "{}");
                        let _ = self.core.store.clear_poll_history();
                        self.stats_snapshot = None;
                        self.stats_history = None;
                    }
                });
            });
            ui.separator();

            egui::Grid::new("poll_stats_grid")
                .num_columns(4)
                .spacing([24.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Platform");
                    ui.strong("Polls");
                    ui.strong("Errors");
                    ui.strong("Last error");
                    ui.end_row();

                    for p in Platform::ALL {
                        let s = poll.by_platform.get(p.as_str()).cloned().unwrap_or_default();
                        if s.polls == 0 {
                            continue; // never polled this platform — nothing to show
                        }
                        ui.label(p.label());
                        ui.label(s.polls.to_string());
                        let err_rate = if s.polls > 0 {
                            100.0 * s.errors as f64 / s.polls as f64
                        } else {
                            0.0
                        };
                        let err_text = format!("{} ({err_rate:.1}%)", s.errors);
                        if s.errors > 0 {
                            ui.colored_label(HL_ERROR_TEXT, err_text);
                        } else {
                            ui.label(err_text);
                        }
                        match s.last_error_at {
                            Some(t) => {
                                use chrono::{Local, TimeZone};
                                let when = Local
                                    .timestamp_opt(t, 0)
                                    .single()
                                    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                                    .unwrap_or_else(|| "—".into());
                                ui.label(when).on_hover_text(&s.last_error);
                            }
                            None => {
                                ui.weak("—");
                            }
                        }
                        ui.end_row();
                    }
                });
            if poll.by_platform.values().all(|s| s.polls == 0) {
                ui.weak("No polls recorded yet.");
            }

            // ── Recent errors — the actual failures behind the counters ─────
            let mut recent: Vec<(Platform, crate::models::PollErrorEntry)> = Vec::new();
            for p in Platform::ALL {
                if let Some(s) = poll.by_platform.get(p.as_str()) {
                    recent.extend(s.recent_errors.iter().cloned().map(|e| (p, e)));
                }
            }
            recent.sort_by_key(|(_, e)| std::cmp::Reverse(e.at));
            if !recent.is_empty() {
                ui.add_space(6.0);
                let header = egui::CollapsingHeader::new(format!("⚠ Recent errors ({})", recent.len()))
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("poll_recent_errors_scroll")
                            .max_height(280.0)
                            .show(ui, |ui| {
                                egui::Grid::new("poll_recent_errors_grid")
                                    .num_columns(5)
                                    .spacing([16.0, 3.0])
                                    .striped(true)
                                    .show(ui, |ui| {
                                        ui.strong("Time");
                                        ui.strong("Platform");
                                        ui.strong("Channel");
                                        ui.strong("Method")
                                            .on_hover_text("Which detection method the failed check used (Helix API, Scrape, Probe, …)");
                                        ui.strong("Error");
                                        ui.end_row();
                                        for (p, e) in &recent {
                                            use chrono::{Local, TimeZone};
                                            let when = Local
                                                .timestamp_opt(e.at, 0)
                                                .single()
                                                .map(|dt| dt.format("%m-%d %H:%M:%S").to_string())
                                                .unwrap_or_else(|| "—".into());
                                            ui.label(when);
                                            ui.label(p.label());
                                            ui.label(&e.monitor);
                                            ui.label(&e.method);
                                            // Long details (URLs, HTTP bodies) get truncated
                                            // in the cell; the full text lives on hover.
                                            let short: String = if e.detail.chars().count() > 100 {
                                                let cut: String = e.detail.chars().take(100).collect();
                                                format!("{cut}…")
                                            } else {
                                                e.detail.clone()
                                            };
                                            ui.colored_label(HL_ERROR_TEXT, short)
                                                .on_hover_text(&e.detail);
                                            ui.end_row();
                                        }
                                    });
                            });
                    });
                header.header_response.on_hover_text(format!(
                    "The last {} individual poll/detect failures per platform, newest first \
                     — what the Errors counter above actually counted. \
                     Cleared by the Reset button.",
                    crate::models::MAX_RECENT_POLL_ERRORS
                ));
            }

            // ── History graphs (error rate per platform, volume per method) ─
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("History:").on_hover_text(
                    "Minute-resolution request history is kept for 60 days (poll_history \
                     table); pick how far back the graphs look. Wider spans use wider \
                     buckets so every view stays readable.",
                );
                for s in super::PollSpan::ALL {
                    let resp = ui
                        .selectable_label(self.stats_poll_span == s, s.label())
                        .on_hover_text(format!(
                            "Show the last {} in {} buckets",
                            s.label(),
                            s.bucket_label()
                        ));
                    if resp.clicked() && self.stats_poll_span != s {
                        self.stats_poll_span = s;
                        self.stats_history = None;
                    }
                }
            });
            let span = self.stats_poll_span;
            if self.stats_history.is_none() {
                let since = chrono::Utc::now().timestamp() - span.secs();
                self.stats_history = Some(
                    self.core
                        .store
                        .poll_history(since, span.bucket_secs())
                        .unwrap_or_default(),
                );
            }
            let history = self.stats_history.clone().unwrap_or_default();
            if history.is_empty() {
                ui.weak("No detection history in this timespan yet — it accumulates as monitors poll.");
            } else {
                let now = chrono::Utc::now().timestamp();
                let to_x = |t: i64| (t - now) as f64 / 3600.0; // hours relative to now (negative)
                let days = span.axis_in_days();
                let fmt_x = move |h: f64| {
                    if days { format!("{:+.1}d", h / 24.0) } else { format!("{h:+.1}h") }
                };
                let bucket_label = span.bucket_label();

                ui.add_space(4.0);
                ui.label("Error rate per platform:").on_hover_text(format!(
                    "Failed checks as a percentage of all checks, per platform (all \
                     detection methods folded together), in {bucket_label} buckets. \
                     X axis is time relative to now. Line colors match the platforms' \
                     log-tag brand colors. Periods with no polling at all are bridged \
                     with straight segments.",
                ));
                // platform -> time-ordered (t, polls, errors), methods folded together.
                let mut per_platform: std::collections::BTreeMap<&str, std::collections::BTreeMap<i64, (u64, u64)>> =
                    Default::default();
                for b in &history {
                    let e = per_platform.entry(b.platform.as_str()).or_default().entry(b.t).or_insert((0, 0));
                    e.0 += b.polls;
                    e.1 += b.errors;
                }
                let fx = fmt_x;
                egui_plot::Plot::new("poll_error_rate_plot")
                    .height(160.0)
                    .legend(egui_plot::Legend::default())
                    .allow_scroll(false)
                    .include_y(0.0)
                    .x_axis_formatter(move |mark, _| fx(mark.value))
                    .y_axis_formatter(|mark, _| format!("{:.0}%", mark.value))
                    .label_formatter(move |name, v| format!("{name}\n{}: {:.1}%", fx(v.x), v.y))
                    .show(ui, |plot_ui| {
                        for p in Platform::ALL {
                            let Some(buckets) = per_platform.get(p.as_str()) else { continue };
                            let pts: Vec<[f64; 2]> = buckets
                                .iter()
                                .map(|(t, (polls, errors))| {
                                    [to_x(*t), 100.0 * *errors as f64 / (*polls).max(1) as f64]
                                })
                                .collect();
                            let (r, g, b) = p.tag().rgb();
                            plot_ui.line(
                                egui_plot::Line::new(p.label(), egui_plot::PlotPoints::from(pts))
                                    .color(egui::Color32::from_rgb(r, g, b)),
                            );
                        }
                    });

                ui.add_space(8.0);
                ui.label("Requests per kind:").on_hover_text(format!(
                    "How many checks each detection method (Helix API, Scrape, Probe, \
                     YT API, …) performed per {bucket_label} bucket, all platforms \
                     combined. X axis is time relative to now.",
                ));
                // method -> time-ordered polls, platforms folded together.
                let mut per_method: std::collections::BTreeMap<&str, std::collections::BTreeMap<i64, u64>> =
                    Default::default();
                for b in &history {
                    *per_method.entry(b.method.as_str()).or_default().entry(b.t).or_insert(0) += b.polls;
                }
                egui_plot::Plot::new("poll_method_volume_plot")
                    .height(160.0)
                    .legend(egui_plot::Legend::default())
                    .allow_scroll(false)
                    .include_y(0.0)
                    .x_axis_formatter(move |mark, _| fx(mark.value))
                    .y_axis_formatter(|mark, _| format!("{:.0}", mark.value))
                    .label_formatter(move |name, v| {
                        format!("{name}\n{}: {:.0} requests / {bucket_label}", fx(v.x), v.y)
                    })
                    .show(ui, |plot_ui| {
                        for (method, buckets) in &per_method {
                            let pts: Vec<[f64; 2]> = buckets
                                .iter()
                                .map(|(t, polls)| [to_x(*t), *polls as f64])
                                .collect();
                            plot_ui.line(egui_plot::Line::new(
                                method.to_string(),
                                egui_plot::PlotPoints::from(pts),
                            ));
                        }
                    });
            }

            ui.add_space(16.0);

            // ── Recordings ───────────────────────────────────────────────────
            ui.heading("Recordings");
            ui.separator();

            let fmt_bytes = |b: i64| -> String {
                if b >= 1_000_000_000_000 {
                    format!("{:.1} TB", b as f64 / 1e12)
                } else if b >= 1_000_000_000 {
                    format!("{:.1} GB", b as f64 / 1e9)
                } else if b >= 1_000_000 {
                    format!("{:.1} MB", b as f64 / 1e6)
                } else {
                    format!("{:.1} KB", b as f64 / 1e3)
                }
            };

            egui::Grid::new("global_stats_grid")
                .num_columns(4)
                .spacing([32.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Total recordings");
                    ui.strong(global.total_recordings.to_string());
                    ui.label("Total archived");
                    ui.strong(fmt_bytes(global.total_bytes));
                    ui.end_row();

                    ui.label("Total channels");
                    ui.strong(global.total_channels.to_string());
                    ui.label("Monitors (active)");
                    ui.strong(format!("{} ({} active)", global.total_monitors, global.active_monitors));
                    ui.end_row();

                    ui.label("Upcoming schedule");
                    ui.strong(format!("{} segments", global.upcoming_segments));
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("Breakdown:").on_hover_text(
                    "New recordings started + bytes archived, broken down by period.",
                );
                for p in super::RecordingsPeriod::ALL {
                    if ui.selectable_label(self.recordings_period == p, p.label()).clicked() {
                        self.recordings_period = p;
                    }
                }
            });
            let daily_rec = self.stats_recordings_daily.clone().unwrap_or_default();
            let today = chrono::Utc::now().date_naive();
            match self.recordings_period {
                super::RecordingsPeriod::Day => {
                    egui::Grid::new("recordings_breakdown_day")
                        .num_columns(3)
                        .striped(true)
                        .spacing([24.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("Day");
                            ui.strong("Recordings");
                            ui.strong("Archived");
                            ui.end_row();
                            for (d, count, bytes) in recordings_week_days(&daily_rec, today) {
                                let future = d > today;
                                ui.label(d.format("%a %Y-%m-%d").to_string());
                                if future {
                                    ui.weak("—");
                                    ui.weak("—");
                                } else {
                                    ui.label(count.to_string());
                                    ui.label(fmt_bytes(bytes));
                                }
                                ui.end_row();
                            }
                        });
                }
                super::RecordingsPeriod::Week | super::RecordingsPeriod::Month | super::RecordingsPeriod::Year => {
                    let summaries = match self.recordings_period {
                        super::RecordingsPeriod::Week => recordings_week_summaries(&daily_rec, today),
                        super::RecordingsPeriod::Month => recordings_month_summaries(&daily_rec, today),
                        _ => recordings_year_summaries(&daily_rec, today),
                    };
                    egui::Grid::new("recordings_breakdown_summary")
                        .num_columns(5)
                        .striped(true)
                        .spacing([24.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("");
                            ui.strong("Recordings");
                            ui.strong("Archived");
                            ui.strong("Avg/day (rec.)");
                            ui.strong("Avg/day (archived)");
                            ui.end_row();
                            for s in &summaries {
                                ui.label(s.label);
                                ui.label(s.count.to_string());
                                ui.label(fmt_bytes(s.bytes));
                                ui.label(format!("{:.1}", s.avg_count_per_day));
                                ui.label(fmt_bytes(s.avg_bytes_per_day.round() as i64));
                                ui.end_row();
                            }
                        });
                }
            }

            ui.add_space(16.0);

            // ── Capture health (🚨 Warnings rollup + trend) ──────────────────
            ui.heading("Capture health 🚨").on_hover_text(
                "Rollup of the capture-alert scanner: data loss reported by the \
                 capture tools' own logs (sequence gaps, failed fetches, tool \
                 errors), the VOD gap-recovery outcomes, and a per-day trend so a \
                 degrading disk/network shows up as a pattern instead of isolated \
                 rows. Details live in the 🚨 Warnings window.",
            );
            ui.separator();
            let (daily, totals) = self.stats_capture_health.clone().unwrap_or_default();
            let crate::store::AlertHealthTotals {
                errors: errs,
                warnings: warns,
                lost_segments: lost,
                ranges_total: ranges,
                ranges_done: done,
                muted_segs: muted,
            } = totals;
            if errs == 0 && warns == 0 {
                ui.weak("No capture problems on record — the tools' logs are clean.");
            } else {
                egui::Grid::new("capture_health_grid")
                    .num_columns(4)
                    .spacing([32.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Data-loss / error alerts");
                        ui.strong(crate::models::group_thousands(errs))
                            .on_hover_text("Alerts where content is missing from a capture (sequence gaps, failed fetches, tool errors).");
                        ui.label("Tool warnings");
                        ui.strong(crate::models::group_thousands(warns));
                        ui.end_row();

                        ui.label("Segments lost");
                        ui.strong(format!(
                            "{} (~{})",
                            crate::models::group_thousands(lost),
                            fmt_duration(lost * 2)
                        ))
                        .on_hover_text("Total live segments the capture tools dropped (~2s each on Twitch).");
                        ui.label("Lost ranges recovered");
                        let mark = if ranges > 0 && done == ranges { " ✔" } else { "" };
                        ui.strong(if ranges > 0 {
                            let m = if muted > 0 {
                                format!(" · ✂ {} muted segs", crate::models::group_thousands(muted))
                            } else {
                                String::new()
                            };
                            format!("{done}/{ranges}{mark}{m}")
                        } else {
                            "—".into()
                        })
                        .on_hover_text(
                            "Lost time ranges re-fetched from the Twitch VOD CDN into patch \
                             files. ✂ = segments that only survived as DMCA-muted copies.",
                        );
                        ui.end_row();
                    });
                if !daily.is_empty() {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Per day (last 30, by first occurrence, UTC)").strong())
                        .on_hover_text(
                            "A rising 'lost' column across days = a systemic problem \
                             (saturated disk/uplink, a failing enclosure), not a one-off \
                             stream hiccup.",
                        );
                    egui::Grid::new("capture_health_daily")
                        .num_columns(6)
                        .striped(true)
                        .spacing([24.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("Day");
                            ui.strong("Errors");
                            ui.strong("Warnings");
                            ui.strong("Lost");
                            ui.strong("Recovered");
                            ui.strong("Muted").on_hover_text(
                                "Recovered segments that only survived as DMCA-muted copies.",
                            );
                            ui.end_row();
                            for d in &daily {
                                ui.label(&d.day);
                                ui.label(crate::models::group_thousands(d.errors));
                                ui.label(crate::models::group_thousands(d.warnings));
                                ui.label(if d.lost_segments > 0 {
                                    format!(
                                        "{} segs (~{})",
                                        crate::models::group_thousands(d.lost_segments),
                                        fmt_duration(d.lost_segments * 2)
                                    )
                                } else {
                                    "—".into()
                                });
                                ui.label(if d.ranges_total > 0 {
                                    format!("{}/{}", d.recovered, d.ranges_total)
                                } else {
                                    "—".into()
                                });
                                ui.label(if d.muted > 0 {
                                    crate::models::group_thousands(d.muted)
                                } else {
                                    "—".into()
                                });
                                ui.end_row();
                            }
                        });
                }
            }

            ui.add_space(8.0);
            // (The 🤝 Collabs partner table lives in the Channel Stats tab —
            // this view is app/system health only.)
        });
    }
}

/// One Recordings-breakdown summary row (e.g. "This week" / "Last month").
struct RecordingsPeriodSummary {
    label: &'static str,
    count: i64,
    bytes: i64,
    avg_count_per_day: f64,
    avg_bytes_per_day: f64,
}

/// Sum of `count`/`bytes` for days in `daily` whose date falls within the
/// inclusive `[start, end]` range. ISO `YYYY-MM-DD` strings sort
/// lexicographically the same as the dates they represent, so no per-row
/// parsing is needed.
fn sum_recording_days(
    daily: &[crate::models::DailyRecordingStat],
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
) -> (i64, i64) {
    let s = start.format("%Y-%m-%d").to_string();
    let e = end.format("%Y-%m-%d").to_string();
    daily
        .iter()
        .filter(|d| d.day.as_str() >= s.as_str() && d.day.as_str() <= e.as_str())
        .fold((0i64, 0i64), |(c, b), d| (c + d.count, b + d.bytes))
}

fn recording_period_summary(
    label: &'static str,
    (count, bytes): (i64, i64),
    days: i64,
) -> RecordingsPeriodSummary {
    let days = days.max(1) as f64;
    RecordingsPeriodSummary {
        label,
        count,
        bytes,
        avg_count_per_day: count as f64 / days,
        avg_bytes_per_day: bytes as f64 / days,
    }
}

fn recording_month_start(d: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    chrono::NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

fn recording_year_start(d: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    chrono::NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap_or(d)
}

/// "This week"/"Last week" (Monday-start). "This week" averages over the
/// days elapsed so far, not a flat 7 — a Tuesday's daily average shouldn't be
/// dragged down by 5 days that haven't happened yet.
fn recordings_week_summaries(
    daily: &[crate::models::DailyRecordingStat],
    today: chrono::NaiveDate,
) -> [RecordingsPeriodSummary; 2] {
    let this_start = super::calendar::week_start(today);
    let elapsed = (today - this_start).num_days() + 1;
    let last_start = this_start - chrono::Duration::days(7);
    let last_end = this_start - chrono::Duration::days(1);
    [
        recording_period_summary("This week", sum_recording_days(daily, this_start, today), elapsed),
        recording_period_summary("Last week", sum_recording_days(daily, last_start, last_end), 7),
    ]
}

/// "This month"/"Last month", same partial-period averaging as the week version.
fn recordings_month_summaries(
    daily: &[crate::models::DailyRecordingStat],
    today: chrono::NaiveDate,
) -> [RecordingsPeriodSummary; 2] {
    let this_start = recording_month_start(today);
    let elapsed = (today - this_start).num_days() + 1;
    let last_end = this_start - chrono::Duration::days(1);
    let last_start = recording_month_start(last_end);
    let last_days = (last_end - last_start).num_days() + 1;
    [
        recording_period_summary("This month", sum_recording_days(daily, this_start, today), elapsed),
        recording_period_summary("Last month", sum_recording_days(daily, last_start, last_end), last_days),
    ]
}

/// "This year"/"Last year", same partial-period averaging as week/month.
fn recordings_year_summaries(
    daily: &[crate::models::DailyRecordingStat],
    today: chrono::NaiveDate,
) -> [RecordingsPeriodSummary; 2] {
    use chrono::Datelike;
    let this_start = recording_year_start(today);
    let elapsed = (today - this_start).num_days() + 1;
    let last_start = chrono::NaiveDate::from_ymd_opt(today.year() - 1, 1, 1).unwrap_or(this_start);
    let last_end = this_start - chrono::Duration::days(1);
    let last_days = (last_end - last_start).num_days() + 1;
    [
        recording_period_summary("This year", sum_recording_days(daily, this_start, today), elapsed),
        recording_period_summary("Last year", sum_recording_days(daily, last_start, last_end), last_days),
    ]
}

/// The 7 days (Monday..Sunday) of the calendar week containing `today`, each
/// paired with that day's recording count/bytes (0 for days with none,
/// including days later in the week that haven't happened yet).
fn recordings_week_days(
    daily: &[crate::models::DailyRecordingStat],
    today: chrono::NaiveDate,
) -> Vec<(chrono::NaiveDate, i64, i64)> {
    let start = super::calendar::week_start(today);
    (0..7)
        .map(|i| {
            let d = start + chrono::Duration::days(i);
            let ds = d.format("%Y-%m-%d").to_string();
            let (count, bytes) = daily
                .iter()
                .find(|r| r.day == ds)
                .map(|r| (r.count, r.bytes))
                .unwrap_or((0, 0));
            (d, count, bytes)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DailyRecordingStat;

    fn day(day: &str, count: i64, bytes: i64) -> DailyRecordingStat {
        DailyRecordingStat { day: day.into(), count, bytes }
    }
    fn ymd(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn week_days_covers_monday_to_sunday_and_fills_gaps() {
        // Wednesday 2026-07-22 anchors the week of Mon 2026-07-20..Sun 2026-07-26.
        let daily = vec![day("2026-07-20", 2, 200), day("2026-07-22", 1, 100)];
        let rows = recordings_week_days(&daily, ymd(2026, 7, 22));
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0].0, ymd(2026, 7, 20));
        assert_eq!((rows[0].1, rows[0].2), (2, 200));
        assert_eq!((rows[1].1, rows[1].2), (0, 0)); // Tue: no data
        assert_eq!((rows[2].1, rows[2].2), (1, 100)); // Wed
        assert_eq!(rows[6].0, ymd(2026, 7, 26));
    }

    #[test]
    fn week_summaries_split_this_vs_last_and_average_partial_week() {
        // Today = Wed 2026-07-22 → this week started Mon 2026-07-20, 3 days elapsed.
        let daily = vec![
            day("2026-07-13", 1, 100), // last week (Mon)
            day("2026-07-19", 1, 100), // last week (Sun)
            day("2026-07-20", 2, 200), // this week (Mon)
            day("2026-07-22", 2, 200), // this week (Wed, today)
        ];
        let [this_week, last_week] = recordings_week_summaries(&daily, ymd(2026, 7, 22));
        assert_eq!(this_week.label, "This week");
        assert_eq!(this_week.count, 4);
        assert_eq!(this_week.bytes, 400);
        assert!((this_week.avg_count_per_day - 4.0 / 3.0).abs() < 1e-9);
        assert_eq!(last_week.label, "Last week");
        assert_eq!(last_week.count, 2);
        assert!((last_week.avg_count_per_day - 2.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn week_summaries_exclude_days_outside_either_week() {
        let daily = vec![day("2026-07-06", 9, 900)]; // two weeks before
        let [this_week, last_week] = recordings_week_summaries(&daily, ymd(2026, 7, 22));
        assert_eq!(this_week.count, 0);
        assert_eq!(last_week.count, 0);
    }

    #[test]
    fn month_summaries_split_this_vs_last_calendar_month() {
        // Today = 2026-07-22 → this month started 2026-07-01 (22 days elapsed).
        let daily = vec![
            day("2026-06-15", 3, 300),
            day("2026-07-01", 1, 100),
            day("2026-07-22", 1, 100),
        ];
        let [this_month, last_month] = recordings_month_summaries(&daily, ymd(2026, 7, 22));
        assert_eq!(this_month.count, 2);
        assert_eq!(last_month.count, 3);
        assert!((this_month.avg_count_per_day - 2.0 / 22.0).abs() < 1e-9);
        // June 2026 has 30 days.
        assert!((last_month.avg_count_per_day - 3.0 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn month_summaries_handle_january_rollover_to_prior_december() {
        let daily = vec![day("2025-12-31", 5, 500), day("2026-01-01", 2, 200)];
        let [this_month, last_month] = recordings_month_summaries(&daily, ymd(2026, 1, 1));
        assert_eq!(this_month.count, 2);
        assert_eq!(last_month.count, 5);
        // December has 31 days.
        assert!((last_month.avg_count_per_day - 5.0 / 31.0).abs() < 1e-9);
    }

    #[test]
    fn year_summaries_split_this_vs_last_calendar_year() {
        let daily = vec![
            day("2025-03-01", 4, 400),
            day("2026-01-01", 1, 100),
            day("2026-07-22", 1, 100),
        ];
        let [this_year, last_year] = recordings_year_summaries(&daily, ymd(2026, 7, 22));
        assert_eq!(this_year.count, 2);
        assert_eq!(last_year.count, 4);
        // 2026-01-01 through 2026-07-22 inclusive = 203 days.
        assert!((this_year.avg_count_per_day - 2.0 / 203.0).abs() < 1e-9);
        // 2025 is not a leap year → 365 days.
        assert!((last_year.avg_count_per_day - 4.0 / 365.0).abs() < 1e-9);
    }
}
