//! Schedule view: month/week/day/agenda grids, segment editing, sources,
//! scheduled recordings.

use super::*;

/// The Schedule tab's calendar granularity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScheduleMode {
    Month,
    Week,
    Day,
    Agenda,
}

/// Setting key for the "Compact" calendar toggle (events collapse to a
/// one-line chip at their start time in the Week/Day views).
pub(super) const K_SCHEDULE_COMPACT: &str = "schedule_compact_events";
/// Local timestamp in the active [`DateFmt`] (empty if unset). Used for the
/// Polled / Went Live / Started On columns and the history tree.
/// A clickable row in the "Schedule sources" dialog's Available column: the
/// source label plus a `⚠` badge for risky sources. Returns the click response.
pub(super) fn source_row(ui: &mut egui::Ui, kind: ScheduleSourceKind, selected: bool) -> egui::Response {
    let mut text = kind.label().to_string();
    if kind.risky() {
        text.push_str("  ⚠");
    }
    ui.selectable_label(selected, text)
        .on_hover_text(kind.description())
}

/// Like [`source_row`] but prefixed with the 1-based priority rank, for the
/// Enabled column.
pub(super) fn source_row_ranked(
    ui: &mut egui::Ui,
    rank: usize,
    kind: ScheduleSourceKind,
    selected: bool,
) -> egui::Response {
    let mut text = format!("{rank}.  {}", kind.label());
    if kind.risky() {
        text.push_str("  ⚠");
    }
    ui.selectable_label(selected, text)
        .on_hover_text(kind.description())
}

