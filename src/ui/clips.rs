//! Clips view: the clip catalogue, scoped to everything / one channel / one
//! broadcast.
//!
//! The catalogue is deliberately larger than what is on disk. A clip outlives
//! the VOD it was cut from and can be deleted upstream at any time, so knowing
//! that it *existed* — and holding the keys that could rebuild it — is worth
//! keeping even for clips whose media was never downloaded. That is why the
//! 🔑 column exists and why a row with no local file is still a useful row.

use super::*;
use crate::models::Clip;

/// How many rows the view will build cells for at once. A busy channel has tens
/// of thousands of clips and every row here costs a `Vec<Cell>`; the view says
/// "showing N of M" rather than pretending the cap isn't there.
const DISPLAY_CAP: usize = 5_000;

/// What the Clips view is currently listing.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(super) enum ClipScope {
    #[default]
    All,
    Channel(i64),
    /// Every clip cut from one broadcast — what the Streams tree's 🎞 row opens.
    Vod {
        platform: crate::models::Platform,
        vod_id: String,
        label: String,
    },
}

/// Self-mutating actions picked from a row's context menu, applied after the
/// row borrow ends (the deferred-action pattern `RowActions` uses).
#[derive(Clone, Debug)]
enum ClipMenuChoice {
    Download(i64),
    Recover(i64),
    Forget(i64),
    DeleteFile(i64),
}

impl StreamArchiverApp {
    pub(super) fn ensure_clips_loaded(&mut self) {
        if self.clips_loaded {
            return;
        }
        self.reload_clips();
    }

    pub(super) fn reload_clips(&mut self) {
        let store = &self.core.store;
        let rows = match &self.clips_scope {
            ClipScope::All => store.recent_clips(DISPLAY_CAP as i64).unwrap_or_default(),
            ClipScope::Channel(cid) => {
                store.clips_for_channel(*cid, DISPLAY_CAP as i64).unwrap_or_default()
            }
            ClipScope::Vod { platform, vod_id, .. } => {
                store.clips_for_vod(*platform, vod_id).unwrap_or_default()
            }
        };
        self.clips_total = store.clip_count().unwrap_or(rows.len() as i64) as usize;
        self.clips_rows = rows;
        self.clips_loaded = true;
    }

    pub(super) fn clips_view(&mut self, ui: &mut egui::Ui) {
        self.ensure_clips_loaded();

        // ── toolbar ──
        ui.horizontal_wrapped(|ui| {
            let scope_label = match &self.clips_scope {
                ClipScope::All => "All channels".to_string(),
                ClipScope::Channel(cid) => self
                    .channels
                    .iter()
                    .find(|c| c.id == *cid)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "Channel".into()),
                ClipScope::Vod { label, .. } => label.clone(),
            };
            ui.label(egui::RichText::new("🎞").size(16.0))
                .on_hover_text("Clip catalogue.");
            ui.label(egui::RichText::new(scope_label).strong());
            if !matches!(self.clips_scope, ClipScope::All)
                && ui
                    .button("✕ Clear scope")
                    .on_hover_text("Show clips from every channel again.")
                    .clicked()
            {
                self.clips_scope = ClipScope::All;
                self.clips_loaded = false;
            }
            ui.separator();
            if ui
                .button("⟳ Reload")
                .on_hover_text("Re-read the catalogue from the database.")
                .clicked()
            {
                self.clips_loaded = false;
            }
            ui.separator();
            let archived = self.clips_rows.iter().filter(|c| c.is_archived()).count();
            let gone = self.clips_rows.iter().filter(|c| c.is_gone()).count();
            let keyed = self.clips_rows.iter().filter(|c| c.has_recovery_keys()).count();
            ui.label(format!("{} shown", self.clips_rows.len()))
                .on_hover_text(format!(
                    "{} clips in the catalogue in total.",
                    self.clips_total
                ));
            ui.label(format!("· {archived} archived"))
                .on_hover_text("Clips whose media is downloaded to disk.");
            if gone > 0 {
                ui.label(
                    egui::RichText::new(format!("· {gone} gone")).color(HL_ERROR_TEXT),
                )
                .on_hover_text(
                    "Clips that have vanished upstream since we indexed them. \
                     Those with recovery keys can still be rebuilt.",
                );
            }
            ui.label(format!("· {keyed} 🔑"))
                .on_hover_text(
                    "Clips still carrying their recovery keys (parent VOD id + offset).\n\n\
                     Twitch drops these when the parent VOD expires — measured at 100% of \
                     clips under two weeks old, and 5% at a year. A clip indexed while its \
                     VOD was alive keeps them permanently; one indexed later never gets them.",
                );
        });
        if self.clips_rows.len() >= DISPLAY_CAP {
            ui.label(
                egui::RichText::new(format!(
                    "Showing the first {DISPLAY_CAP} of {} — narrow the scope to see the rest.",
                    self.clips_total
                ))
                .weak(),
            );
        }
        ui.separator();

