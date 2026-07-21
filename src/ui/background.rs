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
            ui.strong("Scheduled");
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
                ui.strong("Planned");
                ui.add_space(4.0);
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
                ui.add_space(12.0);
            }

            // ── Active tasks ─────────────────────────────────────────────
            ui.strong("Active");
            // Live disk-gate status: bulk passes (remux/merge/concat/embed)
            // run one at a time against the recordings drive — this is the
            // authoritative "what is actually running right now, and how many
            // are queued behind it" line for those jobs.
            {
                let (holders, waiting) = crate::io_gate::local_gate_status();
                if !holders.is_empty() || waiting > 0 {
                    let resp = ui
                        .horizontal(|ui| {
                            ui.label("🖴 Disk gate:");
                            match holders.first() {
                                Some((label, held)) => {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        format!("{label} — running {}", fmt_duration(*held as i64)),
                                    );
                                    if holders.len() > 1 {
                                        let others: String = holders[1..]
                                            .iter()
                                            .map(|(l, h)| {
                                                format!("{l} — running {}", fmt_duration(*h as i64))
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        ui.weak(format!("(+{} more)", holders.len() - 1))
                                            .on_hover_text(format!(
                                                "Other passes ALSO running right now (each holds \
                                                 its own disk-gate permit — another drive, or \
                                                 this drive allows more than one). Only the \
                                                 longest-running one is shown on the line.\n\n{others}"
                                            ));
                                    }
                                }
                                None => {
                                    ui.weak("turning over…");
                                }
                            }
                            if waiting > 0 {
                                ui.weak(format!("· {waiting} queued"));
                                let toggle =
                                    if self.bg_show_gate_queue { "▼ Hide queue" } else { "▶ View queue" };
                                if ui.small_button(toggle).clicked() {
                                    self.bg_show_gate_queue = !self.bg_show_gate_queue;
                                }
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
                    // The queue itself: every pass waiting for a gate, in line
                    // order (per drive) — includes passes that have no task row
                    // of their own (batch re-remux items, embeds, head joins).
                    if self.bg_show_gate_queue && waiting > 0 {
                        for (i, (label, drive, secs)) in
                            crate::io_gate::local_gate_queue().into_iter().enumerate()
                        {
                            ui.horizontal(|ui| {
                                ui.add_space(24.0);
                                ui.weak(format!(
                                    "{}. {label} [{drive}:] — waiting {}",
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

            ui.add_space(12.0);

            // ── Recent completed / failed ────────────────────────────────
            ui.strong("Recent");
            ui.add_space(4.0);

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
                                        "detail" => { ui.label(&task.detail); }
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
                let search_frac = (search_today as f32 / 100.0_f32).clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(search_frac)
                        .text(format!("{search_today} / 100 search queries")),
                );
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

            ui.add_space(8.0);
            // (The 🤝 Collabs partner table lives in the Channel Stats tab —
            // this view is app/system health only.)
        });
    }
}
