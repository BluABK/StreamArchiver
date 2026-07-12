//! Videos view: download list/form, VOD recovery, format probe/designer.

use super::*;

/// State of the on-demand "List formats" probe (Videos tab), shown in a window.
#[derive(Clone)]
pub(super) enum FormatProbe {
    Idle,
    Running,
    Done(String),
    Failed(String),
}

/// A self-mutating action picked from a video row's context menu (open/copy are
/// handled inline; these need deferred access to `self`).
pub(super) enum VideoMenuChoice {
    Stop,
    Retry,
    Delete,
}

/// Backing state for the "Recover VOD" dialog (CDN-derive a deleted/muted Twitch
/// VOD). Opened auto-filled from a tracked recording, or blank for manual entry.
pub(super) struct RecoverVodForm {
    pub(super) login: String,
    pub(super) broadcast_id: String,
    /// "YYYY-MM-DD HH:MM:SS", interpreted as UTC.
    pub(super) start_utc: String,
    pub(super) went_live_approx: bool,
    /// Optional pasted twitch / twitchtracker / streamscharts / sullygnome URL.
    pub(super) url_paste: String,
    /// "" = auto/source, else a resolution like "720p60".
    pub(super) quality: String,
    /// `Some(rec_id)` = attach to that recording; `None` = standalone (Videos list).
    pub(super) rec_id: Option<i64>,
    /// Standalone output dir (ignored when `rec_id` is `Some`).
    pub(super) output_dir: String,
    /// Deleted VOD (probe every segment) vs merely muted (probe only muted ones).
    pub(super) deleted: bool,
    /// The `/videos/<id>` archive id when known (enables the GQL fast-path).
    pub(super) vod_id: Option<String>,
}

/// State of the async Recover-VOD CDN probe (the dry-run before downloading).
#[derive(Clone)]
pub(super) enum RecoverProbe {
    Idle,
    Running,
    Found {
        host: String,
        matched_epoch: i64,
        qualities: Vec<String>,
        total: usize,
        present: usize,
        unmuted: usize,
        missing: usize,
    },
    NotFound(String),
    Failed(String),
}

/// State of the async "Parse URL" lookup (third-party start-time scrape, or the
/// Twitch GQL fast-path for a pasted `/videos/` URL).
#[derive(Clone)]
pub(super) enum RecoverScrape {
    Idle,
    Running,
    /// Tracker scrape resolved just the start epoch (UTC seconds).
    Filled(i64),
    /// GQL resolved the whole VOD (login + broadcast id + start).
    FilledFull { login: String, broadcast_id: String, start_epoch: i64 },
    Failed(String),
}
/// Which field the Format Designer was opened from (for "Apply").
#[derive(Clone, PartialEq)]
pub(super) enum FormatDesignerTarget {
    MonitorForm,
    VideoForm,
}

/// State for the floating Format Designer window.
pub(super) struct FormatDesignerState {
    pub(super) template: String,
    pub(super) selected_monitor_idx: usize,
    /// Recordings for the currently selected monitor (oldest-first from the store).
    pub(super) recordings: Vec<Recording>,
    pub(super) selected_recording_idx: usize,
    /// Which field opened the designer (None = standalone / no write-back).
    pub(super) target: Option<FormatDesignerTarget>,
    /// In-flight background load of recordings for the selected monitor. Drained
    /// each frame in `format_designer_window`; avoids blocking the UI thread.
    pub(super) recordings_load: Option<std::sync::mpsc::Receiver<(Vec<Recording>, usize)>>,
}

impl FormatDesignerState {
    pub(super) fn new(template: String, target: Option<FormatDesignerTarget>) -> Self {
        Self {
            template,
            selected_monitor_idx: 0,
            recordings: Vec::new(),
            selected_recording_idx: 0,
            target,
            recordings_load: None,
        }
    }
}