        if self.clips_rows.is_empty() {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new("No clips catalogued yet.")
                    .strong(),
            );
            ui.label(
                egui::RichText::new(
                    "Clips are indexed automatically: once a couple of hours after a \
                     broadcast ends, and again a day later — that window is when Twitch \
                     still reports which VOD a clip came from, which is what makes a \
                     deleted clip recoverable later. A daily sweep picks up clips made \
                     from older streams too.",
                )
                .weak(),
            );
            return;
        }

        // ── table ──
        let mut sort = self.clips_sort.clone();
        let mut filters = std::mem::take(&mut self.clips_filters);
        filters.resize(CLIP_COLS, String::new());
        let show_actions = self.show_actions;

        let model: Vec<Vec<Cell>> = self
            .clips_rows
            .iter()
            .map(|c| clip_cells(c, &self.channels))
            .collect();

        let mut entries = self.clips_grid.entries.clone();
        let col_order = grid_columns::effective_order(&CLIP_COLUMNS, &entries, |id| {
            id != "actions" || show_actions
        });
        let order_changed = self.clips_grid.note_order(&col_order);

        let mut pick: Option<ClipMenuChoice> = None;
        let mut scope_to: Option<ClipScope> = None;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.style_mut().interaction.selectable_labels = false;
                let mut tb = TableBuilder::new(ui)
                    .id_salt("clips_table")
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                if order_changed {
                    tb.reset();
                }
                for &i in &col_order {
                    let c = &CLIP_COLUMNS[i];
                    let seed = self.clips_grid.widths.get(c.id);
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
                let table = tb.header(46.0, |mut header| {
                    for &i in &col_order {
                        let c = &CLIP_COLUMNS[i];
                        let (rect, _) = header.col(|ui| {
                            if grid_header_cell(
                                ui,
                                GridTableId::Clips,
                                i,
                                c,
                                true,
                                &mut sort,
                                &mut filters[i],
                                &mut entries,
                                &CLIP_COLUMNS,
                                |id| id == "actions",
                            ) {
                                want_reorder = true;
                            }
                        });
                        self.clips_grid.widths.note(c.id, rect.width());
                    }
                });
                if want_reorder {
                    self.reorder_columns = Some(Arc::new(Mutex::new(ReorderColumnsState {
                        table: GridTableId::Clips,
                        draft: entries.clone(),
                        apply: false,
                        cancel: false,
                    })));
                }
                table.body(|body| {
                    let order = ordered_rows(&model, &sort, &filters);
                    body.rows(24.0, order.len(), |mut tr| {
                        let ri = order[tr.index()];
                        let c = &self.clips_rows[ri];
                        let cells = &model[ri];

                        let add_menu = |ui: &mut egui::Ui,
                                        pick: &mut Option<ClipMenuChoice>,
                                        scope_to: &mut Option<ClipScope>| {
                            ui.set_min_width(200.0);
                            if !c.url.is_empty() {
                                if ui.button("🔗  Open clip page").clicked() {
                                    crate::platform::open_url(&c.url);
                                    ui.close();
                                }
                                if ui.button("📋  Copy clip URL").clicked() {
                                    ui.ctx().copy_text(c.url.clone());
                                    ui.close();
                                }
                            }
                            ui.separator();
                            let has_file = !c.output_path.is_empty();
                            if ui
                                .add_enabled(has_file, egui::Button::new("▶  Open file"))
                                .on_disabled_hover_text(
                                    "This clip's media hasn't been downloaded.",
                                )
                                .clicked()
                            {
                                crate::platform::open_path(std::path::Path::new(&c.output_path));
                                ui.close();
                            }
                            if ui
                                .add_enabled(!c.is_active(), egui::Button::new("⬇  Download now"))
                                .on_disabled_hover_text("Already queued or downloading.")
                                .clicked()
                            {
                                *pick = Some(ClipMenuChoice::Download(c.id));
                                ui.close();
                            }
                            // Recovery is only offered where it could actually
                            // work — an action that always fails is worse than
                            // no action at all.
                            let recoverable = c.has_recovery_keys() || c.recording_id.is_some();
                            if ui
                                .add_enabled(
                                    recoverable,
                                    egui::Button::new("🛟  Rebuild from the broadcast"),
                                )
                                .on_hover_text(
                                    "Cut this clip back out of the parent VOD, or out of our \
                                     own recording of it.",
                                )
                                .on_disabled_hover_text(
                                    "No recovery keys: Twitch stopped reporting which VOD this \
                                     clip came from once that VOD expired, and we have no local \
                                     recording of it either. Only the clip's own copy could be \
                                     fetched, and it's gone.",
                                )
                                .clicked()
                            {
                                *pick = Some(ClipMenuChoice::Recover(c.id));
                                ui.close();
                            }
                            ui.separator();
                            if !c.vod_id.is_empty()
                                && ui
                                    .button("🎞  Show this broadcast's clips")
                                    .clicked()
                            {
                                *scope_to = Some(ClipScope::Vod {
                                    platform: c.platform,
                                    vod_id: c.vod_id.clone(),
                                    label: format!("Broadcast {}", c.vod_id),
                                });
                                ui.close();
                            }
                            if let Some(cid) = c.channel_id
                                && ui.button("📺  Show this channel's clips").clicked()
                            {
                                *scope_to = Some(ClipScope::Channel(cid));
                                ui.close();
                            }
                            ui.separator();
                            if ui
                                .add_enabled(has_file, egui::Button::new("🗑  Delete file"))
                                .on_hover_text(
                                    "Dispose of the downloaded media using your configured \
                                     method (Trash / Recycle Bin / permanent). The catalogue \
                                     row and its recovery keys are kept, so the clip can be \
                                     fetched again while it still exists upstream.",
                                )
                                .on_disabled_hover_text("Nothing downloaded for this clip.")
                                .clicked()
                            {
                                *pick = Some(ClipMenuChoice::DeleteFile(c.id));
                                ui.close();
                            }
                            if ui
                                .button("✖  Forget clip")
                                .on_hover_text(
                                    "Remove the catalogue row. Does not delete any file — and \
                                     discards the recovery keys with it, so a clip that later \
                                     vanishes could no longer be rebuilt.",
                                )
                                .clicked()
                            {
                                *pick = Some(ClipMenuChoice::Forget(c.id));
                                ui.close();
                            }
                        };

                        for &ci in &col_order {
                            tr.col(|ui| {
                                let id = CLIP_COLUMNS[ci].id;
                                if id == "actions" {
                                    if ui.small_button("⋯").clicked() {
                                        // The button's own menu; nothing to do
                                        // on a plain click.
                                    }
                                    return;
                                }
                                let text = cells.get(ci).map(|c| c.text.clone()).unwrap_or_default();
                                match id {
                                    "keys" => {
                                        // The one cell worth colouring: it is the
                                        // difference between "recoverable" and
                                        // "gone for good if it disappears".
                                        if c.has_recovery_keys() {
                                            ui.label("🔑").on_hover_text(
                                                "Parent VOD and offset are known — if this clip \
                                                 is deleted it can still be rebuilt.",
                                            );
                                        } else {
                                            ui.label(egui::RichText::new("—").weak())
                                                .on_hover_text(
                                                    "The parent VOD expired before we indexed \
                                                     this clip, so Twitch no longer reports \
                                                     where it came from.",
                                                );
                                        }
                                    }
                                    "state" if c.is_gone() => {
                                        ui.label(
                                            egui::RichText::new(text).color(HL_ERROR_TEXT),
                                        )
                                        .on_hover_text(
                                            "This clip no longer exists upstream.",
                                        );
                                    }
                                    _ => {
                                        let resp = ui.label(text.clone());
                                        if !text.is_empty() {
                                            resp.on_hover_text(text);
                                        }
                                    }
                                }
                            });
                        }
                        tr.response()
                            .context_menu(|ui| add_menu(ui, &mut pick, &mut scope_to));
                    });
                });
            });

        if sort != self.clips_sort {
            let keys: Vec<(usize, bool)> =
                sort.keys.iter().map(|l| (l.col, l.ascending)).collect();
            let persisted = grid_columns::unresolve_sort(&CLIP_COLUMNS, &keys);
            grid_columns::save_sort(&self.core.store, GridTableId::Clips, &persisted);
        }
        self.clips_sort = sort;
        self.clips_filters = filters;
        if entries != self.clips_grid.entries {
            self.clips_grid.entries = entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Clips, &self.clips_grid.entries);
        }

        if let Some(s) = scope_to {
            self.clips_scope = s;
            self.clips_loaded = false;
        }
        match pick {
            Some(ClipMenuChoice::Download(id)) => {
                let _ = self.core.store.set_clip_download(id, "queued", None);
                self.clips_loaded = false;
            }
            Some(ClipMenuChoice::Recover(id)) => {
                // Share the DetectContext's client rather than building one:
                // it carries the connection pool and the token cache.
                if let Some(dctx) = self.core.detect_ctx() {
                    let store = self.core.store.clone();
                    let ctx = ui.ctx().clone();
                    self.core.rt.spawn(async move {
                        let client = dctx.http_client();
                        crate::clips::recover_clip(&store, &client, id, 4).await;
                        ctx.request_repaint();
                    });
                }
                self.clips_loaded = false;
            }
            Some(ClipMenuChoice::Forget(id)) => {
                let _ = self.core.store.forget_clip(id);
                self.clips_loaded = false;
            }
            Some(ClipMenuChoice::DeleteFile(id)) => {
                let store = self.core.store.clone();
                let ctx = ui.ctx().clone();
                self.core.rt.spawn(async move {
                    crate::clips::dispose_clip_media(&store, id).await;
                    ctx.request_repaint();
                });
                self.clips_loaded = false;
            }
            None => {}
        }
    }
}