/// Compact, embeddable schedule-source list editor: one row per source in
/// priority order with an enable checkbox and ▲/▼ reorder buttons. Used by the
/// per-channel / per-instance scope override (the big two-column dialog stays for
/// the global order). Returns true if `entries` changed. Unknown ids are skipped
/// but kept in place.
pub(super) fn source_list_inline_editor(ui: &mut egui::Ui, entries: &mut [SourceEntry]) -> bool {
    let mut changed = false;
    let mut move_up: Option<usize> = None;
    let mut move_down: Option<usize> = None;
    let n = entries.len();
    egui::Frame::group(ui.style()).show(ui, |ui| {
        for (i, entry) in entries.iter_mut().enumerate() {
            let Some(kind) = entry.kind() else { continue };
            ui.horizontal(|ui| {
                if ui.checkbox(&mut entry.enabled, "").changed() {
                    changed = true;
                }
                if ui
                    .add_enabled(i > 0, egui::Button::new("▲").small())
                    .on_hover_text("Higher priority")
                    .clicked()
                {
                    move_up = Some(i);
                }
                if ui
                    .add_enabled(i + 1 < n, egui::Button::new("▼").small())
                    .on_hover_text("Lower priority")
                    .clicked()
                {
                    move_down = Some(i);
                }
                let mut label = egui::RichText::new(kind.label());
                if !entry.enabled {
                    label = label.weak();
                }
                ui.label(label).on_hover_text(kind.description());
                if kind.risky() {
                    ui.colored_label(egui::Color32::from_rgb(0xe0, 0x6c, 0x6c), "⚠");
                }
            });
        }
    });
    if let Some(i) = move_up {
        entries.swap(i, i - 1);
        changed = true;
    }
    if let Some(i) = move_down {
        entries.swap(i, i + 1);
        changed = true;
    }
    changed
}
/// Human-readable recurrence summary for the scheduled-recordings management
/// window, e.g. "Once — 2026-07-10 18:00" or "Every Mon/Wed/Fri 20:00 until
/// 2026-08-01".
pub(super) fn describe_recurrence(r: &ScheduledRecording) -> String {
    match r.kind {
        RecurrenceKind::Once => match r.start_at {
            Some(t) => format!("Once — {}", fmt_datetime_short(t)),
            None => "Once".to_string(),
        },
        RecurrenceKind::Weekly => {
            let bits = r.days_of_week.unwrap_or(0);
            let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let days: Vec<&str> = names
                .iter()
                .enumerate()
                .filter(|(i, _)| bits & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect();
            let days_s = if days.is_empty() { "no days".to_string() } else { days.join("/") };
            let time_s = split_time_of_day(r.time_of_day_secs.unwrap_or(0));
            let mut s = format!("Every {days_s} {time_s}");
            if let Some(u) = r.until {
                s.push_str(&format!(" until {}", fmt_datetime_short(u)));
            }
            s
        }
    }
}

/// Build a filename preview for the Format Designer using real monitor/recording
/// data. Media info is synthetic (1920×1080/60fps/h264/aac) since probing requires
/// async work the UI thread doesn't do. Extension is NOT included.
pub(super) fn build_preview_filename(
    monitor: &MonitorWithChannel,
    recording: Option<&Recording>,
    template: &str,
) -> String {
    let ch_name = monitor.channel.name.as_str();
    let platform_s = monitor.monitor.platform().as_str().to_string();
    let tool_s = monitor.monitor.tool.label().to_string();
    let quality_s = monitor.monitor.quality.clone();
    let take_s = (monitor.recording_count + 1).to_string();
    let mode_s = match monitor.monitor.tool {
        Tool::Streamlink => "live".to_string(),
        Tool::Ffmpeg => "direct".to_string(),
        Tool::YtDlp => {
            if monitor.monitor.platform() == Platform::YouTube {
                // YouTube live is SABR-only now — both from-start and live-edge
                // capture go through the SABR path when the dev build is
                // configured (see downloader::sabr_selected). "dash" only ever
                // applies as a cosmetic fallback when SABR isn't set up.
                "sabr".to_string()
            } else {
                "live".to_string()
            }
        }
    };
    let (started_at, went_live, stream_id_s, title_s, games_s) = match recording {
        Some(r) => (
            r.started_at,
            r.went_live_at.unwrap_or(0),
            r.stream_id.clone().unwrap_or_default(),
            r.title.clone(),
            r.category.clone(),
        ),
        None => (now_unix(), 0i64, String::new(), "Stream Title".to_string(), "Sample Game".to_string()),
    };
    let vars = crate::downloader::TemplateVars {
        name: ch_name,
        title: &title_s,
        channel: ch_name,
        video_id: &stream_id_s,
        quality: &quality_s,
        resolution: "1920x1080",
        height: "1080",
        width: "1920",
        fps: "60",
        vcodec: "h264",
        acodec: "aac",
        tool: &tool_s,
        mode: &mode_s,
        platform: &platform_s,
        take: &take_s,
        games: &games_s,
        secs: started_at,
        went_live,
        // None = the global (settings-configured) token style — previews
        // match what a real capture would be named.
        style: None,
    };
    crate::downloader::preview_filename(template, &vars)
}

impl StreamArchiverApp {
    /// Render every open upcoming-streams window (one per monitor).
    pub(super) fn schedule_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for i in 0..self.schedule_popups.len() {
            let mid = self.schedule_popups[i];
            if self.schedule_popup_window(ctx, mid) {
                closed.push(mid);
            }
        }
        if !closed.is_empty() {
            self.schedule_popups.retain(|m| !closed.contains(m));
        }
    }

    /// Window listing a monitor's upcoming scheduled streams (datetime — title).
    /// Opened by double-clicking a Next stream cell. Returns true on close.
    #[allow(deprecated)]
    pub(super) fn schedule_popup_window(&mut self, ctx: &egui::Context, mid: i64) -> bool {
        if !self.schedule_cache.contains_key(&mid) {
            let v = self
                .core
                .store
                .schedule_for_monitor(mid, now_unix())
                .unwrap_or_default();
            self.schedule_cache.insert(mid, v);
        }
        let segs = self.schedule_cache.get(&mid).cloned().unwrap_or_default();
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("upcoming_streams_vp", mid)),
            egui::ViewportBuilder::default()
                .with_title("Upcoming streams")
                .with_inner_size([460.0, 280.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if segs.is_empty() {
                        ui.label("No upcoming scheduled streams.");
                        return;
                    }
                    ui.label(format!("{} upcoming scheduled stream(s).", segs.len()));
                    ui.add_space(6.0);
                    let lines: Vec<String> = segs
                        .iter()
                        .map(|s| {
                            let when = fmt_datetime_short(s.start_time);
                            if s.category.is_empty() {
                                format!("{when}  —  {}", s.title)
                            } else {
                                format!("{when}  —  {}  ({})", s.title, s.category)
                            }
                        })
                        .collect();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for l in &lines {
                                ui.label(egui::RichText::new(l).monospace());
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

    /// Load the persisted source order into the draft and open the dialog.
    pub(super) fn open_schedule_sources(&mut self) {
        self.schedule_sources_draft = load_source_order(&self.core.store);
        self.schedule_sources_selected = None;
        self.show_schedule_sources = true;
    }

    /// The "Schedule sources" dialog: two columns (Available / Enabled) with →/←
    /// transfer and ▲/▼ priority reordering, mirroring the user's mockup. Every
    /// change persists the order and asks the refresher to re-walk the sources.
    #[allow(deprecated)]
    pub(super) fn schedule_sources_window(&mut self, ctx: &egui::Context) {
        if !self.show_schedule_sources {
            return;
        }
        let mut open = true;
        // Actions collected during the (borrowing) render, applied afterwards.
        let mut enable_id: Option<String> = None;
        let mut disable_id: Option<String> = None;
        let mut move_up_id: Option<String> = None;
        let mut move_down_id: Option<String> = None;
        let mut select_id: Option<String> = None;

        // Filtered, draft-ordered views of the two columns: (draft index, kind).
        let enabled: Vec<(usize, ScheduleSourceKind)> = self
            .schedule_sources_draft
            .iter()
            .enumerate()
            .filter(|(_, e)| e.enabled)
            .filter_map(|(i, e)| e.kind().map(|k| (i, k)))
            .collect();
        let available: Vec<(usize, ScheduleSourceKind)> = self
            .schedule_sources_draft
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.enabled)
            .filter_map(|(i, e)| e.kind().map(|k| (i, k)))
            .collect();
        let selected = self.schedule_sources_selected.clone();
        let sel_in_enabled = enabled
            .iter()
            .any(|(i, _)| Some(&self.schedule_sources_draft[*i].id) == selected.as_ref());
        let sel_in_available = available
            .iter()
            .any(|(i, _)| Some(&self.schedule_sources_draft[*i].id) == selected.as_ref());
        let sel_pos = enabled
            .iter()
            .position(|(i, _)| Some(&self.schedule_sources_draft[*i].id) == selected.as_ref());

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("schedule_sources_vp"),
            egui::ViewportBuilder::default()
                .with_title("Schedule sources")
                .with_inner_size([600.0, 440.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(
                        "Sources are tried top-to-bottom per channel; the first one to resolve \
                         an actual schedule wins. Move sources between the columns and order the \
                         enabled ones by priority.",
                    );
                    ui.add_space(8.0);

                    ui.horizontal_top(|ui| {
                        // ── Available column ──
                        ui.vertical(|ui| {
                            ui.set_width(240.0);
                            ui.strong("Available sources");
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_width(228.0);
                                ui.set_min_height(300.0);
                                if available.is_empty() {
                                    ui.weak("(all sources enabled)");
                                }
                                for (i, kind) in &available {
                                    let id = &self.schedule_sources_draft[*i].id;
                                    let resp = source_row(ui, *kind, selected.as_deref() == Some(id));
                                    if resp.clicked() {
                                        select_id = Some(id.clone());
                                    }
                                    if resp.double_clicked() {
                                        enable_id = Some(id.clone());
                                    }
                                }
                            });
                        });

                        // ── Transfer / reorder buttons ──
                        ui.vertical(|ui| {
                            ui.add_space(48.0);
                            if ui
                                .add_enabled(sel_in_available, egui::Button::new("→"))
                                .on_hover_text("Enable the selected source")
                                .clicked()
                            {
                                enable_id = selected.clone();
                            }
                            if ui
                                .add_enabled(sel_in_enabled, egui::Button::new("←"))
                                .on_hover_text("Disable the selected source")
                                .clicked()
                            {
                                disable_id = selected.clone();
                            }
                            ui.add_space(16.0);
                            if ui
                                .add_enabled(
                                    sel_pos.is_some_and(|p| p > 0),
                                    egui::Button::new("▲"),
                                )
                                .on_hover_text("Higher priority")
                                .clicked()
                            {
                                move_up_id = selected.clone();
                            }
                            if ui
                                .add_enabled(
                                    sel_pos.is_some_and(|p| p + 1 < enabled.len()),
                                    egui::Button::new("▼"),
                                )
                                .on_hover_text("Lower priority")
                                .clicked()
                            {
                                move_down_id = selected.clone();
                            }
                        });

                        // ── Enabled column (priority order) ──
                        ui.vertical(|ui| {
                            ui.set_width(260.0);
                            ui.strong("Enabled sources (priority order)");
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_width(248.0);
                                ui.set_min_height(300.0);
                                if enabled.is_empty() {
                                    ui.weak("(no sources enabled — schedules won't update)");
                                }
                                for (rank, (i, kind)) in enabled.iter().enumerate() {
                                    let id = &self.schedule_sources_draft[*i].id;
                                    let resp = source_row_ranked(
                                        ui,
                                        rank + 1,
                                        *kind,
                                        selected.as_deref() == Some(id),
                                    );
                                    if resp.clicked() {
                                        select_id = Some(id.clone());
                                    }
                                    if resp.double_clicked() {
                                        disable_id = Some(id.clone());
                                    }
                                }
                            });
                        });
                    });

                    ui.add_space(8.0);
                    // Description / risk note for the current selection.
                    if let Some(kind) = selected.as_deref().and_then(ScheduleSourceKind::from_id) {
                        ui.label(kind.description());
                        if kind.risky() {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xe0, 0x6c, 0x6c),
                                "⚠ Risky source — may violate a platform's Terms of Service. \
                                 Use at your own risk.",
                            );
                        }
                    } else {
                        ui.weak("Select a source to see what it does.");
                    }

                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        open = false;
                    }
                });
            },
        );

        // ── Apply collected actions (mutating the draft). ──
        let mut changed = false;
        if let Some(id) = &select_id {
            self.schedule_sources_selected = Some(id.clone());
        }
        if let Some(id) = &enable_id {
            if let Some(e) = self.schedule_sources_draft.iter_mut().find(|e| &e.id == id) {
                e.enabled = true;
                changed = true;
            }
            self.schedule_sources_selected = Some(id.clone());
        }
        if let Some(id) = &disable_id {
            if let Some(e) = self.schedule_sources_draft.iter_mut().find(|e| &e.id == id) {
                e.enabled = false;
                changed = true;
            }
            self.schedule_sources_selected = Some(id.clone());
        }
        // Reorder swaps the selected enabled entry with its enabled neighbour, by
        // swapping their draft slots (disabled entries in between are unaffected).
        if let (Some(id), Some(p)) = (&move_up_id, sel_pos) {
            if p > 0 {
                self.schedule_sources_draft.swap(enabled[p].0, enabled[p - 1].0);
                changed = true;
            }
            self.schedule_sources_selected = Some(id.clone());
        }
        if let (Some(id), Some(p)) = (&move_down_id, sel_pos) {
            if p + 1 < enabled.len() {
                self.schedule_sources_draft.swap(enabled[p].0, enabled[p + 1].0);
                changed = true;
            }
            self.schedule_sources_selected = Some(id.clone());
        }
        if changed {
            if let Err(e) = save_source_order(&self.core.store, &self.schedule_sources_draft) {
                self.status = format!("Error saving schedule sources: {e}");
            } else {
                self.core.request_schedule_refresh();
            }
        }
        if !open {
            self.show_schedule_sources = false;
        }
    }

    /// The Schedule tab: a month/week/day calendar of all upcoming scheduled
    /// streams, with a left sidebar to filter channels and a collision highlight.
    pub(super) fn schedule_view(&mut self, ui: &mut egui::Ui) {

        // Lazy load on first view + initialize the focused date to today.
        if !self.schedule_loaded {
            if self.pending_schedule.is_none() {
                self.spawn_reload_schedule();
            }
            ui.centered_and_justified(|ui| {
                ui.label("Loading schedule…");
            });
            return;
        }
        let anchor = *self
            .schedule_anchor
            .get_or_insert_with(|| chrono::Local::now().date_naive());

        // Empty state: nothing scheduled across any monitor.
        if self.schedule_all.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No upcoming streams scheduled.");
                ui.label(
                    "Schedules come from a channel's published Twitch/YouTube upcoming \
                     streams — channels without one (Twitch returns no segments) show nothing.",
                );
                ui.add_space(8.0);
                if ui.button("⟳  Fetch now").clicked() {
                    self.core.request_schedule_refresh();
                    self.spawn_reload_schedule();
                }
            });
            return;
        }

        // Platform favicons (cheap Arc-backed clone) so the panel closures below can
        // borrow `self` immutably — they only read schedule data.
        let ptex = self
            .platform_tex
            .get_or_insert_with(|| PlatformTextures::load(ui.ctx()))
            .clone();

        // Channel avatars for the week-view header's "recording scheduled" row
        // (schema v51) — built by `schedule_rec_avatars` (mutable) so
        // `schedule_week_grid` (immutable) can just look them up by channel id.
        let sched_rec_avatars = self.schedule_rec_avatars(ui);

        // Shared Streams-list channel colours for every surface below
        // (blocks, chips, stripes, sidebar legend).
        self.rebuild_schedule_chan_colors();

        // Precompute (immutable reads of `self`) what the closures need: collisions
        // and the per-day buckets of visible streams (indices into `schedule_all`).
        let (collide, by_day, all_day_events) = self.schedule_visible_buckets();

        // Actions collected during rendering, applied after the borrowing closures.
        let mut open_day: Option<chrono::NaiveDate> = None;
        let mut nav_anchor: Option<chrono::NaiveDate> = None;
        let mut set_mode: Option<ScheduleMode> = None;
        let mut do_refresh = false;
        let mut open_sources = false;
        let mut clear_hidden = false;
        let mut hide_all = false;
        let mut toggle_channel: Option<i64> = None;
        let mut set_collisions: Option<bool> = None;
        let mut set_show_hidden: Option<bool> = None;

        // ── Left sidebar: per-channel filter + collision toggle. ──
        self.schedule_sidebar(
            ui,
            &ptex,
            &mut clear_hidden,
            &mut hide_all,
            &mut toggle_channel,
            &mut set_show_hidden,
        );

        // Visible date range + header title for the current mode.
        let mode = self.schedule_mode;
        let today = chrono::Local::now().date_naive();
        let (title, prev_date, next_date, collisions_in_view) =
            self.schedule_nav_dates(mode, anchor, &collide);

        // ── Center: the calendar for the selected mode. ──
        egui::CentralPanel::default().show_inside(ui, |ui| {
            // Header: view mode + navigation + title + collision controls.
            self.schedule_toolbar(
                ui,
                mode,
                &title,
                today,
                prev_date,
                next_date,
                collisions_in_view,
                &mut set_mode,
                &mut nav_anchor,
                &mut do_refresh,
                &mut open_sources,
                &mut set_collisions,
            );

            self.schedule_selection_bar(ui);

            // Zoom the calendar body only: scale every relative text size
            // (`.strong()`/`.weak()`/`.small()`/monospace/etc. all resolve
            // through `TextStyle`, so this one override covers virtually all
            // text drawn below without touching each call site). Applied to
            // this `ui` only, after the header/selection-bar above already
            // rendered at normal size, so the toolbar/sidebar are unaffected.
            if (self.schedule_zoom - 1.0).abs() > f32::EPSILON {
                let zoom = self.schedule_zoom;
                for font_id in ui.style_mut().text_styles.values_mut() {
                    font_id.size *= zoom;
                }
            }

            match mode {
                ScheduleMode::Month => {
                    self.schedule_month_grid(ui, anchor, today, &by_day, &collide, &ptex, &mut open_day)
                }
                ScheduleMode::Week => {
                    self.schedule_week_grid(
                        ui, anchor, today, &by_day, &collide, &ptex, &mut open_day,
                        &sched_rec_avatars, &all_day_events,
                    )
                }
                ScheduleMode::Day => {
                    self.schedule_day_grid(ui, anchor, today, &by_day, &collide, &mut open_day)
                }
                ScheduleMode::Agenda => {
                    self.schedule_agenda_view(ui, anchor, &by_day, &collide, &ptex, &mut open_day)
                }
            }
        });

        // ── Apply collected actions. ──
        self.schedule_apply_actions(
            ui,
            set_mode,
            nav_anchor,
            clear_hidden,
            hide_all,
            toggle_channel,
            set_collisions,
            open_day,
            do_refresh,
            open_sources,
            set_show_hidden,
        );
    }

    /// Rebuild the schedule's channel→colour map so every Schedule surface
    /// (event blocks, chips, stripes, sidebar legend) uses the SAME colours
    /// as the Streams list: a manual custom colour wins, else the streamer's
    /// fetched Twitch broadcaster colour (darkened for white-on-block
    /// readability), else the deterministic palette. Twitch colours load
    /// once per channel per session via the same cache the Streams grid uses
    /// (`channel_twitch_colors`).
    fn rebuild_schedule_chan_colors(&mut self) {
        // Collect what each channel needs first (no `self` borrows held), so
        // the Twitch-colour cache below can be filled mutably.
        let mut chans: Vec<(i64, String, String, Option<String>)> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        for r in &self.rows {
            let cid = r.channel.id;
            if !seen.insert(cid) {
                continue;
            }
            let mons: Vec<&MonitorWithChannel> =
                self.rows.iter().filter(|x| x.channel.id == cid).collect();
            let accounts = channel_asset_accounts(&mons);
            let tw = preferred_account_index(&r.channel.preferred_asset, &accounts)
                .filter(|&i| accounts[i].platform == Platform::Twitch)
                .or_else(|| accounts.iter().position(|a| a.platform == Platform::Twitch))
                .map(|i| accounts[i].account.clone());
            chans.push((cid, r.channel.color.clone(), r.channel.name.clone(), tw));
        }
        self.schedule_chan_colors.clear();
        for (cid, custom, name, tw) in chans {
            let color = if !custom.is_empty() {
                channel_event_color(cid, &custom)
            } else if let Some(acct) = tw {
                match *self
                    .channel_twitch_colors
                    .entry(cid)
                    .or_insert_with(|| load_twitch_name_color(&name, &acct))
                {
                    Some(c) => block_safe_color(c),
                    None => channel_event_color(cid, ""),
                }
            } else {
                channel_event_color(cid, "")
            };
            self.schedule_chan_colors.insert(cid, color);
        }
    }

    /// The display colour for a schedule entry — the shared Streams-list
    /// colour when known, else the legacy custom/palette fallback (e.g. a
    /// segment whose monitor was deleted after the schedule was fetched).
    pub(super) fn sched_color(&self, s: &UpcomingStream) -> egui::Color32 {
        self.schedule_chan_colors
            .get(&s.channel_id)
            .copied()
            .unwrap_or_else(|| channel_event_color(s.channel_id, &s.channel_color))
    }

    /// Channel avatars for the week-view header's "recording scheduled" row
    /// (schema v51) — same small-icon cache/pattern as the Streams grid,
    /// built here (mutable) so `schedule_week_grid` (immutable) can just
    /// look them up by channel id.
    fn schedule_rec_avatars(&mut self, ui: &egui::Ui) -> HashMap<i64, egui::TextureHandle> {
        let mut sched_rec_avatars: HashMap<i64, egui::TextureHandle> = HashMap::new();
        let mut channel_ids: Vec<i64> = self
            .scheduled_recordings
            .iter()
            .filter(|r| r.rec.enabled)
            .filter_map(|r| self.rows.iter().find(|row| row.monitor.id == r.rec.monitor_id))
            .map(|row| row.channel.id)
            .collect();
        channel_ids.sort_unstable();
        channel_ids.dedup();
        for cid in channel_ids {
            let Some(channel) = self.rows.iter().find(|r| r.channel.id == cid).map(|r| r.channel.clone())
            else {
                continue;
            };
            let mons: Vec<&MonitorWithChannel> =
                self.rows.iter().filter(|r| r.channel.id == cid).collect();
            let accounts = channel_asset_accounts(&mons);
            let tex = self
                .channel_icons_small
                .entry(cid)
                .or_insert_with(|| resolve_channel_icon_small(&channel, &accounts, ui.ctx()))
                .clone();
            if let Some(t) = tex {
                sched_rec_avatars.insert(cid, t);
            }
        }
        sched_rec_avatars
    }

    /// Precompute (immutable reads of `self`) what the calendar closures
    /// need: the collision set and the per-day buckets of visible streams
    /// (indices into `schedule_all`), plus the "all-day" events for the week
    /// view's bar strip.
    #[allow(clippy::type_complexity)]
    fn schedule_visible_buckets(
        &self,
    ) -> (
        HashSet<usize>,
        HashMap<chrono::NaiveDate, Vec<usize>>,
        Vec<usize>,
    ) {
        let collide: HashSet<usize> = if self.schedule_collisions {
            schedule_collisions(&self.schedule_all, &self.schedule_hidden)
        } else {
            HashSet::new()
        };
        let mut by_day: HashMap<chrono::NaiveDate, Vec<usize>> = HashMap::new();
        // Streams long enough to be treated as "all-day" (see `is_all_day`) —
        // the week view's Google-Calendar-style bar strip renders these
        // instead of (or in addition to) their normal `by_day` chip/block.
        let mut all_day_events: Vec<usize> = Vec::new();
        for (i, s) in self.schedule_all.iter().enumerate() {
            if self.schedule_hidden.contains(&s.channel_id) {
                continue;
            }
            // Skip soft-hidden segments unless the user has toggled "show hidden".
            if !self.schedule_show_hidden
                && self.schedule_hidden_segments.contains(&s.segment_id)
            {
                continue;
            }
            // Skip segments that are secondaries of a merge (auto or manual) —
            // their primary renders the merged event block.
            if s.merged_into.is_some() { continue; }
            if self.schedule_auto_secondary.contains(&s.segment_id) { continue; }
            if is_all_day(s) {
                all_day_events.push(i);
            }
            if let Some(d) = local_date(s.start_time) {
                by_day.entry(d).or_default().push(i);
            }
        }
        // `schedule_all` is sorted by start_time, so each day's list is time-sorted.
        (collide, by_day, all_day_events)
    }

    /// ── Left sidebar: per-channel filter + collision toggle. ──
    fn schedule_sidebar(
        &self,
        ui: &mut egui::Ui,
        ptex: &PlatformTextures,
        clear_hidden: &mut bool,
        hide_all: &mut bool,
        toggle_channel: &mut Option<i64>,
        set_show_hidden: &mut Option<bool>,
    ) {
        egui::Panel::left("schedule_sidebar")
            .resizable(true)
            .default_size(210.0)
            .size_range(160.0..=380.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Channels");
                ui.add_space(2.0);

                // Distinct channels with upcoming streams, sorted by name.
                let mut chans: Vec<(i64, &str)> = Vec::new();
                let mut seen: HashSet<i64> = HashSet::new();
                for s in &self.schedule_all {
                    if seen.insert(s.channel_id) {
                        chans.push((s.channel_id, s.channel_name.as_str()));
                    }
                }
                chans.sort_by_key(|(_, name)| name.to_lowercase());

                let mut all = self.schedule_hidden.is_empty();
                if ui
                    .checkbox(&mut all, "All channels")
                    .on_hover_text("Show streams from every channel")
                    .changed()
                {
                    if all {
                        *clear_hidden = true;
                    } else {
                        *hide_all = true;
                    }
                }
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (id, name) in &chans {
                            let cid = *id;
                            // Count + distinct platforms for this channel's upcoming streams.
                            let mut count = 0usize;
                            let mut plats: Vec<Platform> = Vec::new();
                            for s in self.schedule_all.iter().filter(|s| s.channel_id == cid) {
                                count += 1;
                                let p = s.platform();
                                if !plats.contains(&p) {
                                    plats.push(p);
                                }
                            }
                            // Pre-compute for context-menu closure (can't re-borrow self inside).
                            let ch_is_hidden = self.schedule_hidden.contains(&cid);
                            // The channel's calendar colour — swatch + tinted
                            // name make the sidebar double as the legend.
                            let color = self
                                .schedule_chan_colors
                                .get(&cid)
                                .copied()
                                .unwrap_or_else(|| channel_event_color(cid, ""));
                            let row = ui.horizontal(|ui| {
                                let mut vis = !ch_is_hidden;
                                if ui.checkbox(&mut vis, "").changed() {
                                    *toggle_channel = Some(cid);
                                }
                                let (swatch, _) = ui.allocate_exact_size(
                                    egui::vec2(10.0, 10.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    swatch,
                                    egui::CornerRadius::same(2),
                                    color,
                                );
                                for &p in &plats {
                                    platform_icon(ui, ptex, p).on_hover_text(p.label());
                                }
                                let name_color =
                                    readable_color(color, ui.visuals().panel_fill);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("{name}  ({count})"))
                                            .color(name_color),
                                    )
                                    .truncate(),
                                );
                            }).response;
                            // Context menu — captures only Copy values, uses temp data.
                            row.context_menu(|ui| {
                                let label = if ch_is_hidden {
                                    "👁  Show channel"
                                } else {
                                    "🙈  Hide channel"
                                };
                                if ui.button(label).clicked() {
                                    ui.ctx().data_mut(|d| {
                                        d.insert_temp(egui::Id::new("sched_ch_toggle"), cid)
                                    });
                                    ui.close();
                                }
                                ui.separator();
                                if ui
                                    .button("⟳  Refetch schedule")
                                    .on_hover_text("Re-fetch this channel's schedule now, ignoring the cache")
                                    .clicked()
                                {
                                    ui.ctx().data_mut(|d| {
                                        d.insert_temp(egui::Id::new("sched_ch_refetch"), cid)
                                    });
                                    ui.close();
                                }
                            });
                        }

                        // Hidden-segment toggle (shown only when any exist).
                        let hidden_n = self.schedule_hidden_segments.len();
                        if hidden_n > 0 {
                            ui.separator();
                            let show_hidden = self.schedule_show_hidden;
                            let label = if show_hidden {
                                format!("🔒  Hide {hidden_n} hidden")
                            } else {
                                format!("👁  Show {hidden_n} hidden")
                            };
                            if ui
                                .button(label)
                                .on_hover_text(if show_hidden {
                                    "Filter out soft-hidden events again"
                                } else {
                                    "Show soft-hidden events dimmed (⊘) — right-click to restore"
                                })
                                .clicked()
                            {
                                *set_show_hidden = Some(!show_hidden);
                            }
                        }
                    });
            });
    }

    /// Visible date range + header title for the current mode, plus the
    /// prev/next navigation targets and the number of collisions in view.
    fn schedule_nav_dates(
        &self,
        mode: ScheduleMode,
        anchor: chrono::NaiveDate,
        collide: &HashSet<usize>,
    ) -> (String, chrono::NaiveDate, chrono::NaiveDate, usize) {
        use chrono::Datelike;
        let (range_start, range_end, title) = match mode {
            ScheduleMode::Month => {
                let first = chrono::NaiveDate::from_ymd_opt(anchor.year(), anchor.month(), 1)
                    .unwrap_or(anchor);
                let gs = week_start(first);
                (gs, add_days(gs, 42), month_title(anchor.year(), anchor.month()))
            }
            ScheduleMode::Week => {
                let ws = week_start(anchor);
                let we = add_days(ws, 6);
                // Honor the date-format setting on both ends (so the start year shows
                // for a cross-year week, and the order matches the user's preference).
                let pat = active_date_fmt().date_pattern();
                let title = format!("{} – {}", ws.format(pat), we.format(pat));
                (ws, add_days(ws, 7), title)
            }
            ScheduleMode::Day => {
                let title = anchor
                    .format(&format!("%A, {}", active_date_fmt().date_pattern()))
                    .to_string();
                (anchor, add_days(anchor, 1), title)
            }
            ScheduleMode::Agenda => {
                // Show all upcoming from anchor; use far-future end so the collision
                // badge counts everything visible.
                (anchor, add_days(anchor, 365), "Agenda".to_string())
            }
        };
        let (prev_date, next_date) = match mode {
            // Snap month nav to the 1st: the grid only uses the month anyway, and it
            // keeps paging idempotent (no day-of-month drift across short months).
            ScheduleMode::Month => (
                shift_month(anchor, -1).with_day(1).unwrap_or(anchor),
                shift_month(anchor, 1).with_day(1).unwrap_or(anchor),
            ),
            ScheduleMode::Week | ScheduleMode::Agenda => (add_days(anchor, -7), add_days(anchor, 7)),
            ScheduleMode::Day => (add_days(anchor, -1), add_days(anchor, 1)),
        };
        // Collisions visible in the current view (the per-chip ⚠ uses the global
        // `collide` set; the badge counts only what's on screen).
        let collisions_in_view = collide
            .iter()
            .filter(|&&i| {
                local_date(self.schedule_all[i].start_time)
                    .is_some_and(|d| d >= range_start && d < range_end)
            })
            .count();
        (title, prev_date, next_date, collisions_in_view)
    }

    /// Header: view mode switcher + navigation + title + zoom + collision
    /// controls.
    #[allow(clippy::too_many_arguments)]
    fn schedule_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        mode: ScheduleMode,
        title: &str,
        today: chrono::NaiveDate,
        prev_date: chrono::NaiveDate,
        next_date: chrono::NaiveDate,
        collisions_in_view: usize,
        set_mode: &mut Option<ScheduleMode>,
        nav_anchor: &mut Option<chrono::NaiveDate>,
        do_refresh: &mut bool,
        open_sources: &mut bool,
        set_collisions: &mut Option<bool>,
    ) {
        ui.horizontal(|ui| {
            let mut m = mode;
            ui.selectable_value(&mut m, ScheduleMode::Month, "Month");
            ui.selectable_value(&mut m, ScheduleMode::Week, "Week");
            ui.selectable_value(&mut m, ScheduleMode::Day, "Day");
            ui.selectable_value(&mut m, ScheduleMode::Agenda, "Agenda");
            if m != mode {
                *set_mode = Some(m);
            }
            ui.separator();
            if ui.button("◀").on_hover_text("Previous").clicked() {
                *nav_anchor = Some(prev_date);
            }
            if ui.button("Today").clicked() {
                *nav_anchor = Some(today);
            }
            if ui.button("▶").on_hover_text("Next").clicked() {
                *nav_anchor = Some(next_date);
            }
            ui.add_space(8.0);
            ui.heading(title);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Calendar-body zoom (font + element size) — buttons mirror
                // the Ctrl+Plus/Minus/0 shortcuts. Toolbar/sidebar are
                // unaffected; only the grid rendered below this header scales.
                if ui
                    .add(egui::Button::new("🔍+").small())
                    .on_hover_text("Zoom in (Ctrl+Plus)")
                    .clicked()
                {
                    self.schedule_zoom = (self.schedule_zoom + SCHEDULE_ZOOM_STEP).min(SCHEDULE_ZOOM_MAX);
                }
                if ui
                    .add(egui::Button::new(format!("{:.0}%", self.schedule_zoom * 100.0)).small())
                    .on_hover_text("Reset zoom (Ctrl+0)")
                    .clicked()
                {
                    self.schedule_zoom = 1.0;
                }
                if ui
                    .add(egui::Button::new("🔍−").small())
                    .on_hover_text("Zoom out (Ctrl+Minus)")
                    .clicked()
                {
                    self.schedule_zoom = (self.schedule_zoom - SCHEDULE_ZOOM_STEP).max(SCHEDULE_ZOOM_MIN);
                }
                ui.separator();
                if ui
                    .button("⟳")
                    .on_hover_text("Fetch the latest schedules now (F5)")
                    .clicked()
                {
                    *do_refresh = true;
                }
                if ui
                    .button("⛭ Sources")
                    .on_hover_text(
                        "Choose which schedule sources to use and their priority order",
                    )
                    .clicked()
                {
                    *open_sources = true;
                }
                let mut hc = self.schedule_collisions;
                if ui
                    .checkbox(&mut hc, "Highlight collisions")
                    .on_hover_text("Flag streams whose scheduled times overlap")
                    .changed()
                {
                    *set_collisions = Some(hc);
                }
                let mut compact = self.schedule_compact;
                if ui
                    .checkbox(&mut compact, "Compact")
                    .on_hover_text(
                        "Collapse Week/Day events to a one-line chip at their start time — \
                         a quick overview when many streams overlap. Hover a chip for the \
                         full details.",
                    )
                    .changed()
                {
                    self.schedule_compact = compact;
                    let _ = self
                        .core
                        .store
                        .set_setting(K_SCHEDULE_COMPACT, if compact { "1" } else { "0" });
                }
                if self.schedule_collisions && collisions_in_view > 0 {
                    ui.colored_label(HL_COLLISION, format!("⚠ {collisions_in_view}"))
                        .on_hover_text("Overlapping streams in view");
                }
            });
        });
        ui.separator();
    }

    /// ── Selection action bar (shown when any events are selected). ──
    fn schedule_selection_bar(&mut self, ui: &mut egui::Ui) {
        let n_sel = self.schedule_selected.len();
        if n_sel > 0 {
            let accent = ui.visuals().selection.bg_fill;
            ui.horizontal(|ui| {
                ui.colored_label(accent, format!("{n_sel} selected"));
                ui.separator();
                // Merge: only valid when ≥2 events from the same channel are selected
                let can_merge = n_sel >= 2 && {
                    let ch_ids: HashSet<i64> = self
                        .schedule_selected
                        .iter()
                        .filter_map(|&sid| {
                            self.schedule_all.iter().find(|s| s.segment_id == sid)
                        })
                        .map(|s| s.channel_id)
                        .collect();
                    ch_ids.len() == 1
                };
                if ui
                    .add_enabled(can_merge, egui::Button::new("🔀 Merge"))
                    .on_hover_text("Merge selected events into one calendar entry")
                    .on_disabled_hover_text(
                        if n_sel < 2 {
                            "Select 2+ events to merge"
                        } else {
                            "Can only merge events from the same channel"
                        },
                    )
                    .clicked()
                {
                    let mut segs: Vec<UpcomingStream> = self
                        .schedule_selected
                        .iter()
                        .filter_map(|&sid| {
                            self.schedule_all.iter().find(|s| s.segment_id == sid).cloned()
                        })
                        .collect();
                    // Sort highest priority first (YouTube first) so index 0 is the default primary.
                    segs.sort_by_key(|s| merge_source_priority(&s.source));
                    self.merge_preview = Some(MergePreviewDraft {
                        segments: segs,
                        primary_idx: 0,
                        error: String::new(),
                    });
                }
                if ui
                    .button("🗑 Delete")
                    .on_hover_text("Delete all selected events")
                    .clicked()
                {
                    self.confirm_delete_segments =
                        Some(self.schedule_selected.iter().copied().collect());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕ Clear").on_hover_text("Clear selection").clicked() {
                        self.schedule_selected.clear();
                    }
                });
            });
            ui.separator();
        }
    }

    /// ── Apply collected actions ── runs after the panel closures have
    /// released their borrows of `self`; the temp-storage reads pick up
    /// context-menu / selection actions written by closures that can't
    /// borrow `self` directly.
    #[allow(clippy::too_many_arguments)]
    fn schedule_apply_actions(
        &mut self,
        ui: &egui::Ui,
        set_mode: Option<ScheduleMode>,
        nav_anchor: Option<chrono::NaiveDate>,
        clear_hidden: bool,
        hide_all: bool,
        toggle_channel: Option<i64>,
        set_collisions: Option<bool>,
        open_day: Option<chrono::NaiveDate>,
        do_refresh: bool,
        open_sources: bool,
        set_show_hidden: Option<bool>,
    ) {
        if let Some(m) = set_mode {
            self.schedule_mode = m;
        }
        if let Some(d) = nav_anchor {
            self.schedule_anchor = Some(d);
        }
        if clear_hidden {
            self.schedule_hidden.clear();
        }
        if hide_all {
            let ids: Vec<i64> = self.schedule_all.iter().map(|s| s.channel_id).collect();
            self.schedule_hidden.extend(ids);
        }
        if let Some(id) = toggle_channel {
            // Toggle this channel's visibility.
            if self.schedule_hidden.contains(&id) {
                self.schedule_hidden.remove(&id);
            } else {
                self.schedule_hidden.insert(id);
            }
        }
        if let Some(v) = set_collisions {
            self.schedule_collisions = v;
        }
        if let Some(d) = open_day {
            self.schedule_day_popup = Some(d);
        }
        if do_refresh {
            // Trigger a real network re-fetch (not just a DB reload); the refresher
            // emits an event when done, which reloads the calendar. Also reload now
            // so the current stored data shows immediately.
            self.core.request_schedule_refresh();
            self.spawn_reload_schedule();
            self.status = "Fetching latest schedules…".into();
        }
        if open_sources {
            self.open_schedule_sources();
        }
        // Context-menu actions written into egui temp storage by schedule_copy_menu
        // (closures can't borrow `self` directly when deep inside panel closures).
        if let Some(mid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_jump")))
        {
            self.view = View::Streams;
            self.selected_monitor = Some(mid);
        }
        if let Some(mid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_start")))
        {
            self.core.manual(ManualCommand::Start { id: mid, user_initiated: true });
            self.status = "Checking channel… will record if live.".into();
        }
        if let Some(s) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_schedule_rec")))
            .and_then(|sid| self.schedule_all.iter().find(|s| s.segment_id == sid))
        {
            self.scheduled_recording_form = Some(ScheduledRecordingForm::from_schedule_entry(s));
        }
        if let Some(sid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_edit")))
        {
            self.open_edit_schedule(sid);
        }
        if let Some(sid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_open_src")))
        {
            let ctx = ui.ctx().clone();
            self.open_schedule_source(sid, &ctx);
        }
        if let Some(sid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_hide")))
        {
            if self.schedule_hidden_segments.contains(&sid) {
                self.schedule_hidden_segments.remove(&sid);
            } else {
                self.schedule_hidden_segments.insert(sid);
            }
        }
        if let Some(sid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_delete")))
        {
            self.confirm_delete_segment = Some(sid);
        }
        if let Some(cid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_ch_toggle")))
        {
            if self.schedule_hidden.contains(&cid) {
                self.schedule_hidden.remove(&cid);
            } else {
                self.schedule_hidden.insert(cid);
            }
        }
        if let Some(cid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_ch_refetch")))
        {
            self.core.request_schedule_refresh_for_channel(cid);
            self.spawn_reload_schedule();
            self.status = "Fetching schedule…".into();
        }
        if let Some(v) = set_show_hidden {
            self.schedule_show_hidden = v;
        }
        // Selection actions written into temp storage from schedule_time_grid
        // (free function, can't borrow self).
        if let Some(sid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_sel_toggle")))
        {
            if self.schedule_selected.contains(&sid) {
                self.schedule_selected.remove(&sid);
            } else {
                self.schedule_selected.insert(sid);
            }
        }
        if let Some(sid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_sel_single")))
        {
            self.schedule_selected.clear();
            self.schedule_selected.insert(sid);
        }
        // Un-merge actions from context menus.
        if let Some(primary_id) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_unmerge_manual")))
        {
            if let Err(e) = self.core.store.unmerge_segment(primary_id) {
                self.status = format!("Error un-merging: {e:#}");
            } else {
                self.spawn_reload_schedule();
            }
        }
        if let Some(primary_id) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_unmerge_auto")))
        {
            // Set auto_merge_excluded on all auto-secondaries of this primary.
            let secondary_ids: Vec<i64> = self
                .schedule_auto_secondary
                .iter()
                .copied()
                .filter(|&sid| {
                    // Find this segment in schedule_all; check if it overlaps the primary
                    // and belongs to the same channel.
                    if let Some(primary) = self.schedule_all.iter().find(|s| s.segment_id == primary_id) {
                        self.schedule_all.iter().any(|s| {
                            s.segment_id == sid && s.channel_id == primary.channel_id
                        })
                    } else {
                        false
                    }
                })
                .collect();
            let mut changed = false;
            for sid in secondary_ids {
                if let Err(e) = self.core.store.set_auto_merge_excluded(sid, true) {
                    self.status = format!("Error excluding from auto-merge: {e:#}");
                } else {
                    changed = true;
                }
            }
            if changed {
                self.spawn_reload_schedule();
            }
        }
    }

    /// One compact calendar chip (colored stripe · ⚠ · platform icon · time range · channel)
    /// with a hover detail and the copy context menu. Returns the click response so
    /// the caller can react (e.g. open the day popup). Shared by month + week views.
    pub(super) fn schedule_chip(
        &self,
        ui: &mut egui::Ui,
        i: usize,
        colliding: bool,
        ptex: &PlatformTextures,
    ) -> egui::Response {
        let s = &self.schedule_all[i];
        let is_hidden = self.schedule_hidden_segments.contains(&s.segment_id);
        let is_selected = self.schedule_selected.contains(&s.segment_id);
        let merge_label = self.schedule_merge_labels.get(&s.segment_id).map(String::as_str);
        let color = self.sched_color(s);
        let stripe_color = if is_hidden {
            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 85)
        } else {
            color
        };
        let resp = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                // 3px colored left stripe
                let (stripe_rect, _) = ui.allocate_exact_size(
                    egui::vec2(3.0, ui.text_style_height(&egui::TextStyle::Body)),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(stripe_rect, egui::CornerRadius::same(2), stripe_color);
                if is_hidden {
                    ui.weak("⊘");
                }
                if is_selected {
                    ui.label(egui::RichText::new("✓").color(ui.visuals().selection.bg_fill).small());
                }
                if merge_label.is_some() {
                    ui.label(egui::RichText::new("🔀").small());
                }
                if colliding {
                    ui.colored_label(HL_COLLISION, "⚠");
                }
                platform_icon(ui, ptex, s.platform());
                schedule_source_badge(ui, &s.source);
                if !s.collab.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new("🤝").small()))
                        .on_hover_text(format!("With: {}", s.collab));
                }
                let label_text = format!(
                    "{}  {}",
                    fmt_time_range(s.start_time, s.end_time),
                    s.channel_name
                );
                let label = if is_hidden {
                    egui::RichText::new(label_text).weak()
                } else {
                    egui::RichText::new(label_text)
                };
                ui.add(egui::Label::new(label).truncate());
            })
            .response
            .interact(egui::Sense::click());
        let ml_owned = merge_label.map(str::to_string);
        let resp = resp.on_hover_text(schedule_detail_line_merged(s, merge_label));
        resp.context_menu(|ui| schedule_copy_menu(ui, s, is_hidden, ml_owned.as_deref()));
        resp
    }

    /// Month view: a 6×7 grid of day cells.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn schedule_month_grid(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        today: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        use chrono::Datelike;
        let month = anchor.month();
        let first = chrono::NaiveDate::from_ymd_opt(anchor.year(), month, 1).unwrap_or(anchor);
        let grid_start = week_start(first);

        // Scheduled-recording day badges (schema v51): every enabled rule's
        // occurrences across the visible 6-week range, grouped by local date
        // for `schedule_cell`'s badge row + hover detail.
        let range_start = local_midnight(grid_start) - 1;
        let range_end = local_midnight(add_days(grid_start, 42));
        let mut sched_rec_by_day: HashMap<chrono::NaiveDate, Vec<String>> = HashMap::new();
        for row in &self.scheduled_recordings {
            if !row.rec.enabled {
                continue;
            }
            for ts in crate::scheduled_recordings::occurrences_in_range(&row.rec, range_start, range_end) {
                if let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) {
                    let local = dt.with_timezone(&chrono::Local);
                    sched_rec_by_day
                        .entry(local.date_naive())
                        .or_default()
                        .push(format!("{} — {}", row.channel_name, local.format("%H:%M")));
                }
            }
        }

        let zoom = self.schedule_zoom;
        let spacing = 4.0 * zoom;
        let cell_h = 108.0 * zoom;
        const WD: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        const MAX_CHIPS: usize = 3;
        // Reserve room for a vertical scrollbar so the columns don't shift when it
        // appears, and floor the width so a too-narrow panel gets a horizontal
        // scrollbar instead of clipping the weekend columns.
        let usable = (ui.available_width() - 16.0).max(160.0);
        let col_w = ((usable - spacing * 6.0) / 7.0).floor().max(72.0);

        // Header + weeks share one scroll viewport so their columns stay aligned.
        egui::ScrollArea::both()
            .id_salt("sched_month")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
                ui.horizontal(|ui| {
                    for &wd in &WD {
                        ui.allocate_ui_with_layout(
                            egui::vec2(col_w, 16.0 * zoom),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                ui.label(egui::RichText::new(wd).strong());
                            },
                        );
                    }
                });
                for week in 0..6u64 {
                    ui.horizontal(|ui| {
                        for dow in 0..7u64 {
                            let day = add_days(grid_start, (week * 7 + dow) as i64);
                            ui.allocate_ui_with_layout(
                                egui::vec2(col_w, cell_h),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    self.schedule_cell(
                                        ui,
                                        day,
                                        month,
                                        today,
                                        col_w,
                                        cell_h,
                                        MAX_CHIPS,
                                        by_day.get(&day),
                                        collide,
                                        ptex,
                                        open_day,
                                        sched_rec_by_day.get(&day),
                                    );
                                },
                            );
                        }
                    });
                }
            });
    }

    /// Week view: 7-column time-grid with a 24-hour vertical axis, plus a
    /// day-header row that shows the avatars of channels with a scheduled
    /// recording due that day, and a Google-Calendar-style all-day event bar
    /// strip below the header for streams long enough to count as "all-day"
    /// (see [`is_all_day`] — e.g. a multi-day subathon).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn schedule_week_grid(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        today: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        _ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
        sched_rec_avatars: &HashMap<i64, egui::TextureHandle>,
        all_day_events: &[usize],
    ) {
        use chrono::Datelike;
        let ws = week_start(anchor);
        let we = add_days(ws, 6);
        let days: Vec<chrono::NaiveDate> = (0..7).map(|d| add_days(ws, d)).collect();
        let zoom = self.schedule_zoom;

        // Day-header row (outside the scroll area so it stays fixed).
        let time_col_w = SCHED_TIME_COL_W * zoom;
        let col_gap = SCHED_COL_GAP * zoom;
        let avail_w = ui.available_width();
        let col_w = ((avail_w - time_col_w - 6.0 * col_gap) / 7.0).max(60.0);

        // Channels with an enabled scheduled recording due each visible day
        // (only those we actually have an avatar for — no point bucketing
        // channels that'll render nothing).
        let mut sched_rec_channels_by_day: HashMap<chrono::NaiveDate, Vec<i64>> = HashMap::new();
        if !sched_rec_avatars.is_empty() {
            let range_start = local_midnight(ws) - 1;
            let range_end = local_midnight(add_days(ws, 7));
            for row in &self.scheduled_recordings {
                if !row.rec.enabled {
                    continue;
                }
                let Some(mon_row) = self.rows.iter().find(|r| r.monitor.id == row.rec.monitor_id)
                else {
                    continue;
                };
                let cid = mon_row.channel.id;
                if !sched_rec_avatars.contains_key(&cid) {
                    continue;
                }
                for ts in crate::scheduled_recordings::occurrences_in_range(&row.rec, range_start, range_end) {
                    if let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) {
                        let day = dt.with_timezone(&chrono::Local).date_naive();
                        let list = sched_rec_channels_by_day.entry(day).or_default();
                        if !list.contains(&cid) {
                            list.push(cid);
                        }
                    }
                }
            }
        }
        let show_avatar_row = !sched_rec_channels_by_day.is_empty();
        let header_h = 36.0 * zoom + if show_avatar_row { SCHED_AVATAR_ROW_H * zoom } else { 0.0 };

        ui.horizontal(|ui| {
            ui.add_space(time_col_w);
            for &day in &days {
                let is_today = day == today;
                let hdr = format!("{}\n{}", day.format("%a"), day.day());
                let text = if is_today {
                    egui::RichText::new(hdr).strong().color(ui.visuals().hyperlink_color)
                } else {
                    egui::RichText::new(hdr).strong()
                };
                let day_channels = sched_rec_channels_by_day.get(&day);
                let resp = ui.allocate_ui_with_layout(
                    egui::vec2(col_w + col_gap, header_h),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        if is_today {
                            let r = ui.max_rect();
                            ui.painter().rect_filled(r, egui::CornerRadius::ZERO, TODAY_BG);
                        }
                        let lbl_resp = ui
                            .add(egui::Label::new(text).sense(egui::Sense::click()))
                            .on_hover_text("Open day detail");
                        if show_avatar_row {
                            let _ = ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0 * zoom;
                                const MAX_AVATARS: usize = 5;
                                if let Some(chans) = day_channels {
                                    for &cid in chans.iter().take(MAX_AVATARS) {
                                        if let Some(tex) = sched_rec_avatars.get(&cid) {
                                            let name = self
                                                .rows
                                                .iter()
                                                .find(|r| r.channel.id == cid)
                                                .map(|r| r.channel.name.as_str())
                                                .unwrap_or("");
                                            ui.add(
                                                egui::Image::from_texture(tex)
                                                    .fit_to_exact_size(egui::vec2(
                                                        SCHED_AVATAR_PX * zoom,
                                                        SCHED_AVATAR_PX * zoom,
                                                    ))
                                                    .corner_radius(egui::CornerRadius::same(2)),
                                            )
                                            .on_hover_text(format!("{name} — recording scheduled"));
                                        }
                                    }
                                    if chans.len() > MAX_AVATARS {
                                        ui.weak(format!("+{}", chans.len() - MAX_AVATARS));
                                    }
                                }
                            });
                        }
                        lbl_resp
                    },
                );
                if resp.inner.clicked() {
                    *open_day = Some(day);
                }
            }
        });

        // All-day event bar strip (Google-Calendar style): each qualifying
        // stream (`is_all_day`) draws as one continuous rounded bar spanning
        // its start-to-end day columns, clipped to the visible week. Bars
        // that overlap in date range stack into additional lanes.
        let mut bars: Vec<(usize, usize, usize)> = Vec::new(); // (stream_idx, start_col, end_col)
        for &i in all_day_events {
            let s = &self.schedule_all[i];
            let s_date = local_date(s.start_time).unwrap_or(ws);
            let e_date = local_date(effective_end(s)).unwrap_or(s_date);
            if e_date < ws || s_date > we {
                continue; // doesn't overlap the visible week
            }
            let start_col = if s_date <= ws { 0usize } else { (s_date - ws).num_days().max(0) as usize };
            let end_col = if e_date >= we { 6usize } else { (e_date - ws).num_days().max(0) as usize };
            bars.push((i, start_col.min(6), end_col.min(6).max(start_col.min(6))));
        }
        if !bars.is_empty() {
            bars.sort_by_key(|&(_, start, end)| (start, end));
            let mut lane_end: Vec<i32> = Vec::new();
            let mut placed: Vec<(usize, usize, usize, usize)> = Vec::new(); // (idx, start, end, lane)
            for (idx, start, end) in bars {
                let lane = lane_end
                    .iter()
                    .position(|&le| le < start as i32)
                    .unwrap_or_else(|| {
                        lane_end.push(-1);
                        lane_end.len() - 1
                    });
                lane_end[lane] = end as i32;
                placed.push((idx, start, end, lane));
            }

            let bar_h = SCHED_ALL_DAY_BAR_H * zoom;
            let bar_gap = SCHED_ALL_DAY_BAR_GAP * zoom;
            let strip_w = time_col_w + days.len() as f32 * (col_w + col_gap);
            let strip_h = lane_end.len() as f32 * (bar_h + bar_gap);
            let (resp, painter) =
                ui.allocate_painter(egui::vec2(strip_w, strip_h), egui::Sense::hover());
            let origin = resp.rect.min;
            let mut clicked_day: Option<chrono::NaiveDate> = None;
            for (idx, start, end, lane) in &placed {
                let s = &self.schedule_all[*idx];
                let x0 = origin.x + time_col_w + *start as f32 * (col_w + col_gap);
                let x1 = origin.x + time_col_w + (*end as f32 + 1.0) * (col_w + col_gap) - col_gap;
                let y = origin.y + *lane as f32 * (bar_h + bar_gap);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(x0, y),
                    egui::vec2((x1 - x0).max(4.0), bar_h),
                );
                let evt_id = egui::Id::new("sched_allday").with(ws).with(*idx);
                let evt_resp = ui.interact(rect, evt_id, egui::Sense::click());
                let color = self.sched_color(s);
                let fill = if evt_resp.hovered() {
                    color
                } else {
                    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 210)
                };
                painter.rect_filled(rect, egui::CornerRadius::same(4), fill);
                let label = if s.title.is_empty() {
                    s.channel_name.clone()
                } else {
                    format!("{} — {}", s.channel_name, s.title)
                };
                painter.with_clip_rect(rect).text(
                    egui::pos2(rect.left() + 5.0 * zoom, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(11.0 * zoom),
                    egui::Color32::WHITE,
                );
                if evt_resp.clicked() {
                    clicked_day = Some(days[*start]);
                }
                evt_resp.on_hover_text(schedule_detail_line(s)).context_menu(|ui| {
                    schedule_copy_menu(ui, s, false, None)
                });
            }
            if let Some(d) = clicked_day {
                *open_day = Some(d);
            }
        }

        ui.separator();

        let exclude_all_day: HashSet<usize> = all_day_events.iter().copied().collect();
        schedule_time_grid(
            ui,
            "sched_week",
            &days,
            col_w,
            zoom,
            &self.schedule_all,
            by_day,
            collide,
            &exclude_all_day,
            open_day,
            &self.schedule_hidden_segments,
            &self.schedule_selected,
            &self.schedule_merge_labels,
            &self.schedule_chan_colors,
            self.schedule_compact,
        );
    }

    /// Day view: single-column time-grid with full-width event blocks.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn schedule_day_grid(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        today: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        let is_today = anchor == today;
        let hdr = anchor
            .format(&format!("%A, {}", active_date_fmt().date_pattern()))
            .to_string();
        let text = if is_today {
            egui::RichText::new(hdr).strong().color(ui.visuals().hyperlink_color)
        } else {
            egui::RichText::new(hdr).strong()
        };
        ui.label(text);
        ui.separator();

        let zoom = self.schedule_zoom;
        let avail_w = ui.available_width();
        let col_w = (avail_w - SCHED_TIME_COL_W * zoom - 2.0).max(80.0);
        schedule_time_grid(
            ui,
            "sched_day",
            &[anchor],
            col_w,
            zoom,
            &self.schedule_all,
            by_day,
            collide,
            &HashSet::new(),
            open_day,
            &self.schedule_hidden_segments,
            &self.schedule_selected,
            &self.schedule_merge_labels,
            &self.schedule_chan_colors,
            self.schedule_compact,
        );
    }

    /// Render one month-grid day cell: a bordered box with the day number and up to
    /// `max_chips` stream chips (overflow folds into a clickable "+N more"). A
    /// left-click on the day or a chip opens the day popup (`open_day`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn schedule_cell(
        &self,
        ui: &mut egui::Ui,
        day: chrono::NaiveDate,
        month: u32,
        today: chrono::NaiveDate,
        col_w: f32,
        cell_h: f32,
        max_chips: usize,
        entries: Option<&Vec<usize>>,
        collide: &HashSet<usize>,
        ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
        sched_recs: Option<&Vec<String>>,
    ) {
        use chrono::Datelike;
        let in_month = day.month() == month;
        let is_today = day == today;

        let mut frame = egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(4));
        if is_today {
            frame = frame.fill(TODAY_BG);
        }
        frame.show(ui, |ui| {
            ui.set_min_size(egui::vec2(col_w - 10.0, cell_h - 10.0));
            ui.vertical(|ui| {
                // Day number: strong in-month (today is set off by its tinted
                // cell background), dimmed for the leading/trailing days that spill
                // in from the neighbouring months.
                let num = egui::RichText::new(day.day().to_string());
                let num = if is_today || in_month {
                    num.strong()
                } else {
                    num.weak()
                };
                if ui
                    .add(egui::Label::new(num).sense(egui::Sense::click()))
                    .on_hover_text("Show this day's streams")
                    .clicked()
                {
                    *open_day = Some(day);
                }

                // Scheduled-recording badge row (schema v51) — shown iff this day
                // has ≥1 enabled rule due to fire, regardless of whether the
                // calendar already has a matching stream entry.
                if let Some(recs) = sched_recs.filter(|r| !r.is_empty()) {
                    ui.add(egui::Label::new(
                        egui::RichText::new("⏺ rec").small().color(egui::Color32::from_rgb(0xe0, 0x50, 0x50)),
                    ))
                    .on_hover_text(format!("Scheduled recording(s):\n{}", recs.join("\n")));
                }

                let entries = entries.map(Vec::as_slice).unwrap_or(&[]);
                let shown = entries.len().min(max_chips);
                for &i in &entries[..shown] {
                    let colliding = collide.contains(&i);
                    if self.schedule_chip(ui, i, colliding, ptex).clicked() {
                        *open_day = Some(day);
                    }
                }
                if entries.len() > shown {
                    let more = entries.len() - shown;
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(format!("+{more} more…")).weak(),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .clicked()
                    {
                        *open_day = Some(day);
                    }
                }
            });
        });
    }

    /// Agenda view: date-grouped chronological list of all upcoming streams from `anchor`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn schedule_agenda_view(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        let zoom = self.schedule_zoom;
        // Collect and sort the days from `anchor` forward that have visible entries.
        let mut days: Vec<chrono::NaiveDate> = by_day
            .keys()
            .filter(|&&d| d >= anchor)
            .copied()
            .collect();
        days.sort();

        if days.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new("No streams scheduled from this date.").weak());
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("sched_agenda")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for day in &days {
                    let Some(indices) = by_day.get(day) else { continue };
                    if indices.is_empty() { continue }

                    // Date group header
                    let heading = day
                        .format(&format!("%A, {}", active_date_fmt().date_pattern()))
                        .to_string();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.strong(heading);
                    });
                    ui.separator();

                    for &i in indices {
                        let s = &self.schedule_all[i];
                        let is_hidden = self.schedule_hidden_segments.contains(&s.segment_id);
                        let merge_label_agenda = self.schedule_merge_labels.get(&s.segment_id).map(String::as_str);
                        let color = self.sched_color(s);
                        let stripe_color = if is_hidden {
                            egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 85)
                        } else {
                            color
                        };
                        let colliding = collide.contains(&i);

                        let row_resp = ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;

                            // Colored stripe
                            let (stripe_rect, _) = ui.allocate_exact_size(
                                egui::vec2(4.0, ui.text_style_height(&egui::TextStyle::Body) * 1.4),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(stripe_rect, egui::CornerRadius::same(2), stripe_color);
                            if is_hidden {
                                ui.weak("⊘");
                            }
                            if merge_label_agenda.is_some() {
                                ui.label(egui::RichText::new("🔀").small());
                            }

                            // Time range
                            if colliding {
                                ui.colored_label(HL_COLLISION, "⚠");
                            }
                            ui.add(egui::Label::new(
                                egui::RichText::new(fmt_time_range(s.start_time, s.end_time))
                                    .monospace()
                                    .size(12.0 * zoom),
                            ));

                            // Platform icon + source badge
                            platform_icon(ui, ptex, s.platform());
                            schedule_source_badge(ui, &s.source);

                            // Channel name (bold or weak if hidden)
                            if is_hidden {
                                ui.weak(&s.channel_name);
                            } else {
                                ui.strong(&s.channel_name);
                            }

                            // Title (muted, truncated)
                            if !s.title.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("— {}", s.title))
                                            .weak()
                                            .size(12.0 * zoom),
                                    )
                                    .truncate(),
                                );
                            }

                            // Category in parens (weak)
                            if !s.category.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("({})", s.category))
                                            .weak()
                                            .size(11.0 * zoom),
                                    )
                                    .truncate(),
                                );
                            }
                        })
                        .response
                        .interact(egui::Sense::click());

                        let ml_agenda_owned = merge_label_agenda.map(str::to_string);
                        let row_resp = row_resp.on_hover_text(schedule_detail_line_merged(s, merge_label_agenda));
                        row_resp.context_menu(|ui| schedule_copy_menu(ui, s, is_hidden, ml_agenda_owned.as_deref()));
                        if row_resp.clicked() {
                            *open_day = Some(*day);
                        }
                        ui.add_space(1.0);
                    }
                }
            });
    }

    /// Popup listing every (visible) stream on one calendar day, with the same
    /// per-entry copy menu as the calendar chips.
    #[allow(deprecated)]
    pub(super) fn schedule_day_window(&mut self, ctx: &egui::Context) {
        let Some(date) = self.schedule_day_popup else {
            return;
        };
        let ptex = self
            .platform_tex
            .get_or_insert_with(|| PlatformTextures::load(ctx))
            .clone();
        // Visible streams on that local date (respects the sidebar filter).
        let entries: Vec<&UpcomingStream> = self
            .schedule_all
            .iter()
            .filter(|s| !self.schedule_hidden.contains(&s.channel_id))
            .filter(|s| local_date(s.start_time) == Some(date))
            .collect();
        // Weekday + the user's chosen date format (so the heading matches the chips).
        let heading = date
            .format(&format!("%A, {}", active_date_fmt().date_pattern()))
            .to_string();

        let hidden_segs = self.schedule_hidden_segments.clone();
        let merge_labels = self.schedule_merge_labels.clone();
        let mut open = true;
        let mut copy_all: Option<String> = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("schedule_day_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("Streams · {heading}"))
                .with_inner_size([480.0, 360.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if entries.is_empty() {
                        ui.label("No streams scheduled this day.");
                        return;
                    }
                    ui.label(format!("{} scheduled stream(s).", entries.len()));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for s in &entries {
                                // The popup doesn't carry the collision set; the calendar
                                // surfaces ⚠ markers, so rows here are shown unmarked.
                                let hidden = hidden_segs.contains(&s.segment_id);
                                let ml = merge_labels.get(&s.segment_id).map(String::as_str);
                                schedule_detail_row(ui, s, false, hidden, &ptex, ml, self.sched_color(s));
                                // Action button strip — writes to the same temp-data keys
                                // that schedule_view() drains each frame.
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    if ui.small_button("✏  Edit").on_hover_text("Edit title, category or time").clicked() {
                                        ctx.data_mut(|d| d.insert_temp(egui::Id::new("sched_edit"), s.segment_id));
                                    }
                                    let can_open_src = !s.is_manual()
                                        && ScheduleSourceKind::from_id(&s.source)
                                            .is_some_and(|k| k.has_open_target());
                                    ui.add_enabled(can_open_src, egui::Button::new("🔗  Source").small())
                                        .on_hover_text("Open where this item came from (schedule page or OCR image)")
                                        .on_disabled_hover_text("No external source (manually edited or Discord event)")
                                        .clicked()
                                        .then(|| ctx.data_mut(|d| d.insert_temp(egui::Id::new("sched_open_src"), s.segment_id)));
                                    let hide_label = if hidden { "👁  Show" } else { "🙈  Hide" };
                                    let hide_tip   = if hidden { "Show this event on the calendar" } else { "Soft-hide (still stored, just greyed out)" };
                                    if ui.small_button(hide_label).on_hover_text(hide_tip).clicked() {
                                        ctx.data_mut(|d| d.insert_temp(egui::Id::new("sched_hide"), s.segment_id));
                                    }
                                    if ui.add(egui::Button::new(
                                            egui::RichText::new("🗑  Delete").color(HL_ERROR_TEXT)).small())
                                        .on_hover_text("Tombstone — permanently suppress; won't reappear on refresh")
                                        .clicked()
                                    {
                                        ctx.data_mut(|d| d.insert_temp(egui::Id::new("sched_delete"), s.segment_id));
                                    }
                                });
                                ui.add_space(4.0);
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy all").clicked() {
                        copy_all = Some(
                            entries
                                .iter()
                                .map(|s| schedule_detail_line(s))
                                .collect::<Vec<_>>()
                                .join("\n\n"),
                        );
                    }
                });
            },
        );
        if let Some(t) = copy_all {
            ctx.copy_text(t);
        }
        if !open {
            self.schedule_day_popup = None;
        }
    }

    /// Open the "Edit schedule item" dialog for a calendar occurrence (by segment
    /// id), seeding the draft from its current stored values. No-op if the segment
    /// is no longer in the loaded calendar (e.g. a refresh dropped it).
    pub(super) fn open_edit_schedule(&mut self, segment_id: i64) {
        let Some(s) = self
            .schedule_all
            .iter()
            .find(|s| s.segment_id == segment_id)
        else {
            return;
        };
        let (date, time) = split_local_datetime(s.start_time);
        let (end_date, end_time) = match s.end_time {
            Some(e) => split_local_datetime(e),
            None => (String::new(), String::new()),
        };
        self.edit_schedule = Some(EditScheduleDraft {
            segment_id,
            channel_name: s.channel_name.clone(),
            source: s.source.clone(),
            title: s.title.clone(),
            category: s.category.clone(),
            date,
            time,
            end_date,
            end_time,
            error: String::new(),
        });
    }

    /// Open where a calendar item's schedule came from: the platform schedule page,
    /// the community/profile page, or the source image. No-op for manual rows and
    /// Discord events (the context item is disabled for those).
    pub(super) fn open_schedule_source(&mut self, segment_id: i64, ctx: &egui::Context) {
        enum Target {
            Url(String),
            Path(std::path::PathBuf),
        }
        let Some(s) = self
            .schedule_all
            .iter()
            .find(|s| s.segment_id == segment_id)
        else {
            return;
        };
        let Some(kind) = ScheduleSourceKind::from_id(&s.source) else {
            self.status = "No external source to open for this item.".into();
            return;
        };
        let target = match kind {
            ScheduleSourceKind::TwitchSchedule => crate::detectors::twitch_login(&s.url)
                .map(|login| Target::Url(format!("https://www.twitch.tv/{login}/schedule"))),
            ScheduleSourceKind::YouTubeScrape | ScheduleSourceKind::YouTubeApi => {
                Some(Target::Url(crate::detectors::youtube_streams_url(&s.url)))
            }
            ScheduleSourceKind::YouTubeCommunityOcr => {
                Some(Target::Url(crate::detectors::youtube_community_url(&s.url)))
            }
            ScheduleSourceKind::TwitchBannerOcr => {
                // Prefer the OCR'd offline-banner image on disk (this source's
                // own account dir, then any account/legacy dir); fall back to
                // the channel page (which shows the banner when offline).
                let account = asset_account(&s.url, Platform::Twitch);
                let dir = crate::assets::channel_asset_dir(&s.channel_name, Platform::Twitch, &account);
                crate::assets::find_asset(&dir, "banner.")
                    .or_else(|| {
                        crate::assets::find_asset_any_account(
                            &s.channel_name,
                            Platform::Twitch,
                            "banner.",
                        )
                    })
                    .map(Target::Path)
                    .or_else(|| {
                        crate::detectors::twitch_login(&s.url)
                            .map(|login| Target::Url(format!("https://www.twitch.tv/{login}")))
                    })
            }
            ScheduleSourceKind::TwitterPinned => {
                let handle = load_channel_cfg(&self.core.store, s.channel_id).twitter_handle;
                let handle = handle.trim().trim_start_matches('@');
                (!handle.is_empty()).then(|| Target::Url(format!("https://x.com/{handle}")))
            }
            ScheduleSourceKind::OtherImageOcr => {
                let img = load_channel_cfg(&self.core.store, s.channel_id).other_image;
                let img = img.trim().to_string();
                if img.is_empty() {
                    None
                } else if img.starts_with("http://") || img.starts_with("https://") {
                    Some(Target::Url(img))
                } else {
                    Some(Target::Path(std::path::PathBuf::from(img)))
                }
            }
            ScheduleSourceKind::Discord => None,
        };
        match target {
            Some(Target::Url(url)) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Some(Target::Path(path)) => crate::platform::open_path(&path),
            None => self.status = "Couldn't resolve this item's source to open.".into(),
        }
    }
    /// Preview dialog for merging 2+ selected schedule events into one calendar entry.
    /// The user picks which event is the "primary" (its time/title/URL is shown);
    /// the others become hidden secondaries. The merge is stored in the DB via
    /// `merge_segments_manual`.
    #[allow(deprecated)]
    pub(super) fn merge_preview_window(&mut self, ctx: &egui::Context) {
        if self.merge_preview.is_none() {
            return;
        }
        let mut open = true;
        let mut do_merge = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("merge_preview_vp"),
            egui::ViewportBuilder::default()
                .with_title("Merge schedule events")
                .with_inner_size([520.0, 380.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                let Some(d) = self.merge_preview.as_mut() else {
                    return;
                };
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Select which event is the primary. The primary's time, title and URL will be shown on the calendar; the others will be hidden as part of the group.");
                    ui.add_space(8.0);

                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for (i, s) in d.segments.iter().enumerate() {
                            let (src_badge, src_label) = source_badge(&s.source);
                            let is_primary = d.primary_idx == i;
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut d.primary_idx, i, "");
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        if is_primary {
                                            ui.strong("★ Primary");
                                        } else {
                                            ui.weak("Secondary");
                                        }
                                        ui.label(format!("· {src_badge} {src_label}"));
                                    });
                                    ui.label(format!(
                                        "{} – {}",
                                        fmt_datetime_short(s.start_time),
                                        s.end_time
                                            .map(|e| fmt_datetime_short(e))
                                            .unwrap_or_else(|| "?".into()),
                                    ));
                                    if !s.title.is_empty() {
                                        ui.label(format!("Title: {}", s.title));
                                    }
                                    if !s.category.is_empty() {
                                        ui.weak(format!("Category: {}", s.category));
                                    }
                                });
                            });
                            ui.separator();
                        }
                    });

                    if !d.error.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(HL_ERROR_TEXT, &d.error);
                    }

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("🔀 Merge").clicked() {
                            do_merge = true;
                        }
                        if ui.button("Cancel").clicked() {
                            do_cancel = true;
                        }
                    });
                });
            },
        );

        if do_merge {
            if let Some(d) = self.merge_preview.take() {
                let primary_id = d.segments[d.primary_idx].segment_id;
                let secondary_ids: Vec<i64> = d
                    .segments
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != d.primary_idx)
                    .map(|(_, s)| s.segment_id)
                    .collect();
                match self.core.store.merge_segments_manual(primary_id, &secondary_ids) {
                    Ok(()) => {
                        self.schedule_selected.clear();
                        self.spawn_reload_schedule();
                        self.status = format!(
                            "Merged {} events.",
                            secondary_ids.len() + 1
                        );
                    }
                    Err(e) => {
                        // Re-open the dialog with the error shown.
                        self.merge_preview = Some(MergePreviewDraft {
                            segments: d.segments,
                            primary_idx: d.primary_idx,
                            error: format!("Error merging: {e:#}"),
                        });
                    }
                }
            }
        } else if do_cancel || !open {
            self.merge_preview = None;
        }
    }

    /// Confirmation dialog for deleting multiple selected schedule events at once.
    #[allow(deprecated)]
    pub(super) fn confirm_delete_segments_window(&mut self, ctx: &egui::Context) {
        let Some(ids) = self.confirm_delete_segments.as_ref() else {
            return;
        };
        let count = ids.len();
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_segments_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete schedule items")
                .with_inner_size([400.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(format!(
                        "Permanently delete {count} selected schedule item{}?",
                        if count == 1 { "" } else { "s" }
                    ));
                    ui.label("They will be suppressed and won't reappear on refresh.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(egui::RichText::new("🗑 Delete").color(HL_ERROR_TEXT))
                            .clicked()
                        {
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
            if let Some(ids) = self.confirm_delete_segments.take() {
                let mut errors = 0usize;
                for sid in &ids {
                    match self.core.store.delete_schedule_segment(*sid) {
                        Ok(_) => {
                            self.schedule_hidden_segments.remove(sid);
                        }
                        Err(e) => {
                            warn!("Error deleting schedule segment {sid}: {e:#}");
                            errors += 1;
                        }
                    }
                }
                self.schedule_selected.clear();
                self.spawn_reload_schedule();
                if errors == 0 {
                    self.status = format!(
                        "Deleted {count} schedule item{}.",
                        if count == 1 { "" } else { "s" }
                    );
                } else {
                    self.status = format!(
                        "Deleted {} of {count} items; {errors} error(s) — check logs.",
                        count - errors
                    );
                }
            }
        } else if do_cancel || !open {
            self.confirm_delete_segments = None;
        }
    }

    /// `"manual"` so a later automatic refresh leaves the correction intact.
    #[allow(deprecated)] // CentralPanel::show inside a viewport (matches the other dialogs)
    pub(super) fn edit_schedule_window(&mut self, ctx: &egui::Context) {
        if self.edit_schedule.is_none() {
            return;
        }
        let mut open = true;
        // Actions collected inside the closure, applied after it (the closure
        // borrows the draft mutably; these touch the store / reload).
        let mut do_save = false;
        let mut do_delete = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("edit_schedule_vp"),
            egui::ViewportBuilder::default()
                .with_title("Edit schedule item")
                .with_inner_size([440.0, 320.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                let Some(d) = self.edit_schedule.as_mut() else {
                    return;
                };
                egui::CentralPanel::default().show(ctx, |ui| {
                    let (badge, label) = source_badge(&d.source);
                    ui.horizontal(|ui| {
                        ui.strong(&d.channel_name);
                        ui.weak(format!("· source: {badge} {label}"));
                    });
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(
                            "Times are in your local timezone. Saving marks this item as manually \
                             edited so automatic refreshes won't overwrite it.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(8.0);
                    egui::Grid::new("edit_sched_grid")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Title");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.title).desired_width(300.0),
                            );
                            ui.end_row();

                            ui.label("Category");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.category)
                                    .hint_text("optional")
                                    .desired_width(300.0),
                            );
                            ui.end_row();

                            ui.label("Start");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut d.date)
                                        .hint_text("YYYY-MM-DD")
                                        .desired_width(110.0),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut d.time)
                                        .hint_text("HH:MM")
                                        .desired_width(70.0),
                                );
                            });
                            ui.end_row();

                            ui.label("End");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut d.end_date)
                                        .hint_text("YYYY-MM-DD")
                                        .desired_width(110.0),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut d.end_time)
                                        .hint_text("HH:MM")
                                        .desired_width(70.0),
                                );
                                ui.weak("(optional)");
                            });
                            ui.end_row();
                        });

                    if !d.error.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(HL_ERROR_TEXT, &d.error);
                    }

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("💾  Save").clicked() {
                            do_save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                        ui.add_space(16.0);
                        if ui
                            .button(egui::RichText::new("🗑  Delete").color(HL_ERROR_TEXT))
                            .on_hover_text("Remove this occurrence from the calendar.")
                            .clicked()
                        {
                            do_delete = true;
                        }
                    });
                });
            },
        );

        if do_delete {
            if let Some(d) = self.edit_schedule.take() {
                match self.core.store.delete_schedule_segment(d.segment_id) {
                    Err(e) => self.status = format!("Error deleting item: {e}"),
                    Ok(0) => {
                        self.spawn_reload_schedule();
                        self.status = "Schedule item was already gone.".into();
                    }
                    Ok(_) => {
                        self.spawn_reload_schedule();
                        self.status = "Schedule item deleted.".into();
                    }
                }
            }
            return;
        }

        if do_save {
            // Validate against the draft, writing the error back into it on failure
            // (so the dialog stays open and shows why).
            let parsed = self.edit_schedule.as_ref().and_then(|d| {
                let start = parse_local_datetime(&d.date, &d.time)?;
                // End is optional; when both fields are blank there's no end. When
                // partially filled or unparseable, treat as an error below. A start
                // before today's local midnight is rejected up front: the calendar
                // only loads start ≥ today, so saving a past time would silently
                // drop the row from every view despite a "success" message.
                let end = if start < today_start_unix() {
                    Err("Start must be today or later.")
                } else if d.end_date.trim().is_empty() && d.end_time.trim().is_empty() {
                    Ok(None)
                } else {
                    match parse_local_datetime(&d.end_date, &d.end_time) {
                        Some(e) if e > start => Ok(Some(e)),
                        Some(_) => Err("End must be after start."),
                        None => Err("End date/time is invalid."),
                    }
                };
                Some((d.segment_id, start, end, d.title.trim().to_string(), d.category.trim().to_string()))
            });
            match parsed {
                None => {
                    if let Some(d) = self.edit_schedule.as_mut() {
                        d.error = "Start date/time is invalid (use YYYY-MM-DD and HH:MM).".into();
                    }
                }
                Some((_, _, Err(msg), _, _)) => {
                    if let Some(d) = self.edit_schedule.as_mut() {
                        d.error = msg.into();
                    }
                }
                Some((id, start, Ok(end), title, category)) if !title.is_empty() => {
                    match self.core.store.update_schedule_segment_manual(
                        id, start, end, &title, &category,
                    ) {
                        Ok(0) => {
                            if let Some(d) = self.edit_schedule.as_mut() {
                                d.error =
                                    "This item no longer exists (a refresh may have cleared it).".into();
                            }
                        }
                        Ok(_) => {
                            self.edit_schedule = None;
                            self.spawn_reload_schedule();
                            self.status = "Schedule item updated (marked manual).".into();
                        }
                        Err(e) => {
                            if let Some(d) = self.edit_schedule.as_mut() {
                                d.error = format!("Error saving: {e}");
                            }
                        }
                    }
                }
                Some(_) => {
                    if let Some(d) = self.edit_schedule.as_mut() {
                        d.error = "Title can't be empty.".into();
                    }
                }
            }
        }

        if !open {
            self.edit_schedule = None;
        }
    }

    /// Add/Edit dialog for a [`ScheduledRecording`] (schema v51).
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn scheduled_recording_form_window(&mut self, ctx: &egui::Context) {
        if self.scheduled_recording_form.is_none() {
            return;
        }
        let mut open = true;
        let mut do_save = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("sched_rec_form_vp"),
            egui::ViewportBuilder::default()
                .with_title("Schedule recording")
                .with_inner_size([460.0, 420.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                let Some(f) = self.scheduled_recording_form.as_mut() else {
                    return;
                };
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(&f.channel_name);
                        ui.weak(format!("· {}", f.monitor_url));
                    });
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(
                            "Force-starts a recording at the scheduled time, bypassing Auto — \
                             works even with Auto off or Detection set to Disabled.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(8.0);
                    egui::Grid::new("sched_rec_form_grid")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Label");
                            ui.add(
                                egui::TextEdit::singleline(&mut f.label)
                                    .hint_text("optional")
                                    .desired_width(300.0),
                            );
                            ui.end_row();

                            ui.label("Repeats");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut f.kind, RecurrenceKind::Once, "Once");
                                ui.selectable_value(&mut f.kind, RecurrenceKind::Weekly, "Weekly");
                            });
                            ui.end_row();

                            match f.kind {
                                RecurrenceKind::Once => {
                                    ui.label("Start");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut f.date)
                                                .hint_text("YYYY-MM-DD")
                                                .desired_width(110.0),
                                        );
                                        ui.add(
                                            egui::TextEdit::singleline(&mut f.time)
                                                .hint_text("HH:MM")
                                                .desired_width(70.0),
                                        );
                                    });
                                    ui.end_row();
                                }
                                RecurrenceKind::Weekly => {
                                    ui.label("Days");
                                    ui.horizontal(|ui| {
                                        for (i, name) in
                                            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                                                .iter()
                                                .enumerate()
                                        {
                                            ui.checkbox(&mut f.days[i], *name);
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("Time of day");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut f.weekly_time)
                                            .hint_text("HH:MM")
                                            .desired_width(70.0),
                                    );
                                    ui.end_row();

                                    ui.label("Until");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut f.until_date)
                                            .hint_text("YYYY-MM-DD, optional")
                                            .desired_width(140.0),
                                    );
                                    ui.end_row();
                                }
                            }

                            ui.label("Duration");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut f.use_duration, "Auto-stop after");
                                ui.add_enabled(
                                    f.use_duration,
                                    egui::TextEdit::singleline(&mut f.duration_minutes)
                                        .desired_width(50.0),
                                );
                                ui.label("minutes");
                            });
                            ui.end_row();
                            if !f.use_duration {
                                ui.label("");
                                ui.weak("Records until the stream ends naturally.");
                                ui.end_row();
                            }

                            ui.label("Enabled");
                            ui.checkbox(&mut f.enabled, "");
                            ui.end_row();
                        });

                    if !f.error.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(HL_ERROR_TEXT, &f.error);
                    }

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("💾  Save").clicked() {
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
            self.save_scheduled_recording_form();
            return;
        }
        if !open {
            self.scheduled_recording_form = None;
        }
    }

    /// Validates a [`ScheduledRecordingForm`] and derives the DB fields, probing
    /// the very first occurrence with the same recurrence math the background
    /// job uses (`compute_next_run`) so a dead-on-arrival rule — e.g. a `Weekly`
    /// rule whose `until` has already passed — is caught here instead of
    /// silently never firing.
    #[allow(clippy::type_complexity)]
    pub(super) fn build_scheduled_recording_fields(
        f: &ScheduledRecordingForm,
        now: i64,
    ) -> Result<(RecurrenceKind, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64), &'static str> {
        let label = f.label.trim().to_string();
        let (start_at, days_of_week, time_of_day_secs, until) = match f.kind {
            RecurrenceKind::Once => {
                let start = parse_local_datetime(&f.date, &f.time)
                    .ok_or("Start date/time is invalid (use YYYY-MM-DD and HH:MM).")?;
                if start <= now {
                    return Err("Start must be in the future.");
                }
                (Some(start), None, None, None)
            }
            RecurrenceKind::Weekly => {
                let bits: i64 = f
                    .days
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| **d)
                    .map(|(i, _)| 1i64 << i)
                    .sum();
                if bits == 0 {
                    return Err("Pick at least one day.");
                }
                let tod = parse_time_of_day(&f.weekly_time)
                    .ok_or("Time of day is invalid (use HH:MM).")?;
                let until = if f.until_date.trim().is_empty() {
                    None
                } else {
                    Some(
                        parse_local_datetime(&f.until_date, "23:59:59")
                            .ok_or("Until date is invalid (use YYYY-MM-DD).")?,
                    )
                };
                (None, Some(bits), Some(tod), until)
            }
        };
        let duration_secs = if f.use_duration {
            let m: i64 = f
                .duration_minutes
                .trim()
                .parse()
                .ok()
                .filter(|m| *m > 0)
                .ok_or("Duration must be a whole number of minutes greater than 0.")?;
            Some(m * 60)
        } else {
            None
        };
        let probe = ScheduledRecording {
            id: 0,
            monitor_id: f.monitor_id,
            label: label.clone(),
            kind: f.kind,
            start_at,
            days_of_week,
            time_of_day_secs,
            until,
            duration_secs,
            enabled: true,
            next_run_at: None,
            last_fired_at: None,
            pending_stop_at: None,
            created_at: 0,
        };
        let next_run_at = crate::scheduled_recordings::compute_next_run(&probe, now)
            .ok_or("This rule would never fire (check the days/until date).")?;
        Ok((f.kind, label, start_at, days_of_week, time_of_day_secs, until, duration_secs, next_run_at))
    }

    pub(super) fn save_scheduled_recording_form(&mut self) {
        let Some(f) = self.scheduled_recording_form.as_ref() else {
            return;
        };
        let now = now_unix();
        let built = Self::build_scheduled_recording_fields(f, now);
        let monitor_id = f.monitor_id;
        let id = f.id;
        let enabled = f.enabled;
        match built {
            Err(msg) => {
                if let Some(f) = self.scheduled_recording_form.as_mut() {
                    f.error = msg.to_string();
                }
            }
            Ok((kind, label, start_at, days_of_week, time_of_day_secs, until, duration_secs, next_run_at)) => {
                let result = match id {
                    None => self
                        .core
                        .store
                        .insert_scheduled_recording(
                            monitor_id, &label, kind, start_at, days_of_week, time_of_day_secs,
                            until, duration_secs, Some(next_run_at),
                        )
                        .and_then(|new_id| {
                            if enabled {
                                Ok(())
                            } else {
                                self.core.store.set_scheduled_recording_enabled(new_id, false, None)
                            }
                        }),
                    Some(id) => self.core.store.update_scheduled_recording(
                        id, &label, kind, start_at, days_of_week, time_of_day_secs, until,
                        duration_secs, enabled, Some(next_run_at),
                    ),
                };
                match result {
                    Ok(()) => {
                        self.scheduled_recording_form = None;
                        self.reload_rows();
                        self.status = "Scheduled recording saved.".into();
                    }
                    Err(e) => {
                        if let Some(f) = self.scheduled_recording_form.as_mut() {
                            f.error = format!("Error saving: {e}");
                        }
                    }
                }
            }
        }
    }
    /// The scheduled-recordings management window: list (Channel/Instance/
    /// Recurrence/Next run/Duration/Enabled) with Edit/Delete row actions and
    /// an "+ Add new" picker over every known instance.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn scheduled_recordings_window(&mut self, ctx: &egui::Context) {
        if !self.show_scheduled_recordings {
            return;
        }
        let mut open = true;
        enum Act {
            Edit(i64),
            Delete(i64, String),
            ToggleEnabled(i64, bool),
            AddNew(i64),
        }
        let mut act: Option<Act> = None;
        // "+ Add new" instance picker, session-only (not persisted).
        let mut add_new_monitor = self.rows.first().map(|r| r.monitor.id).unwrap_or(0);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("sched_recs_vp"),
            egui::ViewportBuilder::default()
                .with_title("📅 Scheduled recordings")
                .with_inner_size([720.0, 420.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} scheduled recording(s)", self.scheduled_recordings.len()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            egui::ComboBox::from_id_salt("sched_rec_add_monitor")
                                .selected_text(
                                    self.rows
                                        .iter()
                                        .find(|r| r.monitor.id == add_new_monitor)
                                        .map(|r| format!("{} — {}", r.channel.name, r.monitor.url))
                                        .unwrap_or_else(|| "(no instances yet)".to_string()),
                                )
                                .show_ui(ui, |ui| {
                                    for r in &self.rows {
                                        ui.selectable_value(
                                            &mut add_new_monitor,
                                            r.monitor.id,
                                            format!("{} — {}", r.channel.name, r.monitor.url),
                                        );
                                    }
                                });
                            if ui
                                .add_enabled(!self.rows.is_empty(), egui::Button::new("➕ Add new"))
                                .clicked()
                            {
                                act = Some(Act::AddNew(add_new_monitor));
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new("sched_recs_grid")
                            .num_columns(6)
                            .striped(true)
                            .spacing([10.0, 4.0])
                            .show(ui, |ui| {
                                ui.strong("Channel");
                                ui.strong("Instance");
                                ui.strong("Recurrence");
                                ui.strong("Next run");
                                ui.strong("Duration");
                                ui.strong("");
                                ui.end_row();
                                for row in &self.scheduled_recordings {
                                    let r = &row.rec;
                                    ui.label(&row.channel_name);
                                    ui.label(&row.monitor_url);
                                    ui.label(describe_recurrence(r));
                                    ui.label(match r.next_run_at {
                                        Some(t) if r.enabled => fmt_datetime_short(t),
                                        _ => "—".to_string(),
                                    });
                                    ui.label(match r.duration_secs {
                                        Some(d) => format!("{} min", d / 60),
                                        None => "until stream ends".to_string(),
                                    });
                                    ui.horizontal(|ui| {
                                        let mut enabled = r.enabled;
                                        if ui.checkbox(&mut enabled, "").changed() {
                                            act = Some(Act::ToggleEnabled(r.id, enabled));
                                        }
                                        if ui.button("✏").on_hover_text("Edit").clicked() {
                                            act = Some(Act::Edit(r.id));
                                        }
                                        if ui.button("🗑").on_hover_text("Delete").clicked() {
                                            act = Some(Act::Delete(
                                                r.id,
                                                format!("{} — {}", row.channel_name, row.monitor_url),
                                            ));
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
                });
            },
        );

        match act {
            Some(Act::AddNew(monitor_id)) => {
                if let Some(row) = self.rows.iter().find(|r| r.monitor.id == monitor_id) {
                    self.scheduled_recording_form = Some(ScheduledRecordingForm::new_for_monitor(
                        monitor_id,
                        &row.channel.name,
                        &row.monitor.url,
                    ));
                }
            }
            Some(Act::Edit(id)) => {
                if let Some(row) = self.scheduled_recordings.iter().find(|r| r.rec.id == id) {
                    self.scheduled_recording_form = Some(ScheduledRecordingForm::from_existing(row));
                }
            }
            Some(Act::Delete(id, name)) => {
                self.confirm_delete_scheduled_recording = Some((id, name));
            }
            Some(Act::ToggleEnabled(id, enabled)) => {
                let next_run_at = if enabled {
                    self.scheduled_recordings
                        .iter()
                        .find(|r| r.rec.id == id)
                        .and_then(|r| crate::scheduled_recordings::compute_next_run(&r.rec, now_unix()))
                } else {
                    None
                };
                match self.core.store.set_scheduled_recording_enabled(id, enabled, next_run_at) {
                    Ok(()) => self.reload_rows(),
                    Err(e) => self.status = format!("Error: {e}"),
                }
            }
            None => {}
        }

        if !open {
            self.show_scheduled_recordings = false;
        }
    }

    /// Modal confirmation for deleting a scheduled recording.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn confirm_delete_scheduled_recording_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete_scheduled_recording.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_sched_rec_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete scheduled recording")
                .with_inner_size([380.0, 120.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(format!("Delete the scheduled recording for “{name}”?"));
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
            match self.core.store.delete_scheduled_recording(id) {
                Ok(()) => {
                    self.confirm_delete_scheduled_recording = None;
                    self.reload_rows();
                    self.status = "Scheduled recording deleted.".into();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
        } else if do_cancel || !open {
            self.confirm_delete_scheduled_recording = None;
        }
    }
}