impl StreamArchiverApp {
    /// The "Videos" tab: a list of on-demand downloads with an always-visible
    /// "paste a URL" form pinned to the bottom.
    pub(super) fn videos_view(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("video_add_panel")
            .resizable(true)
            .default_size(300.0)
            .show_inside(ui, |ui| {
                // Per-platform defaults on the right; download form on the left.
                egui::Panel::right("video_defaults_panel")
                    .resizable(true)
                    .default_size(360.0)
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("video_defaults_scroll")
                            .show(ui, |ui| {
                                self.video_defaults_editor(ui);
                            });
                    });
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("video_form_scroll")
                        .show(ui, |ui| {
                            self.video_add_form(ui);
                        });
                });
            });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.videos_list(ui);
        });
    }

    /// Collapsible per-platform download defaults editor (Twitch / YouTube /
    /// Kick / Generic). Edits persist immediately; the download form below
    /// pre-fills from these per detected platform.
    pub(super) fn video_defaults_editor(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;
        // Collect a Browse request inside the loop (where `defs` borrows
        // `self.download_defaults`) and spawn it AFTER the loop releases
        // the borrow, so we can reach `self.pending_browse`.
        // (true = folder, false = file), platform, current path.
        let mut browse_req: Option<(bool, Platform, String)> = None;
        // Preset actions collected inside the loop and applied after all borrows end.
        let mut preset_delete: Option<i64> = None;
        let mut preset_save_tmpl: Option<String> = None;
        // Snapshot custom presets as a slice; separate from `defs` borrow (different field).
        let custom_presets = self.custom_presets.as_slice();
        // Borrow the defaults (not `self`) so the nested egui closures don't
        // alias `self`; persist afterwards.
        let defs = &mut self.download_defaults;

        ui.add_space(6.0);
        ui.strong("⚙  Per-platform defaults");
        ui.label(
            egui::RichText::new(
                "Downloads pre-fill from these by platform; override per download.",
            )
            .small()
            .color(egui::Color32::from_gray(0x90)),
        );
        ui.add_space(4.0);

        for platform in Platform::ALL {
            egui::CollapsingHeader::new(platform.label())
                .id_salt(("dl_def", platform.as_str()))
                .show(ui, |ui| {
                    let d = defs.get_mut(platform);
                    egui::Grid::new(("dl_def_grid", platform.as_str()))
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Tool").on_hover_text(d.tool.tooltip());
                            egui::ComboBox::from_id_salt(("dl_tool", platform.as_str()))
                                .selected_text(d.tool.label())
                                .show_ui(ui, |ui| {
                                    for t in Tool::ALL {
                                        if ui
                                            .selectable_value(&mut d.tool, t, t.label())
                                            .on_hover_text(t.tooltip())
                                            .changed()
                                        {
                                            dirty = true;
                                        }
                                    }
                                });
                            ui.end_row();

                            ui.label("Quality");
                            if ui.text_edit_singleline(&mut d.quality).changed() {
                                dirty = true;
                            }
                            ui.end_row();

                            ui.label("Auth");
                            egui::ComboBox::from_id_salt(("dl_auth", platform.as_str()))
                                .selected_text(d.auth_kind.label())
                                .show_ui(ui, |ui| {
                                    for k in AuthKind::ALL {
                                        if ui
                                            .selectable_value(&mut d.auth_kind, k, k.label())
                                            .changed()
                                        {
                                            dirty = true;
                                        }
                                    }
                                });
                            ui.end_row();

                            match d.auth_kind {
                                AuthKind::CookiesBrowser => {
                                    ui.label("Browser");
                                    if ui
                                        .text_edit_singleline(&mut d.auth_value)
                                        .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (the folder under …/Firefox/Profiles, or an absolute path)")
                                        .changed()
                                    {
                                        dirty = true;
                                    }
                                    ui.end_row();
                                }
                                AuthKind::CookiesFile => {
                                    ui.label("Cookies file");
                                    ui.horizontal(|ui| {
                                        if ui.text_edit_singleline(&mut d.auth_value).changed() {
                                            dirty = true;
                                        }
                                        if ui.button("Browse…").clicked() {
                                            browse_req = Some((false, platform, d.auth_value.clone()));
                                        }
                                    });
                                    ui.end_row();
                                }
                                AuthKind::Token => {
                                    ui.label("Auth token");
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut d.auth_value)
                                                .password(true),
                                        )
                                        .changed()
                                    {
                                        dirty = true;
                                    }
                                    ui.end_row();
                                }
                                AuthKind::Inherit | AuthKind::Disabled => {}
                            }

                            ui.label("Output folder");
                            ui.horizontal(|ui| {
                                if ui.text_edit_singleline(&mut d.output_dir).changed() {
                                    dirty = true;
                                }
                                if ui.button("Browse…").clicked() {
                                    browse_req = Some((true, platform, d.output_dir.clone()));
                                }
                            });
                            ui.end_row();

                            ui.label("Filename template");
                            ui.horizontal(|ui| {
                                let before = d.filename_template.clone();
                                let (del, save) = filename_preset_combo(
                                    ui,
                                    &format!("vdef_tmpl_{}", platform.as_str()),
                                    &mut d.filename_template,
                                    custom_presets,
                                );
                                if del.is_some() { preset_delete = del; }
                                if save { preset_save_tmpl = Some(d.filename_template.clone()); }
                                if ui.text_edit_singleline(&mut d.filename_template).changed()
                                    || d.filename_template != before
                                {
                                    dirty = true;
                                }
                            });
                            ui.end_row();

                            ui.label("Extra args");
                            if ui.text_edit_singleline(&mut d.extra_args).changed() {
                                dirty = true;
                            }
                            ui.end_row();

                            ui.label("Detect title");
                            if ui.checkbox(&mut d.auto_title, "Detect title + channel")
                                .on_hover_text(
                                    "Default state of the \"Detect title + channel\" checkbox \
                                     for new downloads on this platform.",
                                )
                                .changed()
                            {
                                dirty = true;
                            }
                            ui.end_row();
                        });
                });
        }
        if dirty {
            self.persist_download_defaults();
        }
        // defs and custom_presets borrows end; now we can mutate self freely.
        if let Some(id) = preset_delete {
            if let Err(e) = self.core.store.delete_filename_preset(id) {
                self.status = format!("Error deleting preset: {e:#}");
            } else {
                self.custom_presets = self.core.store.get_filename_presets().unwrap_or_default();
            }
        }
        if let Some(tmpl) = preset_save_tmpl {
            self.save_preset_dialog = Some(SavePresetDraft {
                template: tmpl,
                name: String::new(),
                error: String::new(),
            });
        }
        if let Some((is_folder, plat, current)) = browse_req {
            self.pending_browse = Some(if is_folder {
                spawn_browse_folder(&current, move |app, p| {
                    app.download_defaults.get_mut(plat).output_dir = p;
                    app.persist_download_defaults();
                })
            } else {
                spawn_browse_file(&current, move |app, p| {
                    app.download_defaults.get_mut(plat).auth_value = p;
                    app.persist_download_defaults();
                })
            });
        }
    }

    pub(super) fn videos_list(&mut self, ui: &mut egui::Ui) {
        // Reflect background progress while the list is shown — on a 1s TTL,
        // not the old full list_videos() SELECT every frame (events also
        // reload directly, so changes still land promptly).
        if self
            .videos_refreshed
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
        {
            self.reload_videos();
        }

        if self.videos.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No videos yet.");
                ui.label("Paste a URL in the box below to download a video or VOD.");
            });
            return;
        }

        let mut to_stop: Option<i64> = None;
        let mut to_retry: Option<i64> = None;
        let mut to_delete: Option<i64> = None;
        let any_active = self.videos.iter().any(|v| v.is_active());
        // Snapshot live download progress (video_id -> 0.0..=1.0) and speed
        // (video_id -> bytes/sec) for the progress bar + Speed column.
        let progress: std::collections::HashMap<i64, f32> =
            self.core.video_progress.lock().unwrap().clone();
        let speed: std::collections::HashMap<i64, f64> =
            self.core.video_speed.lock().unwrap().clone();

        // Build the sort/filter model and take the persisted sort/filter state
        // into locals (written back after the table is drawn).
        // Model cache: rebuilt when the list reloads or (while anything is
        // downloading) the second ticks for the speed cells — not every frame.
        let now_sec = crate::models::now_unix();
        let model_sec = if speed.is_empty() { 0 } else { now_sec };
        let model_stale = self
            .videos_model_cache
            .as_ref()
            .map(|(rev, sec, _)| *rev != self.videos_rev || *sec != model_sec)
            .unwrap_or(true);
        if model_stale {
            let m: Vec<Vec<Cell>> = self.videos.iter().map(|v| video_cells(v, &speed)).collect();
            self.videos_model_cache = Some((self.videos_rev, model_sec, m));
        }
        let model = &self.videos_model_cache.as_ref().unwrap().2;
        let mut sort = self.videos_sort.clone();
        let mut filters = self.videos_filters.clone();
        if filters.len() != VIDEO_COLS {
            filters = vec![String::new(); VIDEO_COLS];
        }
        // Platform favicons (uploaded once, cheaply cloned per frame) + whether to
        // tint rows by status — captured before the table closures borrow `self`.
        let ptex = self
            .platform_tex
            .get_or_insert_with(|| PlatformTextures::load(ui.ctx()))
            .clone();
        let status_bgcolor = self.status_bgcolor;
        // Whether the Actions column is shown (Settings → Display). Skipped in the
        // builder, header, and each row when off so the column counts match.
        let show_actions = self.show_actions;
        // Persisted column order/visibility, taken as a local copy (mutated by
        // the header's column-chooser context menu, written back + persisted
        // once at the tail of this function).
        let mut entries = self.videos_grid.entries.clone();
        let col_order = grid_columns::effective_order(&VIDEO_COLUMNS, &entries, |id| {
            id != "actions" || show_actions
        });
        // A pure reorder (column count unchanged) leaves egui_extras' width
        // cache stale — force one clean re-fit pass when the order just changed.
        let order_changed = self.videos_grid.note_order(&col_order);

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Non-selectable labels so a right-click reaches the row (menu).
                ui.style_mut().interaction.selectable_labels = false;
                let sel_color = ui.visuals().selection.bg_fill;
                let mut tb = TableBuilder::new(ui)
                    .id_salt("videos_table")
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                if order_changed {
                    tb.reset();
                }
                for &i in &col_order {
                    let c = &VIDEO_COLUMNS[i];
                    // A hide/show/reorder-forced reset restores each column to
                    // its last remembered width instead of snapping back to the
                    // declared default — see `WidthMemory` (`grid_columns.rs`).
                    let seed = self.videos_grid.widths.get(c.id);
                    let col = if c.stretch {
                        Column::remainder().at_least(c.min_width)
                    } else if order_changed && let Some(w) = seed {
                        Column::auto_with_initial_suggestion(w).at_least(c.min_width)
                    } else {
                        Column::auto().at_least(c.min_width)
                    };
                    tb = tb.column(col);
                }
                let mut want_reorder = false;
                let table = tb
                    .header(46.0, |mut header| {
                        for &i in &col_order {
                            let c = &VIDEO_COLUMNS[i];
                            let (rect, _) = header.col(|ui| {
                                if grid_header_cell(
                                    ui, GridTableId::Videos, i, c, true, &mut sort, &mut filters[i],
                                    &mut entries, &VIDEO_COLUMNS, |id| id == "actions",
                                ) {
                                    want_reorder = true;
                                }
                            });
                            self.videos_grid.widths.note(c.id, rect.width());
                        }
                    });
                if want_reorder {
                    self.reorder_columns = Some(ReorderColumnsState {
                        table: GridTableId::Videos,
                        draft: entries.clone(),
                    });
                }
                table.body(|body| {
                        let order = ordered_rows(model, &sort, &filters);
                        // Virtualized: only the rows in view are laid out.
                        body.rows(24.0, order.len(), |mut tr| {
                                let vi = order[tr.index()];
                                let v = &self.videos[vi];
                                // Tint by status (in-flight = accent, failed = red),
                                // honoring the top-bar "Status bgcolor" toggle
                                // (painted per cell — see `tint_cell`).
                                let tint = video_row_tint(&v.status, sel_color, status_bgcolor);
                                // Probed once per row through the TTL cache —
                                // an open context menu re-runs its closure
                                // every frame, and direct stats can block for
                                // seconds on a sleeping/network drive.
                                let row_file_ok = !v.output_path.is_empty()
                                    && self
                                        .fs_probes
                                        .is_file(std::path::Path::new(&v.output_path));
                                let row_dir_ok = self
                                    .fs_probes
                                    .is_dir(std::path::Path::new(&v.output_dir));
                                // Reusable menu body (a `Fn`), attached to the row and
                                // each inline action button so right-clicking anywhere
                                // on the row opens it. Open/copy are handled inline;
                                // self-mutating picks go through `menu_pick`.
                                let mut menu_pick: Option<VideoMenuChoice> = None;
                                let add_menu =
                                    |ui: &mut egui::Ui, pick: &mut Option<VideoMenuChoice>| {
                                        ui.set_min_width(180.0);
                                        if v.is_active() {
                                            if ui.button("⏹  Stop download").clicked() {
                                                *pick = Some(VideoMenuChoice::Stop);
                                                ui.close();
                                            }
                                        } else if ui.button("↻  Retry download").clicked() {
                                            *pick = Some(VideoMenuChoice::Retry);
                                            ui.close();
                                        }
                                        ui.separator();
                                        let file_ok = row_file_ok;
                                        if ui
                                            .add_enabled(file_ok, egui::Button::new("▶  Open file"))
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_path,
                                            ));
                                            ui.close();
                                        }
                                        let dir_ok = row_dir_ok;
                                        if ui
                                            .add_enabled(
                                                dir_ok,
                                                egui::Button::new("📂  Open folder"),
                                            )
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_dir,
                                            ));
                                            ui.close();
                                        }
                                        if ui.button("🔗  Copy URL").clicked() {
                                            ui.ctx().copy_text(v.url.clone());
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                !v.output_path.is_empty(),
                                                egui::Button::new("📋  Copy file path"),
                                            )
                                            .clicked()
                                        {
                                            ui.ctx().copy_text(v.output_path.clone());
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("🗑  Delete from list").clicked() {
                                            *pick = Some(VideoMenuChoice::Delete);
                                            ui.close();
                                        }
                                    };

                                for &ci in &col_order {
                                    tr.col(|ui| { tint_cell(ui, tint); match VIDEO_COLUMNS[ci].id {
                                        "video" => {
                                            let label = if v.title.trim().is_empty() {
                                                v.url.as_str()
                                            } else {
                                                v.title.as_str()
                                            };
                                            ui.label(label).on_hover_text(&v.url);
                                        }
                                        "channel" => {
                                            if !v.channel.is_empty() {
                                                ui.label(&v.channel).on_hover_text(&v.channel);
                                            }
                                        }
                                        "platform" => {
                                            platform_icon(ui, &ptex, v.platform)
                                                .on_hover_text(v.platform.label());
                                            ui.label(v.platform.label());
                                        }
                                        "tool" => {
                                            ui.label(v.tool.label()).on_hover_text(v.tool.tooltip());
                                        }
                                        "status" => match progress.get(&v.id) {
                                            Some(&f) if v.status == "downloading" => {
                                                ui.add(
                                                    egui::ProgressBar::new(f)
                                                        .desired_width(84.0)
                                                        .text(format!("{:.0}%", f * 100.0)),
                                                );
                                            }
                                            _ => {
                                                let resp = ui
                                                    .colored_label(video_status_color(&v.status), &v.status);
                                                if v.status == "failed" {
                                                    let mut msg = fail_hover(&v.log_excerpt);
                                                    if let Some(code) = v.exit_code {
                                                        msg = format!("{msg}\n(exit code {code})");
                                                    }
                                                    resp.on_hover_text(msg);
                                                }
                                            }
                                        },
                                        "speed" => {
                                            if v.status == "downloading" {
                                                if let Some(&bps) = speed.get(&v.id) {
                                                    if bps > 0.0 {
                                                        ui.label(fmt_speed(bps));
                                                    }
                                                }
                                            }
                                        }
                                        "size" => {
                                            if v.bytes > 0 {
                                                ui.label(fmt_bytes(v.bytes));
                                            }
                                        }
                                        "added" => {
                                            ui.label(fmt_date(v.created_at));
                                        }
                                        "file" => {
                                            if !v.output_path.is_empty() {
                                                ui.label(&v.output_path).on_hover_text(&v.output_path);
                                            }
                                        }
                                        "actions" => {
                                            ui.push_id(v.id, |ui| {
                                                let mut btns: Vec<egui::Response> = Vec::with_capacity(5);
                                                if v.is_active() {
                                                    let b =
                                                        ui.small_button("⏹").on_hover_text("Stop download");
                                                    if b.clicked() {
                                                        to_stop = Some(v.id);
                                                    }
                                                    btns.push(b);
                                                } else {
                                                    let b = ui
                                                        .small_button("↻")
                                                        .on_hover_text("Retry download");
                                                    if b.clicked() {
                                                        to_retry = Some(v.id);
                                                    }
                                                    btns.push(b);
                                                }
                                                let dir_ok = row_dir_ok;
                                                let b = ui
                                                    .add_enabled(dir_ok, egui::Button::new("📂").small())
                                                    .on_hover_text("Open output folder");
                                                if b.clicked() {
                                                    crate::platform::open_path(std::path::Path::new(
                                                        &v.output_dir,
                                                    ));
                                                }
                                                btns.push(b);
                                                let file_ok = row_file_ok;
                                                let b = ui
                                                    .add_enabled(file_ok, egui::Button::new("▶").small())
                                                    .on_hover_text("Open file");
                                                if b.clicked() {
                                                    crate::platform::open_path(std::path::Path::new(
                                                        &v.output_path,
                                                    ));
                                                }
                                                btns.push(b);
                                                let b = ui.small_button("📋").on_hover_text("Copy URL");
                                                if b.clicked() {
                                                    ui.ctx().copy_text(v.url.clone());
                                                }
                                                btns.push(b);
                                                let b = ui
                                                    .small_button("🗑")
                                                    .on_hover_text("Delete from list (keeps the file)");
                                                if b.clicked() {
                                                    to_delete = Some(v.id);
                                                }
                                                btns.push(b);
                                                // Buttons swallow the right-click, so give each
                                                // the row menu too.
                                                for b in &btns {
                                                    b.context_menu(|ui| add_menu(ui, &mut menu_pick));
                                                }
                                            });
                                        }
                                        _ => {}
                                    }});
                                }

                                // Right-click anywhere on the row opens the menu.
                                tr.response()
                                    .context_menu(|ui| add_menu(ui, &mut menu_pick));

                                match menu_pick {
                                    Some(VideoMenuChoice::Stop) => to_stop = Some(v.id),
                                    Some(VideoMenuChoice::Retry) => to_retry = Some(v.id),
                                    Some(VideoMenuChoice::Delete) => to_delete = Some(v.id),
                                    None => {}
                                }
                        });
                    });
            });
        if sort != self.videos_sort {
            let keys: Vec<(usize, bool)> = sort.keys.iter().map(|l| (l.col, l.ascending)).collect();
            let persisted = grid_columns::unresolve_sort(&VIDEO_COLUMNS, &keys);
            grid_columns::save_sort(&self.core.store, GridTableId::Videos, &persisted);
        }
        self.videos_sort = sort;
        self.videos_filters = filters;
        if entries != self.videos_grid.entries {
            self.videos_grid.entries = entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Videos, &self.videos_grid.entries);
        }

        // Tick while a download is queued/running so the progress bar, status and
        // size update live (a bit faster than 1s for a smoother bar).
        if any_active {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }

        if let Some(id) = to_stop {
            self.core.manual(ManualCommand::StopVideo(id));
            self.status = "Stopping download…".into();
        }
        if let Some(id) = to_retry {
            match self.core.store.reset_video_for_retry(id) {
                Ok(()) => {
                    self.core.manual(ManualCommand::StartVideo(id));
                    self.status = "Re-queued download.".into();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
            self.reload_videos();
        }
        if let Some(id) = to_delete {
            if let Err(e) = self.core.store.delete_video(id) {
                self.status = format!("Error: {e}");
            }
            self.reload_videos();
        }
    }

    /// The bottom "paste a URL + settings + Download" form on the Videos tab.
    pub(super) fn video_add_form(&mut self, ui: &mut egui::Ui) {
        let platform = Platform::detect(&self.video_form.url);

        // Re-fill the form from this platform's saved defaults whenever the
        // detected platform changes; the user can then override any field.
        if self.video_form.last_platform != Some(platform) {
            let d = self.download_defaults.get(platform).clone();
            let vf = &mut self.video_form;
            vf.tool = d.tool;
            vf.tool_binary = String::new();
            vf.quality = d.quality;
            vf.output_dir = d.output_dir;
            vf.filename_template = d.filename_template;
            vf.extra_args = d.extra_args;
            vf.auto_title = d.auto_title;
            vf.auth_override = None; // "Default (per-platform)"
            vf.auth_value = String::new();
            // Snapshot the default auth too, so a later edit to the default
            // doesn't desync from the other (already snapshotted) fields.
            vf.default_auth_kind = d.auth_kind;
            vf.default_auth_value = d.auth_value;
            vf.last_platform = Some(platform);
        }

        let mut do_download = false;
        let mut do_list_formats = false;
        let mut do_recover_vod = false;
        let mut open_vf_designer = false;
        let mut vf_preset_delete: Option<i64> = None;
        let mut vf_preset_save_tmpl: Option<String> = None;

        {
            let custom_presets = self.custom_presets.as_slice();
            let custom_tool_aliases: Vec<String> =
                self.settings.custom_tools.iter().map(|t| t.alias.clone()).collect();
            let vf = &mut self.video_form;

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Download a video / VOD");
                ui.label(
                    egui::RichText::new("→ MKV")
                        .small()
                        .color(egui::Color32::from_gray(0x90)),
                );
            });

            // Four columns (label · field · label · field) so two fields share a
            // row — the form flows across the available width instead of stacking
            // into a tall, scrolling single column.
            egui::Grid::new("video_form_grid")
                .num_columns(4)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    // URL (wide) + Name.
                    ui.label("URL");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut vf.url)
                                .desired_width(340.0)
                                .hint_text(
                                    "YouTube video, Twitch VOD, or any streamlink/yt-dlp URL",
                                ),
                        );
                        platform_badge(ui, platform);
                        ui.label(platform.label());
                    });
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut vf.title).hint_text(
                        "optional — used for the filename (default: the title, else \"video\")",
                    ));
                    ui.end_row();

                    // Auto-detect + Tool.
                    ui.label("Auto-detect");
                    ui.checkbox(&mut vf.auto_title, "Detect title + channel")
                        .on_hover_text(
                            "Looks up the real title and channel via yt-dlp at download time: \
                             fills the Channel column and the {title}/{channel} variables (and \
                             {name} when Name is left blank).",
                        );
                    ui.label("Tool").on_hover_text(vf.tool.tooltip());
                    let tool_label = if vf.tool == Tool::YtDlp && !vf.tool_binary.is_empty() {
                        if vf.tool_binary == crate::downloader::TOOL_BINARY_SABR {
                            "yt-dlp-dev (SABR)".to_string()
                        } else {
                            vf.tool_binary.clone()
                        }
                    } else {
                        vf.tool.label().to_string()
                    };
                    egui::ComboBox::from_id_salt("video_tool_cb")
                        .selected_text(tool_label)
                        .show_ui(ui, |ui| {
                            for t in Tool::ALL {
                                let selected = vf.tool == t && vf.tool_binary.is_empty();
                                if ui
                                    .selectable_label(selected, t.label())
                                    .on_hover_text(t.tooltip())
                                    .clicked()
                                {
                                    vf.tool = t;
                                    vf.tool_binary.clear();
                                }
                            }
                            let sabr_selected = vf.tool == Tool::YtDlp
                                && vf.tool_binary == crate::downloader::TOOL_BINARY_SABR;
                            if ui
                                .selectable_label(sabr_selected, "yt-dlp-dev (SABR)")
                                .on_hover_text(
                                    "The yt-dlp dev build with SABR support, configured in \
                                     Settings → Downloads → YouTube SABR. Falls back to the \
                                     system yt-dlp if no SABR build path is set there.",
                                )
                                .clicked()
                            {
                                vf.tool = Tool::YtDlp;
                                vf.tool_binary = crate::downloader::TOOL_BINARY_SABR.to_string();
                            }
                            for alias in &custom_tool_aliases {
                                let sel =
                                    vf.tool == Tool::YtDlp && &vf.tool_binary == alias;
                                if ui
                                    .selectable_label(sel, alias)
                                    .on_hover_text(
                                        "Custom tool, configured in Settings → Downloads → \
                                         Custom download tools.",
                                    )
                                    .clicked()
                                {
                                    vf.tool = Tool::YtDlp;
                                    vf.tool_binary = alias.clone();
                                }
                            }
                        });
                    ui.end_row();

                    // Quality + Auth.
                    ui.label("Quality");
                    ui.add(
                        egui::TextEdit::singleline(&mut vf.quality)
                            .hint_text("best, 1080p, or a yt-dlp -f selector"),
                    );
                    ui.label("Auth");
                    let auth_text = match vf.auth_override {
                        None => "Default (per-platform)".to_string(),
                        Some(k) => k.label().to_string(),
                    };
                    egui::ComboBox::from_id_salt("video_auth_cb")
                        .selected_text(auth_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut vf.auth_override,
                                None,
                                "Default (per-platform)",
                            );
                            for k in AuthKind::ALL {
                                ui.selectable_value(&mut vf.auth_override, Some(k), k.label());
                            }
                        });
                    ui.end_row();

                    // Auth value (only for cookie/token overrides) — its own row.
                    match vf.auth_override {
                        Some(AuthKind::CookiesBrowser) => {
                            ui.label("Browser");
                            ui.text_edit_singleline(&mut vf.auth_value)
                                .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (the folder under …/Firefox/Profiles, or an absolute path)");
                            ui.end_row();
                        }
                        Some(AuthKind::CookiesFile) => {
                            ui.label("Cookies file");
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(&mut vf.auth_value);
                                if ui.button("Browse…").clicked() {
                                    self.pending_browse = Some(spawn_browse_file(
                                        &vf.auth_value,
                                        |app, p| app.video_form.auth_value = p,
                                    ));
                                }
                            });
                            ui.end_row();
                        }
                        Some(AuthKind::Token) => {
                            ui.label("Auth token");
                            ui.add(egui::TextEdit::singleline(&mut vf.auth_value).password(true))
                                .on_hover_text("Twitch OAuth token (streamlink)");
                            ui.end_row();
                        }
                        // Default (None), Global (Inherit), and None (Disabled) need no value.
                        _ => {}
                    }

                    // Output folder + Filename template.
                    ui.label("Output folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut vf.output_dir);
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_folder(
                                &vf.output_dir,
                                |app, p| app.video_form.output_dir = p,
                            ));
                        }
                    });
                    let tmpl_hint = "Variables: {name} {title} {channel} {date} {time} {timestamp} {year} {month} {day} {hour} {minute} {second} {tool} {mode} {platform} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {games} {went_live_date} {went_live_time}";
                    ui.label("Filename template").on_hover_text(tmpl_hint);
                    ui.horizontal(|ui| {
                        let (del, save) = filename_preset_combo(
                            ui,
                            "video_form_tmpl",
                            &mut vf.filename_template,
                            custom_presets,
                        );
                        if del.is_some() { vf_preset_delete = del; }
                        if save { vf_preset_save_tmpl = Some(vf.filename_template.clone()); }
                        ui.text_edit_singleline(&mut vf.filename_template).on_hover_text(tmpl_hint);
                        if ui.button("Design…").on_hover_text("Open the Format Designer").clicked() {
                            open_vf_designer = true;
                        }
                    });
                    ui.end_row();

                    // Extra args + Audio tracks.
                    ui.label("Extra args");
                    ui.text_edit_singleline(&mut vf.extra_args);
                    ui.label("Audio tracks");
                    ui.text_edit_singleline(&mut vf.audio_tracks).on_hover_text(
                        "Audio tracks to download. streamlink: --hls-audio-select. yt-dlp: \
                         builds a format selector picking those languages together as separate \
                         muxed tracks (YouTube VODs can carry a dub/descriptive-audio track \
                         alongside the original) — a plain code like 'en' matches 'en-US'/'en-GB' \
                         too. Empty = the tool's default (one track); 'all' (or '*') = every \
                         track; or a comma-separated list of language codes. Ignored for yt-dlp \
                         when Quality is set to a custom format string (that always wins); \
                         ffmpeg keeps the chosen format's tracks either way.",
                    );
                    ui.end_row();

                    // Subtitle tracks + Log chat.
                    ui.label("Subtitle tracks");
                    ui.text_edit_singleline(&mut vf.subtitle_tracks).on_hover_text(
                        "Subtitle tracks to download and embed into the video file (yt-dlp \
                         --sub-langs; embedded, not left as a sidecar next to it, since every \
                         Video download lands in one flat folder). Empty = none; 'all' (or '*') \
                         = every subtitle; or a comma-separated list of language codes. \
                         yt-dlp-only.",
                    );
                    ui.label("Log chat");
                    ui.checkbox(&mut vf.chat_log, "").on_hover_text(
                        "Download chat alongside the video (yt-dlp's live_chat → a \
                         .live_chat.json sidecar, e.g. a YouTube VOD's chat replay). \
                         Sources without a chat track simply produce none.",
                    );
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can = !vf.url.trim().is_empty();
                if ui
                    .add_enabled(can, egui::Button::new("⬇  Download"))
                    .clicked()
                {
                    do_download = true;
                }
                if ui
                    .add_enabled(can, egui::Button::new("List formats"))
                    .on_hover_text(
                        "Show available formats/qualities for this URL using the selected tool",
                    )
                    .clicked()
                {
                    do_list_formats = true;
                }
                if ui
                    .button("🛟  Recover Twitch VOD…")
                    .on_hover_text(
                        "Recover a deleted or DMCA-muted Twitch VOD from surviving CDN \
                         segments (streamer + broadcast id + start time, or a \
                         TwitchTracker/StreamsCharts/SullyGnome URL).",
                    )
                    .clicked()
                {
                    do_recover_vod = true;
                }
            });
            ui.add_space(6.0);
        }

        if do_download {
            self.start_video_download();
        }
        if do_list_formats {
            self.start_format_probe(ui.ctx().clone());
        }
        if do_recover_vod {
            self.open_recover_vod_manual();
        }
        if open_vf_designer {
            let tmpl = self.video_form.filename_template.clone();
            self.open_format_designer(tmpl, Some(FormatDesignerTarget::VideoForm));
        }
        if let Some(id) = vf_preset_delete {
            if let Err(e) = self.core.store.delete_filename_preset(id) {
                self.status = format!("Error deleting preset: {e:#}");
            } else {
                self.custom_presets = self.core.store.get_filename_presets().unwrap_or_default();
            }
        }
        if let Some(tmpl) = vf_preset_save_tmpl {
            self.save_preset_dialog = Some(SavePresetDraft {
                template: tmpl,
                name: String::new(),
                error: String::new(),
            });
        }
    }

    /// Insert the form's video as a queued download and kick off the supervisor.
    pub(super) fn start_video_download(&mut self) {
        let url = self.video_form.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let platform = Platform::detect(&url);
        // "Default" auth uses the snapshotted platform default; an explicit
        // choice overrides it. (All fields resolve from the same snapshot.)
        let (auth_kind, auth_value) = match self.video_form.auth_override {
            Some(kind) => (kind, self.video_form.auth_value.clone()),
            None => (
                self.video_form.default_auth_kind,
                self.video_form.default_auth_value.clone(),
            ),
        };
        let video = Video {
            id: 0,
            platform,
            url,
            title: self.video_form.title.trim().to_string(),
            channel: String::new(),
            tool: self.video_form.tool,
            tool_binary: self.video_form.tool_binary.clone(),
            quality: self.video_form.quality.clone(),
            output_dir: self.video_form.output_dir.clone(),
            filename_template: self.video_form.filename_template.clone(),
            auth_kind,
            auth_value,
            audio_tracks: self.video_form.audio_tracks.trim().to_string(),
            subtitle_tracks: self.video_form.subtitle_tracks.trim().to_string(),
            chat_log: self.video_form.chat_log,
            extra_args: self.video_form.extra_args.clone(),
            auto_title: self.video_form.auto_title,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            log_excerpt: String::new(),
            created_at: 0,
            started_at: None,
            ended_at: None,
        };
        match self.core.store.insert_video(&video) {
            Ok(id) => {
                self.core.manual(ManualCommand::StartVideo(id));
                self.status = "Queued video download.".into();
                // Clear the URL/name; re-fill defaults for the next download.
                self.video_form.url.clear();
                self.video_form.title.clear();
                self.video_form.last_platform = None;
                self.reload_videos();
            }
            Err(e) => self.status = format!("Error: {e}"),
        }
    }

    /// Probe available formats/qualities for the form's URL with the selected
    /// tool, on the async runtime; the result appears in a window.
    pub(super) fn start_format_probe(&mut self, ctx: egui::Context) {
        let url = self.video_form.url.trim().to_string();
        if url.is_empty() {
            self.status = "Enter a URL first.".into();
            return;
        }
        let tool = self.video_form.tool;
        let (auth_kind, auth_value) = match self.video_form.auth_override {
            Some(kind) => (kind, self.video_form.auth_value.clone()),
            None => (
                self.video_form.default_auth_kind,
                self.video_form.default_auth_value.clone(),
            ),
        };
        let global_method = setting_or_empty(&self.core, K_DOWNLOAD_AUTH);
        let global_browser = setting_or_empty(&self.core, K_COOKIES_BROWSER);
        let auth = crate::downloader::resolve_auth_for(
            auth_kind,
            &auth_value,
            &global_method,
            &global_browser,
        );

        let probe = self.format_probe.clone();
        *probe.lock().unwrap() = FormatProbe::Running;
        self.status = "Listing formats…".into();
        self.core.rt.spawn(async move {
            let result = crate::downloader::probe_formats(tool, &url, &auth).await;
            *probe.lock().unwrap() = match result {
                Ok(s) => FormatProbe::Done(s),
                Err(e) => FormatProbe::Failed(e),
            };
            ctx.request_repaint();
        });
    }

    /// Window showing the result of a "List formats" probe.
    #[allow(deprecated)]
    pub(super) fn format_probe_window(&mut self, ctx: &egui::Context) {
        let probe = self.format_probe.lock().unwrap().clone();
        let (title, body, done) = match &probe {
            FormatProbe::Idle => return,
            FormatProbe::Running => ("Listing formats…", "Running…".to_string(), false),
            FormatProbe::Done(s) => ("Available formats", s.clone(), true),
            FormatProbe::Failed(e) => ("Format probe failed", e.clone(), true),
        };
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("format_probe_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.to_string())
                .with_inner_size([680.0, 460.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if done && ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(body.clone());
                    }
                    ui.add_space(4.0);
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&body).monospace())
                                    .selectable(true),
                            );
                        });
                });
            },
        );
        if !open {
            *self.format_probe.lock().unwrap() = FormatProbe::Idle;
        }
    }

    // ── Twitch VOD recovery ──────────────────────────────────────────────────

    /// Format a UTC epoch as the dialog's `"YYYY-MM-DD HH:MM:SS"` string.
    pub(super) fn fmt_utc(epoch: i64) -> String {
        chrono::DateTime::from_timestamp(epoch, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    }

    /// Parse the dialog's `"YYYY-MM-DD HH:MM:SS"` (UTC) into an epoch.
    pub(super) fn parse_utc(s: &str) -> Option<i64> {
        chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|dt| dt.and_utc().timestamp())
    }

    /// A reqwest client for recovery probes/scrapes: a browser UA (helps the
    /// third-party sites) and a generous timeout. Clones share the connection pool.
    pub(super) fn recover_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/122.0 Safari/537.36",
            )
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    }

    /// Open the Recover-VOD dialog auto-filled from a tracked recording.
    pub(super) fn open_recover_vod_from_seed(&mut self, rec_id: i64) {
        let seed = match self.core.store.recording_recovery_seed(rec_id) {
            Ok(Some(s)) => s,
            _ => {
                self.status = "Could not load recording for recovery.".into();
                return;
            }
        };
        let Some(login) = crate::detectors::twitch_login(&seed.monitor_url) else {
            self.status = "VOD recovery is Twitch-only.".into();
            return;
        };
        if seed.stream_id.is_empty() {
            self.status = "This recording has no stream id — use manual entry.".into();
            return;
        }
        *self.recover_probe.lock().unwrap() = RecoverProbe::Idle;
        *self.recover_scrape.lock().unwrap() = RecoverScrape::Idle;
        self.recover_form = Some(RecoverVodForm {
            login,
            broadcast_id: seed.stream_id,
            start_utc: Self::fmt_utc(seed.start_epoch),
            went_live_approx: seed.went_live_approx,
            url_paste: String::new(),
            quality: setting_or_empty(&self.core, crate::recovery::K_RECOVERY_QUALITY),
            rec_id: Some(rec_id),
            output_dir: String::new(),
            deleted: seed.deleted,
            vod_id: seed.vod_id,
        });
    }

    /// Open a blank Recover-VOD dialog for a VOD the app never tracked (the result
    /// lands in the Videos list).
    pub(super) fn open_recover_vod_manual(&mut self) {
        *self.recover_probe.lock().unwrap() = RecoverProbe::Idle;
        *self.recover_scrape.lock().unwrap() = RecoverScrape::Idle;
        self.recover_form = Some(RecoverVodForm {
            login: String::new(),
            broadcast_id: String::new(),
            start_utc: String::new(),
            went_live_approx: false,
            url_paste: String::new(),
            quality: setting_or_empty(&self.core, crate::recovery::K_RECOVERY_QUALITY),
            rec_id: None,
            output_dir: self.settings.default_output_dir.clone(),
            deleted: true,
            vod_id: None,
        });
    }

    /// Kick off the async CDN probe (dry-run): locate the live playlist, list the
    /// available qualities, and count present/muted/missing segments.
    pub(super) fn start_recover_probe(&mut self, ctx: egui::Context) {
        let Some(f) = self.recover_form.as_ref() else { return };
        let Some(start_epoch) = Self::parse_utc(&f.start_utc) else {
            self.status = "Start time must be YYYY-MM-DD HH:MM:SS (UTC).".into();
            return;
        };
        if f.login.trim().is_empty() || f.broadcast_id.trim().is_empty() {
            self.status = "Enter a streamer login and broadcast id.".into();
            return;
        }
        let inputs = crate::recovery::RecoveryInputs {
            login: f.login.trim().to_lowercase(),
            broadcast_id: f.broadcast_id.trim().to_string(),
            start_epoch,
            went_live_approx: f.went_live_approx,
            vod_id: f.vod_id.clone(),
        };
        let probe_all = f.deleted;
        let probe = self.recover_probe.clone();
        let client = Self::recover_http_client();
        let store = Arc::clone(&self.core.store);
        *probe.lock().unwrap() = RecoverProbe::Running;
        self.status = "Probing Twitch CDN…".into();
        self.core.rt.spawn(async move {
            let hosts = crate::recovery::load_hosts(&store);
            let max_conc = crate::recovery::load_max_conc(&store);
            let state = match crate::recovery::resolve_playlist(&client, &inputs, &hosts, max_conc)
                .await
            {
                None => RecoverProbe::NotFound(
                    "No live playlist found — the VOD may be past the ~60-day CDN window, or \
                     the start time / broadcast id is off."
                        .into(),
                ),
                Some(found) => {
                    let qualities =
                        crate::recovery::enumerate_qualities(&client, &found, max_conc).await;
                    let chosen = qualities.first().cloned().unwrap_or_else(|| "chunked".into());
                    let url = found.url.replacen("chunked", &chosen, 1);
                    match crate::recovery::build_playlist(&client, &url, max_conc, probe_all, None, None).await {
                        Ok(r) => RecoverProbe::Found {
                            host: found.host,
                            matched_epoch: found.matched_epoch,
                            qualities,
                            total: r.total,
                            present: r.present,
                            unmuted: r.unmuted_recovered,
                            missing: r.missing,
                        },
                        Err(e) => RecoverProbe::Failed(format!("playlist read failed: {e}")),
                    }
                }
            };
            *probe.lock().unwrap() = state;
            ctx.request_repaint();
        });
    }

    /// Parse the pasted URL. A Twitch `/videos/<id>` URL takes the robust GQL
    /// fast-path (fills login + broadcast id + start + vod_id, no host guessing);
    /// a TwitchTracker/StreamsCharts/SullyGnome URL fills login + broadcast id
    /// synchronously and scrapes the start time. Result lands in `recover_scrape`.
    pub(super) fn parse_recover_url(&mut self, ctx: egui::Context) {
        let url = self
            .recover_form
            .as_ref()
            .map(|f| f.url_paste.trim().to_string())
            .unwrap_or_default();

        // Twitch VOD URL → GQL (exact folder, works for muted-but-online VODs).
        if let Some(vid) = crate::recovery::scrape::twitch_vod_id(&url) {
            if let Some(f) = self.recover_form.as_mut() {
                f.vod_id = Some(vid.clone());
                f.deleted = false; // a resolvable /videos/ id means it's still online
            }
            let scrape = self.recover_scrape.clone();
            let client = Self::recover_http_client();
            *scrape.lock().unwrap() = RecoverScrape::Running;
            self.status = "Looking up VOD via Twitch…".into();
            self.core.rt.spawn(async move {
                let state = match crate::recovery::gql_vod_info(&client, &vid).await {
                    Ok(info) => RecoverScrape::FilledFull {
                        login: info.login,
                        broadcast_id: info.broadcast_id,
                        start_epoch: info.start_epoch,
                    },
                    Err(e) => RecoverScrape::Failed(format!("VOD lookup failed: {e}")),
                };
                *scrape.lock().unwrap() = state;
                ctx.request_repaint();
            });
            return;
        }

        // Third-party tracker URL → parse ids from the path + scrape the start time.
        let Some(f) = self.recover_form.as_mut() else { return };
        let Some(parsed) = crate::recovery::scrape::parse_vod_url(&url) else {
            self.status =
                "Unrecognized URL — paste a twitch.tv/videos/<id> or a TwitchTracker/StreamsCharts/SullyGnome /streams/<id> link."
                    .into();
            return;
        };
        f.login = parsed.login.clone();
        f.broadcast_id = parsed.broadcast_id.clone();
        f.went_live_approx = false;
        // A tracker link identifies a (possibly different) broadcast — drop any
        // vod_id left from a recording seed or an earlier /videos/ paste, or the
        // GQL fast-path would confidently resolve the WRONG VOD; and go back to
        // the safe full-probe mode.
        f.vod_id = None;
        f.deleted = true;
        let scrape = self.recover_scrape.clone();
        let client = Self::recover_http_client();
        *scrape.lock().unwrap() = RecoverScrape::Running;
        self.status = "Fetching stream start time…".into();
        self.core.rt.spawn(async move {
            let state = match crate::recovery::scrape::scrape_start_time(&client, &parsed).await {
                Ok(epoch) => RecoverScrape::Filled(epoch),
                Err(e) => RecoverScrape::Failed(format!("Could not read start time: {e}")),
            };
            *scrape.lock().unwrap() = state;
            ctx.request_repaint();
        });
    }

    /// The Recover-VOD dialog window.
    #[allow(deprecated)]
    pub(super) fn recover_vod_window(&mut self, ctx: &egui::Context) {
        if self.recover_form.is_none() {
            return;
        }
        // Apply an async "Parse URL" result into the form.
        let scrape = self.recover_scrape.lock().unwrap().clone();
        match scrape {
            RecoverScrape::Filled(epoch) => {
                if let Some(f) = self.recover_form.as_mut() {
                    f.start_utc = Self::fmt_utc(epoch);
                }
                *self.recover_scrape.lock().unwrap() = RecoverScrape::Idle;
            }
            RecoverScrape::FilledFull { login, broadcast_id, start_epoch } => {
                if let Some(f) = self.recover_form.as_mut() {
                    f.login = login;
                    f.broadcast_id = broadcast_id;
                    f.start_utc = Self::fmt_utc(start_epoch);
                    f.went_live_approx = false;
                }
                *self.recover_scrape.lock().unwrap() = RecoverScrape::Idle;
            }
            _ => {}
        }
        let scrape_note = match self.recover_scrape.lock().unwrap().clone() {
            RecoverScrape::Running => Some(("Fetching start time…".to_string(), false)),
            RecoverScrape::Failed(e) => Some((e, true)),
            _ => None,
        };
        let probe = self.recover_probe.lock().unwrap().clone();

        let mut open = true;
        let mut do_probe = false;
        let mut do_parse = false;
        let mut do_recover = false;
        let mut do_cancel = false;

        // Snapshot form fields for editing (written back after the closure).
        let mut f = self.recover_form.take().unwrap();

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("recover_vod_vp"),
            egui::ViewportBuilder::default()
                .with_title("Recover Twitch VOD")
                .with_inner_size([560.0, 480.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Reconstruct a deleted or DMCA-muted Twitch VOD from segments still \
                             on the CDN (~60-day window).",
                        )
                        .weak(),
                    );
                    ui.add_space(6.0);

                    egui::Grid::new("recover_vod_grid")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Paste URL");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.url_paste)
                                        .hint_text("twitch.tv/videos/<id> or twitchtracker.com/<login>/streams/<id>")
                                        .desired_width(280.0),
                                );
                                if ui.button("Parse").clicked() {
                                    do_parse = true;
                                }
                            });
                            ui.end_row();

                            ui.label("Streamer login");
                            ui.text_edit_singleline(&mut f.login);
                            ui.end_row();

                            ui.label("Broadcast id");
                            ui.add(
                                egui::TextEdit::singleline(&mut f.broadcast_id)
                                    .hint_text("the /streams/<id> number, not /videos/<id>"),
                            );
                            ui.end_row();

                            ui.label("Start (UTC)");
                            ui.add(
                                egui::TextEdit::singleline(&mut f.start_utc)
                                    .hint_text("YYYY-MM-DD HH:MM:SS"),
                            );
                            ui.end_row();

                            ui.label("Quality");
                            let quals: Vec<String> = match &probe {
                                RecoverProbe::Found { qualities, .. } => qualities.clone(),
                                _ => Vec::new(),
                            };
                            if quals.is_empty() {
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.quality)
                                        .hint_text("auto / source (probe to list)"),
                                );
                            } else {
                                egui::ComboBox::from_id_salt("recover_quality")
                                    .selected_text(if f.quality.is_empty() {
                                        "auto (source)".to_string()
                                    } else {
                                        f.quality.clone()
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut f.quality, String::new(), "auto (source)");
                                        for q in &quals {
                                            ui.selectable_value(&mut f.quality, q.clone(), q);
                                        }
                                    });
                            }
                            ui.end_row();

                            ui.label("Options");
                            ui.vertical(|ui| {
                                ui.checkbox(
                                    &mut f.deleted,
                                    "Deleted VOD (validate every segment)",
                                )
                                .on_hover_text(
                                    "On: HEAD-probe every segment (needed for deleted VODs). \
                                     Off: only muted segments are probed — much faster for a \
                                     VOD that still exists but is muted.",
                                );
                                ui.checkbox(
                                    &mut f.went_live_approx,
                                    "Start time is approximate (widen search)",
                                );
                            });
                            ui.end_row();
                        });

                    if let Some((note, is_err)) = &scrape_note {
                        ui.add_space(2.0);
                        let color = if *is_err {
                            egui::Color32::from_rgb(220, 140, 30)
                        } else {
                            egui::Color32::GRAY
                        };
                        ui.colored_label(color, note);
                    }

                    ui.add_space(8.0);
                    ui.separator();

                    // Probe result.
                    match &probe {
                        RecoverProbe::Idle => {
                            ui.label(
                                egui::RichText::new("Probe to check availability before recovering.")
                                    .weak(),
                            );
                        }
                        RecoverProbe::Running => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Probing Twitch CDN…");
                            });
                        }
                        RecoverProbe::NotFound(msg) | RecoverProbe::Failed(msg) => {
                            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), msg);
                        }
                        RecoverProbe::Found {
                            host,
                            matched_epoch,
                            total,
                            present,
                            unmuted,
                            missing,
                            ..
                        } => {
                            ui.colored_label(
                                egui::Color32::from_rgb(70, 180, 90),
                                "✔ VOD found on the CDN",
                            );
                            ui.label(format!("Host: {host}"));
                            ui.label(format!("True start (UTC): {}", Self::fmt_utc(*matched_epoch)));
                            ui.label(format!(
                                "Segments: {present}/{total} present · {unmuted} un-muted · {missing} missing",
                            ));
                            if *missing > 0 {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 160, 30),
                                    "⚠ Partial recovery — gaps will appear in the timeline.",
                                );
                            }
                        }
                    }

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("🔎  Probe").clicked() {
                            do_probe = true;
                        }
                        let found = matches!(probe, RecoverProbe::Found { .. });
                        if ui
                            .add_enabled(found, egui::Button::new("🛟  Recover"))
                            .on_hover_text("Download & mux the surviving segments into an MKV")
                            .clicked()
                        {
                            do_recover = true;
                        }
                        if ui.button("Cancel").clicked() {
                            do_cancel = true;
                        }
                    });
                });
            },
        );

        // Write the (possibly edited) form back.
        self.recover_form = Some(f);

        if do_parse {
            self.parse_recover_url(ctx.clone());
        }
        if do_probe {
            self.start_recover_probe(ctx.clone());
        }
        if do_recover {
            self.submit_recover_vod();
            open = false;
        }
        if do_cancel {
            open = false;
        }
        if !open {
            self.recover_form = None;
            *self.recover_probe.lock().unwrap() = RecoverProbe::Idle;
            *self.recover_scrape.lock().unwrap() = RecoverScrape::Idle;
        }
    }

    /// Fire the recovery download for the current form and close the dialog.
    pub(super) fn submit_recover_vod(&mut self) {
        let Some(f) = self.recover_form.as_ref() else { return };
        let Some(start_epoch) = Self::parse_utc(&f.start_utc) else {
            self.status = "Start time must be YYYY-MM-DD HH:MM:SS (UTC).".into();
            return;
        };
        let inputs = crate::recovery::RecoveryInputs {
            login: f.login.trim().to_lowercase(),
            broadcast_id: f.broadcast_id.trim().to_string(),
            start_epoch,
            went_live_approx: f.went_live_approx,
            vod_id: f.vod_id.clone(),
        };
        let sink = match f.rec_id {
            Some(id) => crate::recovery::RecoverySink::Recording(id),
            None => crate::recovery::RecoverySink::Standalone {
                output_dir: f.output_dir.clone(),
                filename: format!("Recovered_{}_{}", inputs.login, inputs.broadcast_id),
            },
        };
        self.core.manual(ManualCommand::RecoverVod {
            inputs,
            quality: f.quality.trim().to_string(),
            sink,
            probe_all: f.deleted,
        });
        self.status = "VOD recovery started — see the Background tab.".into();
    }

    // ── Format Designer ──────────────────────────────────────────────────────

    /// Open (or replace) the Format Designer window, pre-loading recordings for
    /// the first monitor in the list.
    pub(super) fn open_format_designer(&mut self, template: String, target: Option<FormatDesignerTarget>) {
        let mut state = FormatDesignerState::new(template, target);
        // Load recordings for the first monitor off the UI thread.
        if let Some(m) = self.rows.first() {
            let store = Arc::clone(&self.core.store);
            let monitor_id = m.monitor.id;
            let (tx, rx) = std::sync::mpsc::channel();
            debug!(monitor_id, "spawning fd-recordings-load thread");
            std::thread::Builder::new()
                .name("fd-recordings-load".into())
                .spawn(move || {
                    let t = std::time::Instant::now();
                    let recs = store.recordings_for_monitor(monitor_id).unwrap_or_default();
                    let default_idx = recs.len().saturating_sub(1);
                    debug!(elapsed_ms = t.elapsed().as_millis(), monitor_id, count = recs.len(), "fd-recordings-load done");
                    let _ = tx.send((recs, default_idx));
                })
                .ok();
            state.recordings_load = Some(rx);
        }
        self.format_designer = Some(state);
    }

    /// The floating Format Designer window: token reference, live preview, and
    /// optional write-back to the field that opened it.
    #[allow(deprecated)]
    pub(super) fn format_designer_window(&mut self, ctx: &egui::Context) {
        if self.format_designer.is_none() {
            return;
        }

        // Token catalogue: (category label, &[(token, tooltip)])
        const TOKENS: &[(&str, &[(&str, &str)])] = &[
            ("Identity", &[
                ("{name}", "Channel / stream name"),
                ("{channel}", "Channel name (VOD downloads)"),
                ("{video_id}", "Stream or video ID"),
                ("{take}", "Recording attempt number"),
            ]),
            ("Capture time", &[
                ("{date}", "Date YYYYMMDD"),
                ("{time}", "Time HHMMSS"),
                ("{year}", "4-digit year"),
                ("{month}", "2-digit month"),
                ("{day}", "2-digit day"),
                ("{hour}", "2-digit hour (UTC)"),
                ("{minute}", "2-digit minute"),
                ("{second}", "2-digit second"),
                ("{timestamp}", "Unix timestamp"),
            ]),
            ("Live timing", &[
                ("{went_live_date}", "Broadcast go-live date YYYYMMDD"),
                ("{went_live_time}", "Broadcast go-live time HHMMSS"),
            ]),
            ("Stream info", &[
                ("{title}", "Stream title"),
                ("{games}", "Games / categories played"),
                ("{quality}", "Configured quality selector"),
                ("{platform}", "twitch · youtube · kick · generic"),
                ("{mode}", "live · sabr · dash · hybrid · direct · vod"),
                ("{tool}", "streamlink · yt-dlp · ffmpeg"),
            ]),
            ("Media (post-probe)", &[
                ("{resolution}", "e.g. 1920x1080"),
                ("{height}", "e.g. 1080"),
                ("{width}", "e.g. 1920"),
                ("{fps}", "e.g. 60"),
                ("{vcodec}", "Video codec e.g. h264 · hevc · av1"),
                ("{acodec}", "Audio codec e.g. aac · opus"),
            ]),
        ];

        // ── Drain any completed background recordings load ────────────────────
        // When a load is in-flight, schedule a repaint so we check again next
        // frame even if there is no user input — otherwise the data sits in the
        // channel until the user moves the mouse.
        let still_loading = if let Some(fd) = self.format_designer.as_mut() {
            if let Some(rx) = &fd.recordings_load {
                match rx.try_recv() {
                    Ok((recs, default_idx)) => {
                        fd.recordings = recs;
                        fd.selected_recording_idx = default_idx;
                        fd.recordings_load = None;
                        false
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        fd.recordings_load = None;
                        false
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => true,
                }
            } else {
                false
            }
        } else {
            false
        };
        if still_loading {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }

        // ── Snapshot state before closure (avoids borrow conflicts) ──────────
        let template = self.format_designer.as_ref().unwrap().template.clone();
        let selected_monitor_idx = self.format_designer.as_ref().unwrap().selected_monitor_idx;
        let selected_recording_idx = self.format_designer.as_ref().unwrap().selected_recording_idx;
        let recordings = self.format_designer.as_ref().unwrap().recordings.clone();
        let target = self.format_designer.as_ref().unwrap().target.clone();

        let monitor_names: Vec<String> = self.rows.iter()
            .map(|r| r.channel.name.clone())
            .collect();
        let selected_monitor = self.rows.get(selected_monitor_idx).cloned();
        let selected_recording = recordings.get(selected_recording_idx).cloned();

        // Pre-compute preview (stale by one frame on fast typing — acceptable).
        let preview = selected_monitor.as_ref()
            .map(|m| build_preview_filename(m, selected_recording.as_ref(), &template))
            .unwrap_or_default();

        // ── Mutable locals for the closure to write into ─────────────────────
        let mut new_template = template.clone();
        let mut new_monitor_idx = selected_monitor_idx;
        let mut new_recording_idx = selected_recording_idx;
        let mut close = false;
        let mut apply = false;
        let mut fd_save_preset = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("format_designer_vp"),
            egui::ViewportBuilder::default()
                .with_title("Format Designer")
                .with_inner_size([820.0, 600.0])
                .with_resizable(true),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(2.0);
                    ui.label("Listing of all possible {formatter} options — highlighted when in use in the template below.");
                    ui.add_space(6.0);

                    // ── Channel + Recording dropdowns ────────────────────────
                    ui.horizontal(|ui| {
                        ui.label("Channel:");
                        let ch_label = monitor_names.get(new_monitor_idx)
                            .cloned()
                            .unwrap_or_else(|| "— none —".to_string());
                        egui::ComboBox::from_id_salt("fd_channel_cb")
                            .selected_text(&ch_label)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                for (i, name) in monitor_names.iter().enumerate() {
                                    if ui.selectable_value(&mut new_monitor_idx, i, name).clicked() {
                                        new_recording_idx = usize::MAX; // sentinel → reload
                                    }
                                }
                            });

                        ui.add_space(12.0);
                        ui.label("Recording:");
                        let rec_label = recordings.get(new_recording_idx)
                            .map(|r| {
                                chrono::DateTime::from_timestamp(r.started_at, 0)
                                    .map(|dt| dt.with_timezone(&chrono::Local)
                                        .format("%Y-%m-%d %H:%M").to_string())
                                    .unwrap_or_else(|| r.started_at.to_string())
                            })
                            .unwrap_or_else(|| "— sample data —".to_string());
                        egui::ComboBox::from_id_salt("fd_recording_cb")
                            .selected_text(&rec_label)
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                // "— sample data —" option (no real recording)
                                let no_rec_label = "— sample data —";
                                if ui.selectable_label(new_recording_idx == usize::MAX, no_rec_label).clicked() {
                                    new_recording_idx = usize::MAX;
                                }
                                // Recordings newest-first
                                for (i, r) in recordings.iter().enumerate().rev() {
                                    let label = chrono::DateTime::from_timestamp(r.started_at, 0)
                                        .map(|dt| dt.with_timezone(&chrono::Local)
                                            .format("%Y-%m-%d %H:%M").to_string())
                                        .unwrap_or_else(|| r.started_at.to_string());
                                    let label = format!("{label}  ({})", r.status);
                                    if ui.selectable_value(&mut new_recording_idx, i, &label).clicked() {}
                                }
                            });
                    });

                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // ── Token grid ───────────────────────────────────────────
                    let accent = ui.style().visuals.selection.bg_fill;
                    let dim = egui::Color32::from_gray(45);
                    let text_col = ui.style().visuals.text_color();

                    for (category, tokens) in TOKENS {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong(*category);
                            ui.label("  ");
                            for (tok, desc) in *tokens {
                                let in_use = new_template.contains(tok);
                                let fill = if in_use { accent } else { dim };
                                let label_text = egui::RichText::new(*tok)
                                    .monospace()
                                    .small()
                                    .color(text_col);
                                let btn = egui::Button::new(label_text)
                                    .fill(fill)
                                    .corner_radius(3.0);
                                if ui.add(btn).on_hover_text(*desc).clicked() {
                                    new_template.push_str(tok);
                                }
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // ── Template input ───────────────────────────────────────
                    ui.horizontal(|ui| {
                        ui.label("Template:");
                        ui.add_sized(
                            [ui.available_width(), 20.0],
                            egui::TextEdit::singleline(&mut new_template)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("{name}_{date}_{time}"),
                        );
                    });

                    ui.add_space(6.0);

                    // ── Preview ──────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        ui.label("Preview:");
                        let ext = ".mkv";
                        let preview_str = if preview.is_empty() {
                            format!("(no channel selected){ext}")
                        } else {
                            format!("{preview}{ext}")
                        };
                        egui::Frame::new()
                            .fill(egui::Color32::from_gray(28))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width() - 4.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&preview_str)
                                            .monospace()
                                    ).selectable(true),
                                );
                            });
                    });

                    ui.add_space(10.0);

                    // ── Action buttons ───────────────────────────────────────
                    ui.horizontal(|ui| {
                        if target.is_some() && ui.button("Apply").on_hover_text("Write this template back to the field that opened the designer").clicked() {
                            apply = true;
                        }
                        if ui.button("💾 Save as preset…").on_hover_text("Save this template as a named preset available in all filename dropdowns").clicked() {
                            fd_save_preset = true;
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            },
        );

        // ── Apply closure results back to state ──────────────────────────────
        let monitor_changed = new_monitor_idx != selected_monitor_idx;

        if let Some(fd) = self.format_designer.as_mut() {
            fd.template = new_template.clone();
            fd.selected_monitor_idx = new_monitor_idx;
            if monitor_changed {
                // Load recordings for the newly selected monitor off the UI thread.
                if let Some(m) = self.rows.get(new_monitor_idx) {
                    let store = Arc::clone(&self.core.store);
                    let monitor_id = m.monitor.id;
                    let (tx, rx) = std::sync::mpsc::channel();
                    debug!(monitor_id, "spawning fd-recordings-load thread (monitor change)");
                    std::thread::Builder::new()
                        .name("fd-recordings-load".into())
                        .spawn(move || {
                            let t = std::time::Instant::now();
                            let recs = store.recordings_for_monitor(monitor_id).unwrap_or_default();
                            let default_idx = recs.len().saturating_sub(1);
                            debug!(elapsed_ms = t.elapsed().as_millis(), monitor_id, count = recs.len(), "fd-recordings-load done");
                            let _ = tx.send((recs, default_idx));
                        })
                        .ok();
                    fd.recordings_load = Some(rx);
                }
                fd.selected_recording_idx = 0;
            } else {
                fd.selected_recording_idx = new_recording_idx;
            }
        }

        if fd_save_preset {
            self.save_preset_dialog = Some(SavePresetDraft {
                template: new_template.clone(),
                name: String::new(),
                error: String::new(),
            });
        }

        if apply {
            match &target {
                Some(FormatDesignerTarget::MonitorForm) => {
                    if let Some(form) = self.form.as_mut() {
                        form.filename_template = new_template.clone();
                    }
                }
                Some(FormatDesignerTarget::VideoForm) => {
                    self.video_form.filename_template = new_template.clone();
                }
                None => {}
            }
            self.format_designer = None;
        } else if close {
            self.format_designer = None;
        }
    }
}