/// One row's cells, positionally 1:1 with [`CLIP_COLUMNS`] up to [`CLIP_COLS`]
/// (Actions has no cell). A missing entry silently shifts every later column's
/// sort and filter onto the wrong data, so the count is asserted in tests.
pub(super) fn clip_cells(c: &Clip, channels: &[crate::models::Channel]) -> Vec<Cell> {
    let channel = c
        .channel_id
        .and_then(|id| channels.iter().find(|ch| ch.id == id))
        .map(|ch| ch.name.clone())
        .unwrap_or_else(|| c.broadcaster_login.clone());
    let secs = (c.duration_ms as f64 / 1000.0).round() as i64;
    vec![
        Cell::text(c.platform.label()),
        Cell::text(c.title.clone()),
        Cell::text(channel),
        Cell::text(c.creator_login.clone()),
        Cell::num(c.created_at as f64, fmt_date(c.created_at)),
        Cell::num(secs as f64, fmt_duration_secs(secs)),
        Cell::num(c.view_count as f64, fmt_viewers(c.view_count)),
        // Sorts recoverable rows together, which is the useful grouping.
        Cell::num(
            if c.has_recovery_keys() { 1.0 } else { 0.0 },
            if c.has_recovery_keys() { "🔑" } else { "—" },
        ),
        match c.vod_offset_secs {
            Some(o) => Cell::num(o as f64, fmt_duration(o)),
            None => Cell::num(f64::INFINITY, String::new()),
        },
        Cell::text(if c.recording_id.is_some() {
            "archived".to_string()
        } else {
            String::new()
        }),
        Cell::text(if c.is_gone() {
            "gone".to_string()
        } else {
            c.state.clone()
        }),
        Cell::num(
            c.bytes as f64,
            if c.bytes > 0 { fmt_bytes(c.bytes) } else { String::new() },
        ),
        Cell::text(c.output_path.clone()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Platform;

    fn clip() -> Clip {
        Clip {
            id: 1,
            platform: Platform::Twitch,
            slug: "Abc-123".into(),
            broadcaster_login: "laynalazar".into(),
            creator_login: "Someone".into(),
            title: "A Clip".into(),
            duration_ms: 15_600,
            created_at: 1_786_000_000,
            view_count: 4211,
            vod_id: "2840712897".into(),
            vod_offset_secs: Some(4780),
            ..Default::default()
        }
    }

    #[test]
    fn clip_cells_stay_positionally_one_to_one_with_the_columns() {
        // A short row silently shifts every later column's sort/filter onto the
        // wrong data — the exact bug this assertion exists to catch.
        assert_eq!(clip_cells(&clip(), &[]).len(), CLIP_COLS);
    }

    #[test]
    fn a_clip_of_an_unmonitored_channel_still_names_its_broadcaster() {
        // Chat-harvested clips have no channel_id; falling back to the login
        // keeps the row identifiable instead of blank.
        let cells = clip_cells(&clip(), &[]);
        assert_eq!(cells[2].text, "laynalazar");
    }

    #[test]
    fn the_keys_cell_sorts_recoverable_clips_together() {
        let with = clip_cells(&clip(), &[]);
        let mut c = clip();
        c.vod_id = String::new();
        c.vod_offset_secs = None;
        let without = clip_cells(&c, &[]);
        assert_eq!(with[7].text, "🔑");
        assert_eq!(without[7].text, "—");
        match (&with[7].key, &without[7].key) {
            (SortKey::Num(a), SortKey::Num(b)) => assert!(a > b, "keyed rows sort first"),
            _ => panic!("the keys cell must sort numerically, not as text"),
        }
    }

    #[test]
    fn a_clip_without_an_offset_sorts_last_rather_than_at_zero() {
        // Zero would put unknown-offset clips at the very start of the VOD,
        // which reads as a real position rather than a missing one.
        let mut c = clip();
        c.vod_offset_secs = None;
        let cells = clip_cells(&c, &[]);
        assert_eq!(cells[8].text, "");
        match cells[8].key {
            SortKey::Num(n) => assert!(n.is_infinite()),
            _ => panic!("offset must sort numerically"),
        }
    }

    #[test]
    fn a_vanished_clip_reads_gone_whatever_its_download_state_was() {
        let mut c = clip();
        c.state = "indexed".into();
        c.gone_at = 500;
        assert_eq!(clip_cells(&c, &[])[10].text, "gone");
    }
}
