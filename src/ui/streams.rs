//! Streams view: channel grid, add-channel form, imports, OAuth connects.

use super::*;

/// Backing state for the create/rename channel-container dialog.
pub(super) struct ChannelForm {
    /// `Some(id)` = renaming an existing channel; `None` = creating a new one.
    pub(super) id: Option<i64>,
    pub(super) name: String,
    /// Hex color string (e.g. `"#ff9800"` or `"ff9800"`). Empty = auto palette.
    pub(super) color: String,
    /// Post-stream VOD-download overrides for this channel (`None` = inherit global).
    pub(super) vod_download: Option<bool>,
    pub(super) vod_replace: Option<bool>,
    /// Head-backfill-on-new-take overrides for this channel (`None` = inherit global).
    pub(super) head_backfill_fetch: Option<bool>,
    pub(super) head_backfill_replace: Option<bool>,
    /// Automatic-deletion overrides for this channel (`None` = inherit global):
    /// post-join parts cleanup, and how automatic media deletes are executed.
    pub(super) join_cleanup: Option<crate::disposal::JoinCleanup>,
    pub(super) disposal_method: Option<crate::disposal::DisposalMethod>,
    /// Preferred platform when this channel has multiple instances
    /// simultaneously live (`None` = inherit the global default).
    pub(super) primary_platform_pref: Option<Platform>,
}
/// Background load state of an import fetch (followed/subscriptions).
pub(super) enum ImportLoadState {
    Loading,
    Loaded {
        cands: Vec<ImportCandidate>,
        /// Existing YouTube monitor URL → lowercased `UC…` identity, resolved in
        /// the same background task (cached across opens), so `@handle`-added
        /// monitors dedup exactly against a subscription's channel id instead of
        /// only by name.
        resolved: Vec<(String, String)>,
    },
    Error(String),
}

/// One row in the import confirmation dialog: a candidate plus its per-row choices.
pub(super) struct ImportRow {
    pub(super) cand: ImportCandidate,
    /// Whether to import this entry.
    pub(super) selected: bool,
    /// "Auto" — sets `monitor.enabled` (scheduler auto-records). Default off.
    pub(super) auto: bool,
    /// "Disabled" — imports the channel with the master automation switch off
    /// (`automation_enabled = false` on both container and instance): fully
    /// dormant — no polling, detection, or fetches — until re-enabled.
    pub(super) disabled: bool,
    /// Already present in the app (matched an existing monitor by id/login) — shown
    /// greyed and not selectable, so an import can't create duplicates.
    pub(super) already: bool,
    /// A channel with the same name already exists, but identities couldn't be
    /// matched (e.g. an existing YouTube monitor added by @handle vs. a candidate's
    /// channel id). Flagged + left unselected, but still selectable to override.
    pub(super) maybe_dup: bool,
}

/// The "Import followed/subscriptions" confirmation dialog.
pub(super) struct ImportDialog {
    pub(super) title: String,
    /// Background fetch result; moved into `rows` once loaded.
    pub(super) load: Arc<Mutex<ImportLoadState>>,
    /// Editable rows (populated from `load` on the first frame after it completes).
    pub(super) rows: Vec<ImportRow>,
    pub(super) loaded: bool,
    pub(super) search: String,
    pub(super) status: String,
    /// Batch quality override for this import ("Overrides for this import"
    /// section). Empty = each monitor gets its per-platform default quality.
    pub(super) quality_override: String,
    /// Batch output-directory override. Empty = per-platform default output dir.
    pub(super) out_dir_override: String,
}

/// Self-mutating actions collected while rendering the Streams grid (whose
/// table closure only borrows `self`'s fields disjointly), applied after the
/// table in `apply_streams_actions`.
#[derive(Default)]
struct StreamsOut {
    acts: RowActions,
    toggle_channel: Option<i64>,
    toggle_instance: Option<i64>,
    toggle_stream: Option<String>,
    open_path: Option<std::path::PathBuf>,
    open_in_player: Option<StreamTarget>,
    play_new_instance_mid: Option<i64>,
    copy_text: Option<String>,
    delete_recording: Option<i64>,
    open_recording_props: Option<i64>,
    open_recover_take: Option<i64>,
    archive_vod_now: Option<i64>,
    backfill_head_now: Option<i64>,
    /// (monitor id, recording id) — "View chat" on a stream/take row.
    view_chat_rec: Option<(i64, i64)>,
    // Container-level actions.
    toggle_channel_enabled: Option<(i64, bool)>, // set all instances
    toggle_channel_automation: Option<(i64, bool)>, // master switch
    rename_channel: Option<i64>,
    delete_channel: Option<(i64, String)>,
    clear_channel_err: Option<i64>,
    open_channel_props: Option<i64>,
    /// A double-click on an ad / changes cell opens that take's popup.
    open_ad_popup: Option<i64>,
    open_meta_popup: Option<MetaPopup>,
    open_schedule_popup: Option<i64>,
    /// "Channel history" on a stream row: the owning monitor's all-time
    /// title/category change ledger, independent of any recording.
    open_history_popup: Option<i64>,
    /// Channel id whose 🤝 collab-history window should open.
    open_collab_history: Option<i64>,
    /// Channel id whose 📈 viewer-stats popup should open (span mode).
    open_viewer_stats: Option<i64>,
    /// `(channel id, window title, since, until)` — 📈 popup clamped to one
    /// broadcast's time range ("Stream stats" on a stream row).
    open_stream_stats: Option<(i64, String, i64, i64)>,
    /// Channel id to open the 🚂 mark-hype-train dialog for.
    mark_hype: Option<i64>,
}

#[derive(Clone, Copy)]
enum VodJobKind {
    Recovery,
    Backfill,
}
#[derive(Clone, Copy)]
enum Vis {
    Channel(usize),
    Instance { row: usize, depth: usize },
    Stream { mid: i64, gi: usize, depth: usize },
    Take { mid: i64, gi: usize, ti: usize, depth: usize },
    VodJob { mid: i64, gi: usize, ti: usize, kind: VodJobKind, depth: usize },
}
// A stream is only expandable when it has more than one take,
// or at least one take carries a VOD-recovery/backfill job —
// the job row is the only depth-3 child a single-take stream
// can have (its own take info stays folded into the Stream
// row, same as today).
fn stream_has_children(g: &crate::models::StreamGroup) -> bool {
    g.takes.len() > 1
        || g.takes
            .iter()
            .any(|t| t.recovery_state.is_some() || t.vod_dl_state.is_some())
}

impl StreamArchiverApp {
    /// Modal for creating a new channel container or renaming an existing one.
    #[allow(deprecated)]
    pub(super) fn channel_form_window(&mut self, ctx: &egui::Context) {
        if self.channel_form.is_none() {
            return;
        }
        let renaming = self.channel_form.as_ref().unwrap().id.is_some();
        let title = if renaming { "Rename channel" } else { "Add channel" };
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("channel_form_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.to_string())
                .with_inner_size([380.0, 220.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    let f = self.channel_form.as_mut().unwrap();
                    egui::Grid::new("channel_form_grid")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Name");
                            ui.text_edit_singleline(&mut f.name);
                            ui.end_row();

                            ui.label("Color");
                            ui.horizontal(|ui| {
                                // Colored swatch preview
                                let swatch_color = if f.color.is_empty() {
                                    egui::Color32::from_gray(0x60)
                                } else {
                                    parse_hex_color(&f.color)
                                        .unwrap_or(egui::Color32::from_gray(0x60))
                                };
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(20.0, 20.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(rect, 4.0, swatch_color);
                                ui.painter().rect_stroke(
                                    rect,
                                    4.0,
                                    egui::Stroke::new(1.0, egui::Color32::from_gray(0x80)),
                                    egui::StrokeKind::Inside,
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.color)
                                        .hint_text("#rrggbb")
                                        .desired_width(80.0),
                                );
                                if !f.color.is_empty() && ui.small_button("✕").clicked() {
                                    f.color.clear();
                                }
                            });
                            ui.end_row();

                            ui.label("Download VOD after end");
                            tristate_combo(ui, "chform_vod_download", &mut f.vod_download)
                                .on_hover_text(
                                    "Post-stream VOD download for every instance in this channel. \
                                     Inherit follows the global default (Settings).",
                                );
                            ui.end_row();

                            ui.label("Replace with VOD");
                            tristate_combo(ui, "chform_vod_replace", &mut f.vod_replace)
                                .on_hover_text(
                                    "Replace the live recording with the VOD on success (never for \
                                     a muted Twitch VOD). Inherit follows the global default.",
                                );
                            ui.end_row();

                            ui.label("Fetch new head backfill on new take");
                            tristate_combo(ui, "chform_head_backfill_fetch", &mut f.head_backfill_fetch)
                                .on_hover_text(
                                    "Capture-from-start only: fetch a fresh head backfill for a \
                                     retake (reconnect mid-broadcast), not just the stream's first \
                                     take. Inherit follows the global default (Settings).",
                                );
                            ui.end_row();

                            ui.label("Replace old head (if new is undamaged)");
                            tristate_combo(ui, "chform_head_backfill_replace", &mut f.head_backfill_replace)
                                .on_hover_text(
                                    "Once a fresh head backfill passes its integrity checks, delete \
                                     older takes' now-redundant head files for the same stream. Only \
                                     takes effect when fetching a new head is also on. Inherit \
                                     follows the global default.",
                                );
                            ui.end_row();

                            ui.label("After full.mkv join");
                            join_cleanup_combo(ui, "chform_join_cleanup", &mut f.join_cleanup)
                                .on_hover_text(
                                    "Once a verified full.mkv (head + live capture joined) lands \
                                     for a take in this channel: keep both parts (safe, doubles \
                                     the stream's disk cost), delete just the head, or delete \
                                     both parts (the take then points at the full). Deletions \
                                     follow the deletion method below. Inherit follows the \
                                     global default (Settings → Downloads → Automatic deletion).",
                                );
                            ui.end_row();

                            ui.label("Automatic deletes go to");
                            disposal_method_combo(ui, "chform_disposal_method", &mut f.disposal_method)
                                .on_hover_text(
                                    "How automatic media deletions for this channel are executed \
                                     (post-join cleanup, superseded heads, a live capture \
                                     replaced by its VOD): moved to the configured trash folder, \
                                     sent to the Recycle Bin, or deleted permanently. Inherit \
                                     follows the global default.",
                                );
                            ui.end_row();

                            ui.label("Preferred platform when multiple live");
                            platform_pref_combo(ui, "chform_platform_pref", &mut f.primary_platform_pref)
                                .on_hover_text(
                                    "When this channel has more than one instance simultaneously \
                                     live, show this platform's info on the channel row instead of \
                                     whichever went live earliest. An instance-level pin (per \
                                     instance) overrides this. Inherit follows the global default \
                                     (Settings → Interface → Display).",
                                );
                            ui.end_row();
                        });
                    if !renaming {
                        ui.label(
                            egui::RichText::new(
                                "A channel is a container — add instances (URLs to record) to it with ➕.",
                            )
                            .small()
                            .color(egui::Color32::from_gray(0x90)),
                        );
                    }
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

        if do_save {
            let f = self.channel_form.as_ref().unwrap();
            let name = f.name.trim().to_string();
            if name.is_empty() {
                self.status = "Name is required.".into();
            } else {
                let id_opt = f.id;
                let color = f.color.trim().to_string();
                let platform_pref = f.primary_platform_pref;
                let vod_scope = crate::vod_archive::VodArchiveScope {
                    download: f.vod_download,
                    replace: f.vod_replace,
                };
                let head_backfill_scope = crate::head_backfill::HeadBackfillScope {
                    fetch: f.head_backfill_fetch,
                    replace: f.head_backfill_replace,
                };
                let disposal_scope = crate::disposal::DisposalScope {
                    method: f.disposal_method,
                    join_cleanup: f.join_cleanup,
                };
                let res = match id_opt {
                    Some(id) => self
                        .core
                        .store
                        .rename_channel(id, &name)
                        .and_then(|()| self.core.store.set_channel_color(id, &color))
                        .map(|()| id),
                    None => self.core.store.create_container(&name),
                };
                match res {
                    Ok(cid) => {
                        let _ = crate::vod_archive::save_channel_vod_scope(
                            &self.core.store,
                            cid,
                            &vod_scope,
                        );
                        let _ = crate::head_backfill::save_channel_head_backfill_scope(
                            &self.core.store,
                            cid,
                            &head_backfill_scope,
                        );
                        let _ = crate::disposal::save_channel_disposal_scope(
                            &self.core.store,
                            cid,
                            &disposal_scope,
                        );
                        let _ = crate::platform_pref::save_channel_primary_platform(
                            &self.core.store,
                            cid,
                            platform_pref,
                        );
                        // The preference feeds the cached Streams-view rollup
                        // (`StreamsViewCache::platform_pref`) — bump the rev so
                        // it takes effect immediately instead of waiting for
                        // the next unrelated cache invalidation.
                        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                        self.status = "Saved.".into();
                        self.channel_form = None;
                        // A rename changes the asset-dir path these name-derived
                        // caches read from, so drop them for this channel.
                        if let Some(id) = id_opt {
                            self.channel_icons.remove(&id);
                            self.channel_icons_small.remove(&id);
                            let mids: Vec<i64> = self
                                .rows
                                .iter()
                                .filter(|r| r.channel.id == id)
                                .map(|r| r.monitor.id)
                                .collect();
                            for mid in mids {
                                self.instance_icons_small.remove(&mid);
                            }
                            self.channel_twitch_colors.remove(&id);
                            self.channel_asset_thumbs.remove(&id);
                            self.channel_emote_counts.remove(&id);
                            self.channel_asset_status.remove(&id);
                        }
                        self.reload_rows();
                    }
                    Err(e) => self.status = format!("Error: {e}"),
                }
            }
        } else if do_cancel || !open {
            self.channel_form = None;
        }
    }
    pub(super) fn channels_view(&mut self, ui: &mut egui::Ui) {
        if self.channels.is_empty() {
            self.streams_cache = None;
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No channels yet.");
                ui.label("Click “Add stream” to add a channel + its first instance, or “Add channel” for an empty container.");
            });
            return;
        }

        let now = crate::models::now_unix();
        // 👁 sparkline data: the last hour of raw viewer samples per monitor,
        // refreshed at most once a minute (samples only land once a minute, so
        // querying any faster is pure waste). One small indexed query.
        if now - self.spark_loaded_at >= 60 {
            self.spark_loaded_at = now;
            self.spark_data =
                self.core.store.recent_viewer_history(now - 3_660).unwrap_or_default();
        }
        let any_active = self
            .rows
            .iter()
            .any(|r| r.last_recording_status.as_deref() == Some("recording"));
        // Snapshot which monitors have a live capture process (state dots/tints).
        let active_ids: HashSet<i64> =
            self.core.active.lock().unwrap().keys().copied().collect();

        self.rebuild_streams_cache(ui.ctx(), &active_ids, now);
        let out = self.channels_table(ui, now, &active_ids);
        self.apply_streams_actions(ui, out, any_active);
    }

    /// Rebuild the frame-invariant Streams-view data (`streams_cache`) when
    /// its stamp is stale (see the comment inside). Extracted verbatim from
    /// `channels_view`.
    fn rebuild_streams_cache(
        &mut self,
        ctx: &egui::Context,
        active_ids: &HashSet<i64>,
        now: i64,
    ) {
        // ── Frame-invariant view data, cached across repaints ────────────────
        // Rebuilding this every frame — cloning every Channel, re-grouping every
        // expanded monitor's recordings and re-formatting the whole sort model —
        // dominated frame time under mouse-move repaint rates. Rebuilt when the
        // second ticks (durations / sort keys — but those only move while a
        // capture is active, so a fully idle grid doesn't rebuild at all) or
        // when `streams_cache_rev` bumps (reload installed, expansion toggled,
        // F5, settings saved).
        let stamp = (
            if active_ids.is_empty() { 0 } else { now },
            self.streams_cache_rev,
        );
        if self.streams_cache.as_ref().map(|c| c.stamp) != Some(stamp) {
            // One entry per channel container (including empty ones), attaching
            // its instance rows (indices into self.rows).
            let mut rows_by_channel: HashMap<i64, Vec<usize>> = HashMap::new();
            for (i, row) in self.rows.iter().enumerate() {
                rows_by_channel.entry(row.channel.id).or_default().push(i);
            }
            let chan_entries: Vec<ChanEntry> = self
                .channels
                .iter()
                .map(|c| ChanEntry {
                    channel: c.clone(),
                    rows: rows_by_channel.get(&c.id).cloned().unwrap_or_default(),
                })
                .collect();

            // Resolve each container's avatar (its chosen-platform profile pic)
            // and its name colour up front — both need `&mut self` (caches), so
            // the read-only table closure below can just look them up by id.
            let mut channel_avatars: HashMap<i64, egui::TextureHandle> = HashMap::new();
            // Same, per instance row (each shows its own account's avatar).
            let mut instance_avatars: HashMap<i64, egui::TextureHandle> = HashMap::new();
            // Per-container name colour as (base, adjust): `adjust` marks a fetched
            // Twitch broadcaster colour that should be made readable against the row's
            // *effective* background at render time (rows tint when recording/ad/error).
            // Manual + auto-palette colours are used as-is (already curated).
            let mut channel_name_colors: HashMap<i64, (egui::Color32, bool)> = HashMap::new();
            for e in &chan_entries {
                let cid = e.channel.id;
                let accounts = {
                    let mons: Vec<&MonitorWithChannel> =
                        e.rows.iter().map(|&i| &self.rows[i]).collect();
                    channel_asset_accounts(&mons)
                };
                let tex = self
                    .channel_icons_small
                    .entry(cid)
                    .or_insert_with(|| resolve_channel_icon_small(&e.channel, &accounts, ctx))
                    .clone();
                if let Some(t) = tex {
                    channel_avatars.insert(cid, t);
                }
                for &ri in &e.rows {
                    let mid = self.rows[ri].monitor.id;
                    if !self.instance_icons_small.contains_key(&mid) {
                        let tex = resolve_instance_icon_small(&self.rows[ri], ctx);
                        self.instance_icons_small.insert(mid, tex);
                    }
                    if let Some(t) = self.instance_icons_small.get(&mid).and_then(|o| o.clone()) {
                        instance_avatars.insert(mid, t);
                    }
                }
                // Name colour: a manual custom colour wins; otherwise tint Twitch
                // channels with the streamer's own (cached) name colour — from the
                // preferred icon-source account when set, else the first Twitch
                // account; otherwise the automatic palette.
                let name_color = if !e.channel.color.is_empty() {
                    (channel_event_color(cid, &e.channel.color), false)
                } else if let Some(tw) = preferred_account_index(&e.channel.preferred_asset, &accounts)
                    .filter(|&i| accounts[i].platform == Platform::Twitch)
                    .map(|i| &accounts[i])
                    .or_else(|| accounts.iter().find(|a| a.platform == Platform::Twitch))
                {
                    match *self
                        .channel_twitch_colors
                        .entry(cid)
                        .or_insert_with(|| load_twitch_name_color(&e.channel.name, &tw.account))
                    {
                        Some(c) => (c, true), // raw broadcaster colour → readability at render
                        None => (channel_event_color(cid, ""), false),
                    }
                } else {
                    (channel_event_color(cid, ""), false)
                };
                channel_name_colors.insert(cid, name_color);
            }
            // Drop small-icon entries for monitors that no longer exist so deleted
            // instances don't pin their textures forever.
            {
                let live: HashSet<i64> = self.rows.iter().map(|r| r.monitor.id).collect();
                self.instance_icons_small.retain(|mid, _| live.contains(mid));
            }

            // Lazily load + cache recordings for currently-expanded monitors, then
            // group each monitor's takes into streams.
            // A channel always shows its instances when expanded; an instance shows
            // its stream history when *it* is expanded — so we only need recordings
            // for expanded instances inside expanded channels.
            let mut expanded_monitors: Vec<i64> = Vec::new();
            for e in &chan_entries {
                if !self.expanded_channels.contains(&e.channel.id) {
                    continue;
                }
                for &ri in &e.rows {
                    let mid = self.rows[ri].monitor.id;
                    if self.expanded_instances.contains(&mid) {
                        expanded_monitors.push(mid);
                    }
                }
            }
            for &mid in &expanded_monitors {
                if !self.rec_cache.contains_key(&mid) {
                    let recs = self
                        .core
                        .store
                        .recordings_for_monitor(mid)
                        .unwrap_or_default();
                    self.rec_cache.insert(mid, recs);
                }
            }
            let groups: HashMap<i64, Vec<StreamGroup>> = expanded_monitors
                .iter()
                .map(|&mid| {
                    let recs = self.rec_cache.get(&mid).map(Vec::as_slice).unwrap_or(&[]);
                    (mid, group_recordings(recs))
                })
                .collect();

            // Per-recording ad-break detail (offsets) for the cut-list tooltips on
            // expanded history rows. Cached (cleared on reload) so we issue the SELECT
            // once per take with ads, not every rebuild; bounded by what's expanded.
            for &mid in &expanded_monitors {
                let need: Vec<i64> = match self.rec_cache.get(&mid) {
                    Some(recs) => recs
                        .iter()
                        .filter(|r| r.ad_count > 0 && !self.ad_break_cache.contains_key(&r.id))
                        .map(|r| r.id)
                        .collect(),
                    None => Vec::new(),
                };
                for rid in need {
                    let v = self
                        .core
                        .store
                        .ad_breaks_for_recording(rid)
                        .unwrap_or_default();
                    self.ad_break_cache.insert(rid, v);
                }
            }
            // Same lazy caching for per-recording title/category change logs.
            for &mid in &expanded_monitors {
                let need: Vec<i64> = match self.rec_cache.get(&mid) {
                    Some(recs) => recs
                        .iter()
                        .filter(|r| {
                            r.meta_change_count > 0 && !self.meta_change_cache.contains_key(&r.id)
                        })
                        .map(|r| r.id)
                        .collect(),
                    None => Vec::new(),
                };
                for rid in need {
                    let v = self
                        .core
                        .store
                        .meta_changes_for_recording(rid)
                        .unwrap_or_default();
                    self.meta_change_cache.insert(rid, v);
                }
            }

            // Preferred-platform-when-multiple-live config: loaded once per
            // rebuild (not per channel row per frame — see `PlatformPrefCtx`).
            let platform_pref = crate::platform_pref::PlatformPrefCtx::load(&self.core.store);

            // Channel-level sort/filter model (one entry per top-level channel row).
            let model: Vec<Vec<Cell>> = chan_entries
                .iter()
                .map(|e| {
                    let mons: Vec<&MonitorWithChannel> =
                        e.rows.iter().map(|&i| &self.rows[i]).collect();
                    channel_cells(&e.channel, &mons, active_ids, now, &platform_pref)
                })
                .collect();

            self.streams_cache = Some(StreamsViewCache {
                stamp,
                chan_entries,
                channel_avatars,
                instance_avatars,
                channel_name_colors,
                groups,
                model,
                platform_pref,
            });
        }
    }

    /// Render the Streams grid: the virtualized table, its header, every
    /// row kind and their context menus. The table closure only borrows
    /// `self`'s fields disjointly, so self-mutating picks are collected in the
    /// returned `StreamsOut` and applied afterwards in
    /// `apply_streams_actions`.
    fn channels_table(
        &mut self,
        ui: &mut egui::Ui,
        now: i64,
        active_ids: &HashSet<i64>,
    ) -> StreamsOut {
        // Self-mutating actions, collected during rendering and applied after the
        // table closure (which only borrows `self` immutably).
        let mut out = StreamsOut::default();

        let selected_monitor = self.selected_monitor;
        // Snapshot expansion state for read-only use inside the table closure.
        let exp_channels = self.expanded_channels.clone();
        let exp_instances = self.expanded_instances.clone();
        let exp_streams = self.expanded_streams.clone();
        // Snapshot live VOD-backfill download progress (video_id -> 0.0..=1.0),
        // same map the Videos tab reads (`core.video_progress`) — joined via
        // `Recording.vod_dl_video_id` for the VodJob backfill row's progress bar.
        let vid_progress = self.core.video_progress.lock().unwrap().clone();
        // Snapshot which monitors currently have an ad playing (for the row tint).
        let ad_active = self.core.ad_active.lock().unwrap().clone();
        let ad_running = |mid: i64| ad_active.get(&mid).is_some_and(|&end| now < end);
        // Snapshot capture-ended-but-finalize-pending takes (monitor -> rec):
        // these monitors are still in `active` while their remux waits at the
        // disk gate, and must show "finalizing", not "recording".
        let finalizing_mons: HashMap<i64, i64> = self.core.finalizing.lock().unwrap().clone();
        let finalizing_ids: HashSet<i64> = finalizing_mons.keys().copied().collect();
        let finalizing_recs: HashSet<i64> = finalizing_mons.values().copied().collect();
        // Snapshot which monitors have a live-chat download running (💬 badge on
        // instance rows, bubbled up to their channel row while active).
        let active_chat_ids: HashSet<i64> =
            self.core.active_chats.lock().unwrap().keys().copied().collect();
        // Snapshot the stop-holds (user Stop suppressing auto-restart — ✋ badge).
        let stop_holds_snapshot: HashMap<i64, crate::downloader::StopHold> =
            self.core.stop_holds.lock().unwrap().clone();

        let cache = self.streams_cache.as_ref().unwrap();
        let chan_entries = &cache.chan_entries;
        let channel_avatars = &cache.channel_avatars;
        let instance_avatars = &cache.instance_avatars;
        let channel_name_colors = &cache.channel_name_colors;
        let groups = &cache.groups;
        let model = &cache.model;
        let ad_breaks = &self.ad_break_cache;
        let meta_logs = &self.meta_change_cache;
        let mut sort = self.streams_sort.clone();
        let mut filters = self.streams_filters.clone();
        if filters.len() != STREAM_COLS {
            filters = vec![String::new(); STREAM_COLS];
        }
        // Whether status row tints are drawn (top-bar "Status bgcolor" toggle).
        let status_bgcolor = self.status_bgcolor;
        // Whether the Actions column is shown (Settings → Display). When off it's
        // skipped in the builder, header, and every renderer so the counts match.
        let show_actions = self.show_actions;
        // Persisted column order/visibility, taken as a local copy (mutated by
        // the header's column-chooser context menu, written back + persisted
        // once at the tail of this function).
        let mut entries = self.streams_grid.entries.clone();
        let col_order = grid_columns::effective_order(&STREAM_COLUMNS, &entries, |id| {
            id != "actions" || show_actions
        });
        // A pure reorder (column count unchanged) leaves egui_extras' width
        // cache stale — force one clean re-fit pass when the order just changed.
        let order_changed = self.streams_grid.note_order(&col_order);
        // Snapshot before the table closure so we can read it inside (which only
        // has an immutable borrow of self) and clear it afterwards.
        let scroll_to_cid = self.scroll_to_channel;

        // Fill the available height so the horizontal scrollbar sits at the
        // bottom of the window rather than directly under the (short) row list.
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Labels are selectable by default, which makes them sense clicks
                // (for text selection) and swallow right-clicks over their text —
                // breaking the row context menu. Turn it off for the table so the
                // row's click sense wins (the menu offers "Copy URL" instead).
                ui.style_mut().interaction.selectable_labels = false;
                // Theme accent used for recording/selected rows; ad/error states
                // override the per-row selection color before each row.
                let sel_color = ui.visuals().selection.bg_fill;
                // Platform favicons, uploaded once and cheaply cloned per frame.
                let ptex = self
                    .platform_tex
                    .get_or_insert_with(|| PlatformTextures::load(ui.ctx()))
                    .clone();
                // "Manual fit" (the "⇔" toolbar button) and an in-session reorder
                // both force a fresh sizing pass, but they seed it differently:
                // a manual fit should size fresh from content (forget anything
                // remembered), while a hide/show/reorder should restore each
                // column to whatever the user last resized it to — see
                // `WidthMemory` (`grid_columns.rs`) for why egui_extras's own
                // cache can't survive either event on its own.
                let manual_fit = std::mem::replace(&mut self.reset_streams_columns, false);
                let reset_cols = manual_fit || order_changed;
                let mut tb = TableBuilder::new(ui)
                    .id_salt("streams_table")
                    .striped(true)
                    .resizable(true)
                    // Make rows sense clicks so they can be selected and carry a
                    // right-click context menu.
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                if reset_cols {
                    // Clear persisted column widths so the next load() triggers a
                    // fresh sizing pass at the columns' initial widths.
                    tb.reset();
                    if manual_fit {
                        self.streams_grid.widths.clear();
                    }
                }
                // One column per entry in `col_order` (this frame's persisted,
                // visibility-filtered display order — see `effective_order`);
                // the header and every row shape below all iterate the same
                // `col_order`, so the counts can't drift.
                for &i in &col_order {
                    let c = &STREAM_COLUMNS[i];
                    let min_width = streams_col_min_width(c);
                    let col = if reset_cols {
                        // Drive a clean sizing pass: auto_with_initial_suggestion
                        // seeds the column at min_width (not 0) so cells render
                        // normally during the pass — no zero-width wrapping, no
                        // vertical row bounce. After the pass, content widths are
                        // stored and the next frame snaps to them. A remembered
                        // width (unless this is a manual fit, which wants a fresh
                        // content-based size) overrides that seed so a hide/show/
                        // reorder restores the user's own size instead of
                        // snapping back to the declared default.
                        let seed = self.streams_grid.widths.get(c.id).unwrap_or(min_width);
                        Column::auto_with_initial_suggestion(seed)
                            .at_least(min_width)
                            .clip(c.initial > 0.0)
                    } else if c.initial > 0.0 {
                        // Content-capped column (Title / Game): start narrow and
                        // clip — the cell truncates and shows the full text on hover.
                        Column::initial(c.initial).at_least(min_width).clip(true)
                    } else {
                        Column::auto().at_least(min_width)
                    };
                    tb = tb.column(col);
                }
                // Flatten the channel -> (instance) -> stream -> take tree into
                // the rows currently visible (respecting expansion state).
                // Built BEFORE the table so scroll-to-row can target an index
                // (the sort/filter state is last frame's when a header was
                // clicked this frame — corrected on the immediate repaint).
                let vis = Self::build_vis_rows(
                    model, &sort, &filters, chan_entries, &self.rows, groups,
                    &exp_channels, &exp_instances, &exp_streams,
                );
                // Scroll a newly-added channel into view (rows are virtualized,
                // so the in-cell scroll_to_cursor approach can't work — the
                // target row may not even be laid out this frame).
                if let Some(cid) = scroll_to_cid
                    && let Some(i) = vis.iter().position(|v| {
                        matches!(v, Vis::Channel(ci) if chan_entries[*ci].channel.id == cid)
                    })
                {
                    tb = tb.scroll_to_row(i, Some(egui::Align::Center));
                }
                let mut want_reorder = false;
                let table = tb.header(46.0, |mut header| {
                    for &i in &col_order {
                        let c = &STREAM_COLUMNS[i];
                        let (rect, _) = header.col(|ui| {
                            if grid_header_cell(
                                ui, GridTableId::Streams, i, c, true, &mut sort, &mut filters[i],
                                &mut entries, &STREAM_COLUMNS, |id| id == "actions",
                            ) {
                                want_reorder = true;
                            }
                        });
                        // Every frame, not just on a reset — this is what a later
                        // hide/show/reorder's fresh sizing pass seeds from.
                        self.streams_grid.widths.note(c.id, rect.width());
                    }
                });
                if want_reorder {
                    self.reorder_columns = Some(ReorderColumnsState {
                        table: GridTableId::Streams,
                        draft: entries.clone(),
                    });
                }
                table.body(|body| {
                    // Virtualized: only the rows in view are laid out — the old
                    // per-row loop rebuilt every widget of every row each frame.
                    body.rows(24.0, vis.len(), |mut tr| {
                        match vis[tr.index()] {
                            Vis::Channel(ci) => {
                                Self::channel_row(
                                    &mut tr, &chan_entries[ci], &self.rows, groups,
                                    channel_avatars, channel_name_colors, &ptex,
                                    active_ids, &finalizing_ids, &active_chat_ids,
                                    &ad_running, &exp_channels, now, sel_color,
                                    status_bgcolor, &col_order, &self.spark_data,
                                    &mut out, &cache.platform_pref,
                                );
                            }
                            Vis::Instance { row: ri, depth } => {
                                Self::instance_row(
                                    &mut tr, &self.rows[ri], depth, groups,
                                    &mut self.fs_probes, &self.settings,
                                    &self.scheduled_recordings, &ptex, now, active_ids,
                                    &finalizing_ids, &active_chat_ids, selected_monitor,
                                    &exp_instances, instance_avatars,
                                    &stop_holds_snapshot, &ad_running, sel_color,
                                    status_bgcolor, &col_order, &self.spark_data,
                                    &mut out,
                                );
                            }
                            Vis::Stream { mid, gi, depth } => {
                                Self::stream_row(
                                    &mut tr, &groups[&mid][gi], mid, depth, &self.rows,
                                    &mut self.fs_probes, &self.settings,
                                    &self.background_tasks, &finalizing_recs, ad_breaks,
                                    meta_logs, &self.collab_by_stream, &exp_streams, now,
                                    &col_order, &mut out,
                                );
                            }
                            Vis::Take { mid, gi, ti, depth } => {
                                Self::take_row(
                                    &mut tr, &groups[&mid][gi], ti, depth, &self.rows,
                                    mid, &self.core, &mut self.status,
                                    &mut self.fs_probes, &self.settings,
                                    &self.background_tasks, &finalizing_recs, ad_breaks,
                                    meta_logs, &self.collab_by_stream,
                                    &mut self.rename_rec_id, &mut self.rename_draft,
                                    &mut self.rename_preview,
                                    &mut self.show_rename_dialog, now, &col_order,
                                    &mut out,
                                );
                            }
                            Vis::VodJob { mid, gi, ti, kind, depth } => {
                                Self::vod_job_row(
                                    &mut tr, &groups[&mid][gi], ti, kind, depth,
                                    &self.background_tasks, &vid_progress,
                                    &mut self.fs_probes, &col_order, &mut out,
                                );
                            }
                        }
                    });
                });
            });
        if sort != self.streams_sort {
            let keys: Vec<(usize, bool)> = sort.keys.iter().map(|l| (l.col, l.ascending)).collect();
            let persisted = grid_columns::unresolve_sort(&STREAM_COLUMNS, &keys);
            grid_columns::save_sort(&self.core.store, GridTableId::Streams, &persisted);
        }
        self.streams_sort = sort;
        self.streams_filters = filters;
        if entries != self.streams_grid.entries {
            self.streams_grid.entries = entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Streams, &self.streams_grid.entries);
        }
        // Consume the scroll target: it fired (or the channel was filtered out)
        // — either way, clear it so we don't keep requesting scroll every frame.
        if scroll_to_cid.is_some() {
            self.scroll_to_channel = None;
        }
        out
    }

    /// Apply the self-mutating actions collected while rendering the Streams
    /// grid (expansion toggles, context-menu picks, popup opens, manual
    /// commands). Runs after the table, when `self` is freely mutable again.
    fn apply_streams_actions(
        &mut self,
        ui: &mut egui::Ui,
        out: StreamsOut,
        any_active: bool,
    ) {
        let StreamsOut {
            mut acts,
            toggle_channel,
            toggle_instance,
            toggle_stream,
            open_path,
            open_in_player,
            play_new_instance_mid,
            copy_text,
            delete_recording,
            open_recording_props,
            open_recover_take,
            archive_vod_now,
            backfill_head_now,
            view_chat_rec,
            toggle_channel_enabled,
            toggle_channel_automation,
            rename_channel,
            delete_channel,
            clear_channel_err,
            open_channel_props,
            open_ad_popup,
            open_meta_popup,
            open_schedule_popup,
            open_history_popup,
            open_collab_history,
            open_viewer_stats,
            open_stream_stats,
            mark_hype,
        } = out;
        if let Some(rid) = open_ad_popup
            && !self.ad_popups.contains(&rid)
        {
            self.ad_popups.push(rid);
        }
        if let Some(p) = open_meta_popup {
            let key = p.key();
            if !self.meta_popups.iter().any(|m| m.key() == key) {
                self.meta_popups.push(p);
            }
        }
        if let Some(mid) = open_history_popup
            && !self.history_popups.contains(&mid)
        {
            self.history_popups.push(mid);
        }
        if let Some(cid) = open_collab_history.or(acts.open_collab_history) {
            self.open_collab_history(cid);
        }
        if let Some(cid) = open_viewer_stats.or(acts.open_viewer_stats) {
            self.open_viewer_stats(cid);
        }
        if let Some((cid, label, since, until)) = open_stream_stats {
            self.open_stream_stats(cid, &label, since, until);
        }
        if let Some(cid) = mark_hype.or(acts.mark_hype) {
            self.hype_mark_channel = cid;
            self.hype_mark_abs.clear();
            self.show_hype_mark = true;
        }
        if let Some(rec_id) = open_recover_take {
            self.open_recover_vod_from_seed(rec_id);
        }
        if let Some(rec_id) = archive_vod_now {
            self.core.manual(ManualCommand::ArchiveVodNow(rec_id));
            self.status = "Downloading published VOD…".into();
        }
        if let Some(rec_id) = backfill_head_now.or_else(|| acts.backfill_head.take()) {
            self.core.manual(ManualCommand::BackfillHeadNow(rec_id));
            self.status = "Backfilling head…".into();
        }
        // Next stream double-click: a channel/stream/take row sets the local; an
        // instance row routes through RowActions.
        if let Some(mid) = open_schedule_popup.or(acts.open_schedule)
            && !self.schedule_popups.contains(&mid)
        {
            self.schedule_popups.push(mid);
        }

        // Tick the live Duration column ~1/sec while anything is recording.
        if any_active {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
        }

        if toggle_channel.is_some() || toggle_instance.is_some() || toggle_stream.is_some() {
            // Expansion feeds the cached view data — rebuild it right away.
            self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
        }
        if let Some(id) = toggle_channel {
            if !self.expanded_channels.remove(&id) {
                self.expanded_channels.insert(id);
            }
        }
        if let Some(id) = toggle_instance {
            if !self.expanded_instances.remove(&id) {
                self.expanded_instances.insert(id);
            }
        }
        if let Some(k) = toggle_stream {
            if !self.expanded_streams.remove(&k) {
                self.expanded_streams.insert(k);
            }
        }
        if let Some(mid) = acts.edit {
            if let Some(r) = self.rows.iter().find(|r| r.monitor.id == mid) {
                let mut mf = MonitorForm::from_existing(r);
                let sc = crate::vod_archive::load_monitor_vod_scope(&self.core.store, r.monitor.id);
                mf.vod_download = sc.download;
                mf.vod_replace = sc.replace;
                let hbsc = crate::head_backfill::load_monitor_head_backfill_scope(&self.core.store, r.monitor.id);
                mf.head_backfill_fetch = hbsc.fetch;
                mf.head_backfill_replace = hbsc.replace;
                let dsc = crate::disposal::load_monitor_disposal_scope(&self.core.store, r.monitor.id);
                mf.join_cleanup = dsc.join_cleanup;
                mf.disposal_method = dsc.method;
                mf.primary_pin = crate::platform_pref::monitor_is_pinned(&self.core.store, r.monitor.id);
                self.form = Some(mf);
            }
        }
        if let Some(mid) = acts.properties {
            if !self.properties_popups.contains(&mid) {
                self.properties_popups.push(mid);
            }
            // Invalidate the full-size icon cache so the Properties window reloads it
            // (assets may have been fetched since last open). We do NOT invalidate
            // channel_icons_small here: the small avatar in the streams table is still
            // referenced in this frame's paint commands, and dropping it now would free
            // the texture from the shared painter before the main viewport paints,
            // causing "Failed to find texture" warnings. The small icon refreshes
            // automatically on the next AssetFetch completion (cleared in logic()).
            if let Some(r) = self.rows.iter().find(|r| r.monitor.id == mid) {
                self.channel_icons.remove(&r.channel.id);
                self.channel_twitch_colors.remove(&r.channel.id);
            }
        }
        if let Some(cid) = acts.add_instance {
            // Look up the container in `channels` (not `rows`) so this also works
            // for an empty container that has no instances yet.
            if let Some(c) = self.channels.iter().find(|c| c.id == cid) {
                self.form = Some(MonitorForm::add_instance(
                    c,
                    &self.monitor_defaults,
                    &self.settings.default_output_dir,
                ));
            }
        }
        if let Some(id) = acts.select {
            self.selected_monitor = Some(id);
        }
        if let Some((id, on)) = acts.toggle_enabled {
            if let Err(e) = self.core.store.set_monitor_enabled(id, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some((id, on)) = acts.toggle_automation {
            if let Err(e) = self.core.store.set_monitor_automation_enabled(id, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some((id, name)) = acts.delete {
            self.confirm_delete = Some((id, name));
        }
        if let Some((cid, on)) = toggle_channel_enabled {
            if let Err(e) = self.core.store.set_channel_enabled(cid, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some((cid, on)) = toggle_channel_automation {
            if let Err(e) = self.core.store.set_channel_automation_enabled(cid, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some(cid) = rename_channel {
            if let Some(c) = self.channels.iter().find(|c| c.id == cid) {
                let sc = crate::vod_archive::load_channel_vod_scope(&self.core.store, cid);
                let hbsc = crate::head_backfill::load_channel_head_backfill_scope(&self.core.store, cid);
                let dsc = crate::disposal::load_channel_disposal_scope(&self.core.store, cid);
                let platform_pref = crate::platform_pref::channel_primary_platform(&self.core.store, cid);
                self.channel_form = Some(ChannelForm {
                    id: Some(cid),
                    name: c.name.clone(),
                    color: c.color.clone(),
                    vod_download: sc.download,
                    vod_replace: sc.replace,
                    head_backfill_fetch: hbsc.fetch,
                    head_backfill_replace: hbsc.replace,
                    join_cleanup: dsc.join_cleanup,
                    disposal_method: dsc.method,
                    primary_platform_pref: platform_pref,
                });
            }
        }
        if let Some((cid, name)) = delete_channel {
            self.confirm_delete_channel = Some((cid, name));
        }
        if let Some(cid) = clear_channel_err {
            if let Err(e) = self.core.store.clear_channel_errors(cid) {
                self.status = format!("Error: {e}");
            } else {
                self.reload_rows();
            }
        }
        if let Some(cid) = open_channel_props {
            if !self.channel_properties_popups.contains(&cid) {
                self.channel_properties_popups.push(cid);
            }
            // Same reasoning as acts.properties above: only drop the full-size icon
            // (used inside the child viewport) — not the small avatar (still painted
            // by the main viewport this frame).
            self.channel_icons.remove(&cid);
            self.channel_twitch_colors.remove(&cid);
            self.channel_asset_thumbs.remove(&cid);
            self.channel_emote_counts.remove(&cid);
            self.channel_asset_status.remove(&cid);
        }
        if let Some(id) = acts.start {
            self.core.manual(ManualCommand::Start { id, user_initiated: true });
            self.status = "Checking channel… will record if live.".into();
        }
        if let Some((id, hours)) = acts.stop {
            self.core
                .manual(ManualCommand::StopHoldFor { monitor_id: id, hours });
            self.status = match hours {
                Some(h) => format!("Stopping — auto-record held for {h} hours."),
                None => "Stopping — auto-record held until a new broadcast.".into(),
            };
        }
        if let Some(id) = acts.stop_chat {
            self.core.manual(ManualCommand::StopChat(id));
            self.status = "Stopping chat download…".into();
        }
        if let Some(mid) = acts.reorganize_monitor {
            self.core.manual(ManualCommand::ReorganizeMonitor(mid));
            self.status = "Re-organizing monitor recordings…".into();
        }
        if let Some(cid) = acts.reorganize_channel {
            self.core.manual(ManualCommand::ReorganizeChannel(cid));
            self.status = "Re-organizing channel recordings…".into();
        }
        if let Some(mid) = acts.view_chat {
            self.open_chat_popup(mid, None, ui.ctx());
        }
        if let Some((mid, rid)) = view_chat_rec {
            self.open_chat_popup(mid, Some(rid), ui.ctx());
        }
        if let Some(p) = open_path {
            crate::platform::open_path(&p);
        }
        if let Some(target) = open_in_player.or_else(|| acts.stream_in_player.take()) {
            let player = self.settings.media_player_path.trim().to_string();
            if !player.is_empty() {
                let _ = build_player_command(&player, &target).spawn();
            } else if let StreamTarget::Finished(p) | StreamTarget::Growing(p) = &target {
                // SplitAv is unreachable here: its buttons gate on a player.
                crate::platform::open_path(p);
            }
        }
        if let Some(mid) = play_new_instance_mid.or(acts.play_new_instance.take()) {
            let player = self.settings.media_player_path.trim().to_string();
            if !player.is_empty()
                && let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid)
                && let Some(msg) =
                    spawn_play_new_instance(row, &player, &self.settings, &self.core.store)
            {
                self.status = msg;
            }
        }
        if let Some(t) = copy_text {
            ui.ctx().copy_text(t);
        }
        if let Some(rid) = delete_recording {
            if let Err(e) = self.core.store.delete_recording(rid) {
                self.status = format!("Error: {e}");
            }
            // Drop it from the cached history immediately — reload_rows keeps
            // per-monitor caches, so the row would otherwise linger until F5.
            for recs in self.rec_cache.values_mut() {
                recs.retain(|r| r.id != rid);
            }
            // The take (and its cascaded ad breaks / meta changes) is gone; close
            // any popup that referenced it (a take popup for it, or a stream popup
            // that included it).
            self.ad_popups.retain(|r| *r != rid);
            self.meta_popups.retain(|p| match p {
                MetaPopup::Take(id) => *id != rid,
                MetaPopup::Stream(takes) => !takes.iter().any(|(id, _)| *id == rid),
            });
            self.rec_props_popups.retain(|p| p.rec_id != rid);
            self.reload_rows();
        }
        if let Some(rid) = open_recording_props
            && !self.rec_props_popups.iter().any(|p| p.rec_id == rid)
        {
            // Seed the notes draft from the cached recording (already loaded).
            let notes = self
                .rec_cache
                .values()
                .flat_map(|v| v.iter())
                .find(|r| r.id == rid)
                .map(|r| r.notes.clone())
                .unwrap_or_default();
            self.rec_props_popups.push(RecPropsPopup { rec_id: rid, notes });
        }
    }

    /// Flatten the channel -> (instance) -> stream -> take tree into the rows
    /// currently visible (respecting sort/filter order and expansion state).
    #[allow(clippy::too_many_arguments)]
    fn build_vis_rows(
        model: &[Vec<Cell>],
        sort: &SortState,
        filters: &[String],
        chan_entries: &[ChanEntry],
        rows: &[MonitorWithChannel],
        groups: &HashMap<i64, Vec<StreamGroup>>,
        exp_channels: &HashSet<i64>,
        exp_instances: &HashSet<i64>,
        exp_streams: &HashSet<String>,
    ) -> Vec<Vis> {
        let order = ordered_rows(model, sort, filters);
        let mut vis: Vec<Vis> = Vec::new();
        for &ci in &order {
            let e = &chan_entries[ci];
            vis.push(Vis::Channel(ci));
            if !exp_channels.contains(&e.channel.id) {
                continue;
            }
            // Channel container -> its instances -> each instance's
            // stream history -> takes.
            for &ri in &e.rows {
                let mid = rows[ri].monitor.id;
                vis.push(Vis::Instance { row: ri, depth: 1 });
                if !exp_instances.contains(&mid) {
                    continue;
                }
                if let Some(grps) = groups.get(&mid) {
                    for (gi, g) in grps.iter().enumerate() {
                        vis.push(Vis::Stream { mid, gi, depth: 2 });
                        if stream_has_children(g) && exp_streams.contains(&g.key) {
                            for (ti, t) in g.takes.iter().enumerate() {
                                if g.takes.len() > 1 {
                                    vis.push(Vis::Take { mid, gi, ti, depth: 3 });
                                }
                                if t.recovery_state.is_some() {
                                    vis.push(Vis::VodJob {
                                        mid, gi, ti, kind: VodJobKind::Recovery, depth: 3,
                                    });
                                }
                                if t.vod_dl_state.is_some() {
                                    vis.push(Vis::VodJob {
                                        mid, gi, ti, kind: VodJobKind::Backfill, depth: 3,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        vis
    }

    /// Render one channel-container row across all columns, plus its context
    /// menu. Self-mutating picks land in `out`.
    #[allow(clippy::too_many_arguments)]
    fn channel_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        e: &ChanEntry,
        rows: &[MonitorWithChannel],
        groups: &HashMap<i64, Vec<StreamGroup>>,
        channel_avatars: &HashMap<i64, egui::TextureHandle>,
        channel_name_colors: &HashMap<i64, (egui::Color32, bool)>,
        ptex: &PlatformTextures,
        active_ids: &HashSet<i64>,
        finalizing_ids: &HashSet<i64>,
        active_chat_ids: &HashSet<i64>,
        ad_running: &impl Fn(i64) -> bool,
        exp_channels: &HashSet<i64>,
        now: i64,
        sel_color: egui::Color32,
        status_bgcolor: bool,
        col_order: &[usize],
        // Recent viewer samples per monitor for the 👁 sparkline (last hour).
        spark: &HashMap<i64, Vec<(i64, i64)>>,
        out: &mut StreamsOut,
        platform_pref: &crate::platform_pref::PlatformPrefCtx,
    ) {
        let ch = &e.channel;
        let cid = ch.id;
        let mons: Vec<&MonitorWithChannel> =
            e.rows.iter().map(|&ri| &rows[ri]).collect();
        let ninst = mons.len();
        let any_rec = mons.iter().any(|m| {
            active_ids.contains(&m.monitor.id) && !finalizing_ids.contains(&m.monitor.id)
        });
        let fin_count = mons
            .iter()
            .filter(|m| finalizing_ids.contains(&m.monitor.id))
            .count();
        let live_count = channel_live_count(&mons, active_ids);
        let expanded = exp_channels.contains(&cid);
        let platforms = channel_platforms(&mons);
        let last_poll = mons
            .iter()
            .filter_map(|m| m.monitor.last_checked_at)
            .max()
            .unwrap_or(0);
        // The earliest-live (or, if none live, most recent past recording)
        // instance drives the time columns — unless a pin/platform preference
        // picks a different currently-live instance instead (must match
        // `channel_cells`'s sort-model computation exactly, or display and
        // sort order would silently disagree).
        let primary = channel_primary_preferred(
            &mons, active_ids, now, &platform_pref.pins, platform_pref.effective(cid),
        );
        let rec = primary.map(|m| recording_cells(m, now));
        let ads = primary.map(|m| {
            (m.last_recording_ad_count, m.last_recording_ad_secs)
        });
        let meta_changes =
            primary.map(|m| m.last_recording_meta_changes);
        // While recording, show the live meta-log; else
        // fall back to the last-detected info so a
        // live-not-recording channel still shows it.
        let cur_category = primary
            .map(|m| if m.last_recording_status.as_deref() == Some("recording") {
                m.last_recording_category.clone()
            } else {
                m.last_game.clone()
            })
            .unwrap_or_default();
        let cur_title = primary
            .map(|m| if m.last_recording_status.as_deref() == Some("recording") {
                m.last_recording_title.clone()
            } else {
                m.last_title.clone()
            })
            .unwrap_or_default();
        let cur_viewers = primary.map(|m| m.last_viewers).unwrap_or(-1);
        // Current "Stream Together" collab of the primary live instance —
        // drives the 🤝 Collab cell and the name-cell " × Partner" suffix.
        let cur_collab = primary.and_then(|m| m.live_collab.clone());
        // The channel's next stream = the SOONEST upcoming
        // across its instances (the past-recording primary
        // may be a different platform with no schedule).
        let next_mon = mons
            .iter()
            .filter(|m| m.next_stream_at.is_some())
            .min_by_key(|m| m.next_stream_at.unwrap());
        let next_stream_at = next_mon.and_then(|m| m.next_stream_at);
        let next_stream_title = next_mon
            .map(|m| m.next_stream_title.clone())
            .unwrap_or_default();
        let next_stream_mid = next_mon.map(|m| m.monitor.id);
        let ad_free =
            ad_free_summary(channel_ad_free_count(&mons), ninst);
        // Tint the container row by the rolled-up state of
        // its instances (ad playing / recording / errored).
        let any_ad = mons.iter().any(|m| ad_running(m.monitor.id));
        let any_err = mons.iter().copied().any(monitor_errored);
        let tint =
            row_tint(any_rec, any_ad, any_err, false, sel_color, status_bgcolor);
        {
            let mut disc = false;
            for &ci2 in col_order {
                tr.col(|ui| { tint_cell(ui, tint); match STREAM_COLUMNS[ci2].id {
                    "enabled" => {
                        let mut on = ch.automation_enabled;
                        let cb = ui
                            .add_enabled(ninst > 0, egui::Checkbox::new(&mut on, ""))
                            .on_hover_text("Master switch for this channel. Off = all its instances go fully dormant (no detection/recording/fetch) until acted on manually. Independent from each instance's own switch and from Auto.");
                        if cb.changed() {
                            out.toggle_channel_automation = Some((cid, on));
                        }
                    }
                    "auto" => {
                        let mut on = ch.enabled;
                        let cb = ui
                            .add_enabled(ninst > 0, egui::Checkbox::new(&mut on, ""))
                            .on_hover_text("Auto-record this channel (disk-space control). Off = its instances are still monitored (state, schedules, metadata, posts stay current) but nothing records unless started manually. Independent from each instance's own toggle.");
                        if cb.changed() {
                            out.toggle_channel_enabled = Some((cid, on));
                        }
                    }
                    "actions" => {
                        ui.push_id(cid, |ui| {
                            if ui
                                .small_button("➕")
                                .on_hover_text("Add an instance to this channel")
                                .clicked()
                            {
                                out.acts.add_instance = Some(cid);
                            }
                            if ui
                                .small_button("✏")
                                .on_hover_text("Rename channel")
                                .clicked()
                            {
                                out.rename_channel = Some(cid);
                            }
                            if ui
                                .small_button("🗑")
                                .on_hover_text("Delete channel and all its instances")
                                .clicked()
                            {
                                out.delete_channel = Some((cid, ch.name.clone()));
                            }
                        });
                    }
                    "platform" => {
                        platform_icons(ui, ptex, &platforms);
                    }
                    "name" => {
                        // Disclosure triangle, then the chosen-platform
                        // avatar, then the channel name.
                        let mut clicked = false;
                        if ninst > 0 {
                            let tri = if expanded { "▼" } else { "▶" };
                            if ui
                                .add(egui::Button::new(tri).small().frame(false))
                                .on_hover_text("Expand / collapse")
                                .clicked()
                            {
                                clicked = true;
                            }
                        } else {
                            ui.add_space(16.0);
                        }
                        if let Some(tex) = channel_avatars.get(&cid) {
                            let resp = ui.add(
                                egui::Image::from_texture(tex)
                                    .fit_to_exact_size(egui::vec2(18.0, 18.0))
                                    .corner_radius(egui::CornerRadius::same(3)),
                            );
                            queue_alt_image_preview(ui.ctx(), &resp, tex);
                            ui.add_space(3.0);
                        }
                        let (base, adjust) = channel_name_colors
                            .get(&cid)
                            .copied()
                            .unwrap_or_else(|| {
                                (channel_event_color(cid, &ch.color), false)
                            });
                        // Make a fetched Twitch colour readable
                        // against the row's actual background (the
                        // tint when highlighted, else the panel).
                        let name_color = if adjust {
                            let bg =
                                tint.unwrap_or_else(|| ui.visuals().panel_fill);
                            readable_color(base, bg)
                        } else {
                            base
                        };
                        ui.label(
                            egui::RichText::new(&ch.name)
                                .strong()
                                .color(name_color),
                        );
                        // "Stream Together" partners as a weak " × Partner"
                        // suffix while a shared-chat session is live (title
                        // @mentions stay in the 🤝 Collab column only).
                        if let Some(c) = &cur_collab {
                            let suffix = c.name_suffix();
                            if !suffix.is_empty() {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(suffix).weak())
                                        .truncate(),
                                )
                                .on_hover_text(collab_hover(c));
                            }
                        }
                        disc = clicked;
                    }
                    "tool" => {
                        ui.weak(ninst.to_string());
                    }
                    "detection" => {}
                    "scheduled_rec" => {}
                    "polled" => {
                        ts_label(ui, last_poll);
                    }
                    "state" => {
                        if any_rec {
                            let (icon, color) = state_icon("recording");
                            let label = if live_count > 1 {
                                format!("{icon} {live_count}")
                            } else {
                                icon.to_string()
                            };
                            // Every recording instance reports its stream
                            // ended -> the channel is NOT live; the captures
                            // are just draining/muxing. Show ⏬ so the row
                            // stops reading as "live".
                            let all_draining = mons.iter().all(|m| {
                                !active_ids.contains(&m.monitor.id)
                                    || finalizing_ids.contains(&m.monitor.id)
                                    || m.capture_offline
                            });
                            let hover = if all_draining {
                                CAPTURE_OFFLINE_HOVER.to_string()
                            } else if live_count > 1 {
                                format!("recording ({live_count} instances live)")
                            } else {
                                "recording".to_string()
                            };
                            ui.colored_label(color, label).on_hover_text(hover.clone());
                            if all_draining {
                                ui.colored_label(
                                    egui::Color32::from_rgb(0xd0, 0xa0, 0x40),
                                    egui::RichText::new("⏬").small(),
                                )
                                .on_hover_text(hover);
                            }
                        } else if fin_count > 0 {
                            let (icon, color) = state_icon("finalizing");
                            let label = if fin_count > 1 {
                                format!("{icon} {fin_count}")
                            } else {
                                icon.to_string()
                            };
                            ui.colored_label(color, label).on_hover_text(FINALIZING_HOVER);
                        } else if live_count > 0 {
                            let (icon, color) = state_icon("live");
                            let label = if live_count > 1 {
                                format!("{icon} {live_count}")
                            } else {
                                icon.to_string()
                            };
                            let hover = if live_count > 1 {
                                format!("{live_count} instances live")
                            } else {
                                "live".to_string()
                            };
                            ui.colored_label(color, label).on_hover_text(hover);
                        } else if let Some(p) = primary {
                            if p.last_recording_status.as_deref() == Some("failed") {
                                let (icon, color) = state_icon("failed");
                                ui.colored_label(color, icon)
                                    .on_hover_text(fail_hover(&p.last_recording_log));
                            }
                        }
                        // Bubble the instances' live badges up while
                        // they're active — a collapsed channel otherwise
                        // hides that a recording was trigger-started or
                        // that a chat download is still running.
                        // Present-state only: both vanish when the
                        // instance goes idle (history stays on the
                        // stream/take rows).
                        let trig_mons: Vec<&&MonitorWithChannel> = mons
                            .iter()
                            .filter(|m| {
                                active_ids.contains(&m.monitor.id)
                                    && !m.last_recording_trigger.is_empty()
                            })
                            .collect();
                        if !trig_mons.is_empty() {
                            let label = if trig_mons.len() > 1 {
                                format!("⚡ {}", trig_mons.len())
                            } else {
                                "⚡".to_string()
                            };
                            let hover = if trig_mons.len() == 1 {
                                format!(
                                    "Recording started by a trigger word: {}",
                                    trig_mons[0].last_recording_trigger
                                )
                            } else {
                                let lines: Vec<String> = trig_mons
                                    .iter()
                                    .map(|m| {
                                        format!(
                                            "{}: {}",
                                            instance_label(&m.monitor.url),
                                            m.last_recording_trigger
                                        )
                                    })
                                    .collect();
                                format!(
                                    "Recordings started by trigger words:\n{}",
                                    lines.join("\n")
                                )
                            };
                            ui.colored_label(
                                egui::Color32::from_rgb(0xe8, 0xc5, 0x4a),
                                egui::RichText::new(label).small(),
                            )
                            .on_hover_text(hover);
                        }
                        let chat_count = mons
                            .iter()
                            .filter(|m| active_chat_ids.contains(&m.monitor.id))
                            .count();
                        if chat_count > 0 {
                            let label = if chat_count > 1 {
                                format!("💬 {chat_count}")
                            } else {
                                "💬".to_string()
                            };
                            ui.colored_label(
                                egui::Color32::from_rgb(0x4a, 0xc2, 0xff),
                                egui::RichText::new(label).small(),
                            )
                            .on_hover_text(if chat_count > 1 {
                                format!("{chat_count} live-chat downloads are running.")
                            } else {
                                "A live-chat download is running.".to_string()
                            });
                        }
                        let chan_needs_remux: usize = e.rows.iter()
                            .filter_map(|&ri| groups.get(&rows[ri].monitor.id))
                            .flat_map(|gs| gs.iter())
                            .flat_map(|g| g.takes.iter())
                            .filter(|t| {
                                t.output_path.ends_with(".ts")
                                    && crate::downloader::path_in_cache(&t.output_path)
                            })
                            .count();
                        if chan_needs_remux > 0 {
                            let lbl = if chan_needs_remux == 1 {
                                "⚠ needs remux".to_string()
                            } else {
                                format!("⚠ {} need remux", chan_needs_remux)
                            };
                            let tip = if chan_needs_remux == 1 {
                                "1 recording is stuck as .ts — expand to find it.".to_string()
                            } else {
                                format!("{} recordings are stuck as .ts — expand to find them.", chan_needs_remux)
                            };
                            ui.colored_label(egui::Color32::from_rgb(220, 140, 30), lbl)
                                .on_hover_text(tip);
                        }
                    }
                    "next_stream" => {
                        if next_stream_cell(ui, next_stream_at, &next_stream_title, true) {
                            out.open_schedule_popup = next_stream_mid;
                        }
                    }
                    "game" => {
                        meta_value_cell(ui, &cur_category);
                    }
                    "title" => {
                        meta_value_cell(ui, &cur_title);
                    }
                    "collab" => {
                        if collab_cell(ui, cur_collab.as_ref()) {
                            out.open_collab_history = Some(cid);
                        }
                    }
                    "viewers" => {
                        let sp = primary.and_then(|m| spark.get(&m.monitor.id));
                        if viewers_cell(ui, cur_viewers, sp) {
                            out.open_viewer_stats = Some(cid);
                        }
                    }
                    "changes" => {
                        if let Some(c) = meta_changes {
                            meta_cell(ui, c, None, false);
                        }
                    }
                    "ads" => {
                        if let Some((c, s)) = ads {
                            combined_ads_cell(ui, c, s, None, None);
                        }
                    }
                    "went_live" => {
                        if let Some(r) = &rec {
                            ts_went_live_label(ui, r.went_live_secs, r.went_live_approx);
                        }
                    }
                    "started_on" => {
                        if let Some(r) = &rec {
                            ts_label(ui, r.started_secs);
                        }
                    }
                    "lost_time" => {
                        if let Some(r) = &rec {
                            ui.label(&r.lost);
                        }
                    }
                    "duration" => {
                        if let Some(r) = &rec {
                            ui.label(&r.duration);
                        }
                    }
                    "ad_free" => {
                        if !ad_free.0.is_empty() {
                            ui.colored_label(SUCCESS_GREEN, ad_free.0);
                        }
                    }
                    "added" => {
                        ui.label(fmt_date(ch.created_at));
                    }
                    "tags" => {
                        let cur_tags =
                            primary.map(|m| m.last_tags.clone()).unwrap_or_default();
                        let cur_lang =
                            primary.map(|m| m.last_language.clone()).unwrap_or_default();
                        tags_cell(ui, &cur_tags, &cur_lang);
                    }
                    _ => {}
                }});
            }
            tr.response().context_menu(|ui| {
                ui.set_min_width(170.0);
                if ui.button("➕  Add instance").clicked() {
                    out.acts.add_instance = Some(cid);
                    ui.close();
                }
                if ui.button("✏  Rename channel").clicked() {
                    out.rename_channel = Some(cid);
                    ui.close();
                }
                if any_err {
                    ui.separator();
                    if ui.button("✖  Clear error").clicked() {
                        out.clear_channel_err = Some(cid);
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("📁  Re-organize all recordings").on_hover_text("Move all recordings for this channel into/out of subdirectories.").clicked() {
                    out.acts.reorganize_channel = Some(cid);
                    ui.close();
                }
                if ui
                    .button("📈  Viewer stats")
                    .on_hover_text(
                        "Viewer/follower history graphs and sub/bits/raid events for \
                         this channel (also in the Channel Stats tab, or double-click \
                         the 👁 cell).",
                    )
                    .clicked()
                {
                    out.open_viewer_stats = Some(cid);
                    ui.close();
                }
                if ui
                    .button("🚂  Mark hype train…")
                    .on_hover_text(
                        "A hype train is running (or just ran) and wasn't captured? \
                         Record it manually — the start time you give also teaches \
                         the chat-side inference what it should have caught.",
                    )
                    .clicked()
                {
                    out.mark_hype = Some(cid);
                    ui.close();
                }
                ui.separator();
                if ui.button("🗑  Delete channel").clicked() {
                    out.delete_channel = Some((cid, ch.name.clone()));
                    ui.close();
                }
                ui.separator();
                if ui.button("ℹ  Properties").clicked() {
                    out.open_channel_props = Some(cid);
                    ui.close();
                }
            });
            if disc {
                out.toggle_channel = Some(cid);
            }
        }
    }

    /// Render one capture-instance row (the cells live in
    /// `render_instance_row`), computing the per-row probe/target context
    /// first. Self-mutating picks land in `out`.
    #[allow(clippy::too_many_arguments)]
    fn instance_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        row: &MonitorWithChannel,
        depth: usize,
        groups: &HashMap<i64, Vec<StreamGroup>>,
        fs_probes: &mut FsProbes,
        settings: &SettingsForm,
        scheduled_recordings: &[ScheduledRecordingWithNames],
        ptex: &PlatformTextures,
        now: i64,
        active_ids: &HashSet<i64>,
        finalizing_ids: &HashSet<i64>,
        active_chat_ids: &HashSet<i64>,
        selected_monitor: Option<i64>,
        exp_instances: &HashSet<i64>,
        instance_avatars: &HashMap<i64, egui::TextureHandle>,
        stop_holds_snapshot: &HashMap<i64, crate::downloader::StopHold>,
        ad_running: &impl Fn(i64) -> bool,
        sel_color: egui::Color32,
        status_bgcolor: bool,
        col_order: &[usize],
        // Recent viewer samples per monitor for the 👁 sparkline (last hour).
        spark: &HashMap<i64, Vec<(i64, i64)>>,
        out: &mut StreamsOut,
    ) {
        let mid = row.monitor.id;
        let finalizing = finalizing_ids.contains(&mid);
        // "Recording" = a live capture process; a finalize-pending take still
        // occupies `active` but its capture has ended.
        let recording = active_ids.contains(&mid) && !finalizing;
        let chat_active = active_chat_ids.contains(&mid);
        let is_selected = selected_monitor == Some(mid);
        let has_hist = row.recording_count > 0;
        let expanded = exp_instances.contains(&mid);
        // Tint by state: ad playing / recording / errored /
        // keyboard-selected.
        let tint = row_tint(
            recording,
            ad_running(mid),
            monitor_errored(row),
            is_selected,
            sel_color,
            status_bgcolor,
        );
        let inst_needs_remux = groups.get(&mid)
            .map(|gs| {
                gs.iter()
                    .flat_map(|g| g.takes.iter())
                    .filter(|t| {
                        t.output_path.ends_with(".ts")
                            && crate::downloader::path_in_cache(&t.output_path)
                    })
                    .count()
            })
            .unwrap_or(0);
        let media_player = settings.media_player_path.trim().to_string();
        // Best target to open in the media player for this monitor:
        // prefer an active take's live capture the configured player
        // can actually play (a dual-capture monitor falls through to
        // the DASH companion under non-mpv players); fall back to the
        // most recent finished recording's output file.
        // Probes go through the TTL cache (`fs_probes`) —
        // this runs per row per frame.
        let inst_stream_target: Option<StreamTarget> = {
            let fs = &mut *fs_probes;
            groups.get(&mid).and_then(|gs| {
                // Active takes first: prefer a target the
                // configured player can play (dual capture falls
                // through to the DASH companion under non-mpv
                // players); with nothing playable, keep an
                // unplayable one so the button can explain itself.
                let active: Vec<StreamTarget> = gs
                    .iter()
                    .flat_map(|g| g.takes.iter())
                    .filter(|t| t.is_active())
                    .filter_map(|t| fs.target(&t.output_path))
                    .collect();
                if let Some(t) = active
                    .iter()
                    .find(|t| playable_with(t, &media_player))
                    .or_else(|| active.first())
                {
                    return Some(t.clone());
                }
                // Most recent finished take.
                for g in gs {
                    for t in &g.takes {
                        if !t.output_path.is_empty()
                            && fs.is_file(std::path::Path::new(&t.output_path))
                        {
                            return Some(StreamTarget::Finished(
                                std::path::PathBuf::from(&t.output_path),
                            ));
                        }
                    }
                }
                None
            })
        };
        let output_dir_ok = fs_probes
            .is_dir(std::path::Path::new(&row.monitor.output_dir));
        // The most recently started take for this instance (any
        // stream) — the "Backfill head" manual action's target.
        let inst_latest_rec_id = groups.get(&mid).and_then(|gs| {
            gs.iter()
                .flat_map(|g| g.takes.iter())
                .max_by_key(|t| t.started_at)
                .map(|t| t.id)
        });
        let stop_hold_desc = stop_holds_snapshot.get(&mid).map(|h| match h {
            crate::downloader::StopHold::Until(t) => {
                format!("until {}", fmt_datetime_short(*t))
            }
            crate::downloader::StopHold::FreshStream { .. } => {
                "until this channel starts a new broadcast".to_string()
            }
        });
        if render_instance_row(
            tr, row, ptex, now, recording, finalizing, chat_active,
            tint, output_dir_ok, depth, has_hist, expanded,
            inst_needs_remux,
            inst_stream_target.as_ref(), &media_player,
            instance_avatars.get(&mid),
            inst_latest_rec_id,
            scheduled_recordings,
            stop_hold_desc,
            spark.get(&mid),
            col_order, &mut out.acts,
        ) {
            out.toggle_instance = Some(mid);
        }
    }

    /// Render one stream-group row (a broadcast's takes, aggregated) plus its
    /// context menu. Self-mutating picks land in `out`.
    #[allow(clippy::too_many_arguments)]
    fn stream_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        g: &StreamGroup,
        mid: i64,
        depth: usize,
        rows: &[MonitorWithChannel],
        fs_probes: &mut FsProbes,
        settings: &SettingsForm,
        background_tasks: &[crate::events::BackgroundTask],
        finalizing_recs: &HashSet<i64>,
        ad_breaks: &HashMap<i64, Vec<AdBreak>>,
        meta_logs: &HashMap<i64, Vec<StreamMetaChange>>,
        collab_by_stream: &HashMap<(i64, String), String>,
        exp_streams: &HashSet<String>,
        now: i64,
        col_order: &[usize],
        out: &mut StreamsOut,
    ) {
        let has_takes = stream_has_children(g);
        let expanded = exp_streams.contains(&g.key);
        let when = fmt_went_live(g.went_live_at, g.went_live_approx);
        let ts_fn = |s| if short_ts_on() { fmt_datetime_compact(s) } else { fmt_datetime_short(s) };
        let label = if when.is_empty() {
            format!("🎬 {}", ts_fn(g.started_at()))
        } else if short_ts_on() {
            // Compact went-live with ~ prefix
            let approx = g.went_live_approx;
            let s = fmt_datetime_compact(g.went_live_at.unwrap_or(0));
            format!("🎬 {}{s}", if approx { "~" } else { "" })
        } else {
            format!("🎬 {when}")
        };
        let span = (g.ended_at().unwrap_or(now) - g.started_at()).max(0);
        let dir = g
            .takes
            .iter()
            .find(|t| !t.output_path.is_empty())
            .and_then(|t| {
                std::path::Path::new(&t.output_path)
                    .parent()
                    .map(|p| p.to_path_buf())
            });
        // A single-take stream maps to one file (offer it in
        // the context menu); multi-take streams don't.
        let single_file = (g.takes.len() == 1
            && !g.takes[0].output_path.is_empty())
        .then(|| g.takes[0].output_path.clone());
        let ad_count = g.ad_count();
        let ad_secs = g.ad_secs();
        // A single-take stream carries the cut detail on its
        // one take; multi-take streams show per-take cuts when
        // expanded.
        let ad_rec =
            if g.takes.len() == 1 { Some(g.takes[0].id) } else { None };
        let meta_count = g.meta_change_count();
        // Same rule as ads: a single-take stream carries its
        // detail directly; multi-take shows per-take on expand.
        let meta_rec =
            if g.takes.len() == 1 { Some(g.takes[0].id) } else { None };
        let media_player = settings.media_player_path.trim().to_string();
        // Best target for this stream group: an active capture the
        // configured player can play first (dual capture: the SABR
        // primary under mpv, else the DASH companion's .ts; with
        // nothing playable an unplayable target is kept so the
        // button can explain itself), then any existing output
        // file across the takes.
        let grp_stream_target: Option<StreamTarget> = {
            let fs = &mut *fs_probes;
            let active: Vec<StreamTarget> = g.takes.iter()
                .filter(|t| t.is_active())
                .filter_map(|t| fs.target(&t.output_path))
                .collect();
            active
                .iter()
                .find(|t| playable_with(t, &media_player))
                .or_else(|| active.first())
                .cloned()
                .or_else(|| {
                    g.takes.iter()
                        .find(|t| {
                            !t.output_path.is_empty()
                                && fs.is_file(std::path::Path::new(&t.output_path))
                        })
                        .map(|t| StreamTarget::Finished(
                            std::path::PathBuf::from(&t.output_path),
                        ))
                })
        };
        {
            let mut disc = false;
            for &ci2 in col_order {
                tr.col(|ui| match STREAM_COLUMNS[ci2].id {
                    "actions" => {
                        let ok =
                            dir.as_ref().is_some_and(|d| fs_probes.is_dir(d));
                        if ui
                            .add_enabled(ok, egui::Button::new("📂").small())
                            .on_hover_text("Open folder")
                            .clicked()
                        {
                            out.open_path = dir.clone();
                        }
                        let player_ok = !media_player.is_empty()
                            && grp_stream_target
                                .as_ref()
                                .map(|t| playable_with(t, &media_player))
                                .unwrap_or(false);
                        if ui
                            .add_enabled(
                                player_ok,
                                egui::Button::new("⏵").small(),
                            )
                            .on_hover_text(if g.status() == "recording" {
                                "Stream in player"
                            } else {
                                "Open in player"
                            })
                            .on_disabled_hover_text(if media_player.is_empty() {
                                "Set a media player in Settings → Defaults first"
                            } else if grp_stream_target.is_some() {
                                "In-progress SABR capture needs mpv (separate audio/video files)"
                            } else {
                                "No playable capture file found"
                            })
                            .clicked()
                        {
                            out.open_in_player = grp_stream_target.clone();
                        }
                        if ui
                            .add_enabled(
                                !media_player.is_empty(),
                                egui::Button::new("▷").small(),
                            )
                            .on_hover_text("Play new instance in media player at the live edge (does not record)")
                            .on_disabled_hover_text("Set a media player in Settings → Defaults first")
                            .clicked()
                        {
                            out.play_new_instance_mid = Some(mid);
                        }
                    }
                    "name" => {
                        disc = tree_name(
                            ui, depth, has_takes, expanded, None,
                            egui::RichText::new(label.clone()),
                        );
                        if has_takes {
                            ui.weak(format!("· {} takes", g.takes.len()));
                        }
                    }
                    "state" => {
                        let finalizing = g.status() == "recording"
                            && g.takes.iter().any(|t| finalizing_recs.contains(&t.id));
                        let shown = if finalizing { "finalizing" } else { g.status() };
                        let (icon, color) = state_icon(shown);
                        let resp = ui.colored_label(color, icon);
                        if finalizing {
                            resp.on_hover_text(FINALIZING_HOVER);
                        } else if g.status() == "failed" {
                            let log = g
                                .takes
                                .last()
                                .map(|t| t.log_excerpt.as_str())
                                .unwrap_or("");
                            resp.on_hover_text(fail_hover(log));
                        } else {
                            resp.on_hover_text(g.status());
                        }
                        let nr = g.takes.iter().filter(|t| {
                            t.output_path.ends_with(".ts")
                                && crate::downloader::path_in_cache(&t.output_path)
                        }).count();
                        if nr > 0 {
                            let lbl = if nr == 1 {
                                "⚠ needs remux".to_string()
                            } else {
                                format!("⚠ {} need remux", nr)
                            };
                            ui.colored_label(egui::Color32::from_rgb(220, 140, 30), lbl)
                                .on_hover_text("Right-click → Re-remux to MKV.");
                        }
                        let trigger_info = g
                            .takes
                            .iter()
                            .find(|t| !t.trigger_info.is_empty())
                            .map(|t| t.trigger_info.as_str())
                            .unwrap_or("");
                        let vod_not_published = g
                            .takes
                            .iter()
                            .any(|t| t.vod_state.as_deref() == Some("not_published"));
                        let vod_muted_secs = g
                            .takes
                            .iter()
                            .filter(|t| t.vod_state.as_deref() == Some("found"))
                            .map(|t| t.vod_muted_secs.unwrap_or(0))
                            .find(|&s| s > 0);
                        let full_backfilled =
                            g.takes.iter().any(|t| t.full_path.is_some());
                        let head_backfilled =
                            g.takes.iter().any(|t| t.backfill_path.is_some());
                        let backfill_running = g.takes.iter().any(|t| {
                            head_backfill_running(background_tasks, t.id)
                        });
                        let backfill_queued = g
                            .takes
                            .iter()
                            .any(|t| t.head_backfill_state == "queued");
                        let gap_running = g.takes.iter().any(|t| {
                            gap_recover_running(background_tasks, t.id)
                        });
                        take_status_badges(
                            ui,
                            trigger_info,
                            vod_not_published,
                            vod_muted_secs,
                            full_backfilled,
                            head_backfilled,
                            backfill_running,
                            backfill_queued,
                            gap_running,
                        );
                    }
                    "game" => {
                        meta_value_cell(ui, g.category());
                    }
                    "title" => {
                        meta_value_cell(ui, g.title());
                    }
                    "collab" => {
                        // Stored collab of this past/current broadcast, from
                        // the preloaded (monitor, stream id) → names map.
                        if let Some(sid) = &g.stream_id
                            && let Some(names) = collab_by_stream.get(&(mid, sid.clone()))
                        {
                            ui.add(egui::Label::new(names).truncate()).on_hover_text(
                                "Who this broadcast was streamed together with \
                                 (recorded collab history; @name = from the title)",
                            );
                        }
                    }
                    "changes" => {
                        let det = meta_rec.and_then(|id| meta_logs.get(&id));
                        if meta_cell(ui, meta_count, det, true) {
                            out.open_meta_popup = Some(MetaPopup::Stream(
                                g.takes.iter().map(|t| (t.id, t.started_at)).collect(),
                            ));
                        }
                    }
                    "ads" => {
                        let det = ad_rec.and_then(|id| ad_breaks.get(&id));
                        if let Some(r) = combined_ads_cell(
                            ui, ad_count, ad_secs, det, ad_rec,
                        ) {
                            out.open_ad_popup = Some(r);
                        }
                    }
                    "went_live" => {
                        ts_went_live_label(ui, g.went_live_at.unwrap_or(0), g.went_live_approx);
                    }
                    "started_on" => {
                        ts_label(ui, g.started_at());
                    }
                    "lost_time" => {
                        // Resolved lost time when known; else the
                        // provisional started - went_live (so the stream
                        // row matches the monitor row instead of going
                        // blank while a capture is still catching up).
                        let lost = match g.lost_secs() {
                            Some(l) => Some(fmt_duration(l.max(0))),
                            None => g
                                .went_live_at
                                .map(|w| fmt_duration((g.started_at() - w).max(0))),
                        };
                        if let Some(s) = lost {
                            ui.label(s);
                        }
                    }
                    "duration" => {
                        ui.label(fmt_duration(g.captured_secs(now))).on_hover_text(
                            format!(
                                "{} captured across {} take(s) · span {}",
                                fmt_bytes(g.total_bytes()),
                                g.takes.len(),
                                fmt_duration(span),
                            ),
                        );
                    }
                    // "on"/"platform"/"tool"/"detection"/"polled"/
                    // "next_stream"/"ad_free"/"added" are n/a per stream.
                    _ => {}
                });
            }
            tr.response().context_menu(|ui| {
                ui.set_min_width(180.0);
                let dir_ok =
                    dir.as_ref().is_some_and(|d| fs_probes.is_dir(d));
                if ui
                    .add_enabled(dir_ok, egui::Button::new("📂  Open folder"))
                    .clicked()
                {
                    out.open_path = dir.clone();
                    ui.close();
                }
                if let Some(f) = &single_file {
                    // TTL-cached: menus re-run per frame while open.
                    let file_ok =
                        fs_probes.is_file(std::path::Path::new(f));
                    if ui
                        .add_enabled(
                            file_ok,
                            egui::Button::new("▶  Open file"),
                        )
                        .clicked()
                    {
                        out.open_path = Some(std::path::PathBuf::from(f));
                        ui.close();
                    }
                    if ui.button("📋  Copy file path").clicked() {
                        out.copy_text = Some(f.clone());
                        ui.close();
                    }
                }
                if ui
                    .add_enabled(
                        !media_player.is_empty()
                            && grp_stream_target
                                .as_ref()
                                .map(|t| playable_with(t, &media_player))
                                .unwrap_or(false),
                        egui::Button::new("⏵  Stream in player"),
                    )
                    .on_hover_text(if g.status() == "recording" {
                        "Open live capture in the configured media player"
                    } else {
                        "Open in the configured media player"
                    })
                    .on_disabled_hover_text(if media_player.is_empty() {
                        "Set a media player in Settings → Defaults first"
                    } else if grp_stream_target.is_some() {
                        "In-progress SABR capture needs mpv (separate audio/video files)"
                    } else {
                        "No playable capture file found"
                    })
                    .clicked()
                {
                    out.open_in_player = grp_stream_target.clone();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        !media_player.is_empty(),
                        egui::Button::new("▷  Play new instance"),
                    )
                    .on_hover_text("Tune into the stream at the live edge in the media player (does not record)")
                    .on_disabled_hover_text("Set a media player in Settings → Defaults first")
                    .clicked()
                {
                    out.play_new_instance_mid = Some(mid);
                    ui.close();
                }
                {
                    // Latest take with a chat sidecar drives the
                    // stream's chat view. Probe-cache lookups: an
                    // open context menu re-runs this every frame.
                    let fs = &mut *fs_probes;
                    let chat_rec = g
                        .takes
                        .iter()
                        .rev()
                        .find(|t| chat_file_for_recording_cached(fs, t).is_some())
                        .map(|t| t.id);
                    if ui
                        .add_enabled(
                            chat_rec.is_some(),
                            egui::Button::new("💬  View chat"),
                        )
                        .on_disabled_hover_text(
                            "No chat log file found for this stream",
                        )
                        .clicked()
                    {
                        out.view_chat_rec = chat_rec.map(|rid| (mid, rid));
                        ui.close();
                    }
                }
                // VOD-related actions target this stream's LATEST
                // take — a multi-take stream has no single "the"
                // file, but "the VOD" and "the missed head" both
                // conceptually belong to the broadcast as a whole,
                // so pick the take most likely still relevant.
                // Mirrors the same buttons on the Take row.
                if let Some(t) = g.takes.iter().max_by_key(|t| t.started_at)
                    && t.stream_id.is_some()
                {
                    if ui
                        .button("🛟  Recover VOD…")
                        .on_hover_text("Reconstruct this stream's (latest take's) VOD from segments still on the Twitch CDN (deleted or DMCA-muted).")
                        .clicked()
                    {
                        out.open_recover_take = Some(t.id);
                        ui.close();
                    }
                    if ui
                        .button("📥  Download post-stream VOD")
                        .on_hover_text("Download the platform's full published VOD for this stream's latest take now (also retries a failed archive). For the missed intro of a from-start capture, use \"Backfill head\" instead.")
                        .clicked()
                    {
                        out.archive_vod_now = Some(t.id);
                        ui.close();
                    }
                    let owning_monitor = rows
                        .iter()
                        .find(|r| r.monitor.id == mid)
                        .map(|r| &r.monitor);
                    let is_twitch = owning_monitor
                        .map(|m| m.platform() == Platform::Twitch)
                        .unwrap_or(false);
                    let is_live = owning_monitor
                        .map(|m| matches!(m.last_state.as_str(), "live" | "recording"))
                        .unwrap_or(false);
                    if is_twitch
                        && ui
                            .add_enabled(is_live, egui::Button::new("🧩  Backfill head"))
                            .on_hover_text(
                                "Fetch this stream's latest take's missed intro from \
                                 Twitch's still-growing live CDN playlist (pre-mute \
                                 audio). Always forced — ignores the \"fetch new head \
                                 backfill on new take\" setting.",
                            )
                            .on_disabled_hover_text(
                                "This channel isn't currently live — head backfill needs \
                                 the still-growing live CDN playlist, which stops being \
                                 reliably pre-mute-safe once the stream ends. Use \
                                 \"Download post-stream VOD\" instead.",
                            )
                            .clicked()
                    {
                        out.backfill_head_now = Some(t.id);
                        ui.close();
                    }
                }
                if ui
                    .add_enabled(
                        dir.is_some(),
                        egui::Button::new("📋  Copy folder path"),
                    )
                    .clicked()
                {
                    out.copy_text =
                        dir.as_ref().map(|d| d.to_string_lossy().into_owned());
                    ui.close();
                }
                ui.separator();
                if ui
                    .button("📝  Title/category history")
                    .on_hover_text(
                        "Every title/category change ever seen for this instance — \
                         while recording or not.",
                    )
                    .clicked()
                {
                    out.open_history_popup = Some(mid);
                    ui.close();
                }
                if ui
                    .button("🤝  Collab history")
                    .on_hover_text(
                        "Every \"Stream Together\" session recorded for this channel: \
                         when, with whom, and who hosted (plus @mention-in-title \
                         collabs).",
                    )
                    .clicked()
                {
                    out.open_collab_history =
                        rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id);
                    ui.close();
                }
                if ui
                    .button("📈  Stream stats")
                    .on_hover_text(
                        "Viewer graph and sub/bits/raid events for just this \
                         broadcast's time window.",
                    )
                    .clicked()
                {
                    if let Some(r) = rows.iter().find(|r| r.monitor.id == mid) {
                        let label = format!(
                            "{} — {}",
                            r.channel.name,
                            fmt_datetime_short(g.started_at())
                        );
                        out.open_stream_stats = Some((
                            r.channel.id,
                            label,
                            g.started_at(),
                            g.ended_at().unwrap_or(0),
                        ));
                    }
                    ui.close();
                }
            });
            if disc {
                out.toggle_stream = Some(g.key.clone());
            }
        }
    }

    /// Render one Take sub-row (an individual capture of a multi-take stream)
    /// plus its context menu. Self-mutating picks land in `out`.
    #[allow(clippy::too_many_arguments)]
    fn take_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        g: &StreamGroup,
        ti: usize,
        depth: usize,
        rows: &[MonitorWithChannel],
        mid: i64,
        core: &AppCore,
        status: &mut String,
        fs_probes: &mut FsProbes,
        settings: &SettingsForm,
        background_tasks: &[crate::events::BackgroundTask],
        finalizing_recs: &HashSet<i64>,
        ad_breaks: &HashMap<i64, Vec<AdBreak>>,
        meta_logs: &HashMap<i64, Vec<StreamMetaChange>>,
        collab_by_stream: &HashMap<(i64, String), String>,
        rename_rec_id: &mut Option<i64>,
        rename_draft: &mut String,
        rename_preview: &mut String,
        show_rename_dialog: &mut bool,
        now: i64,
        col_order: &[usize],
        out: &mut StreamsOut,
    ) {
        let t = &g.takes[ti];
        let take_variant = dual_take_variant(g, t);
        let dir = std::path::Path::new(&t.output_path)
            .parent()
            .map(|p| p.to_path_buf());
        let file_ok = !t.output_path.is_empty()
            && fs_probes.is_file(std::path::Path::new(&t.output_path));
        let media_player = settings.media_player_path.trim().to_string();
        {
            for &ci2 in col_order {
                tr.col(|ui| match STREAM_COLUMNS[ci2].id {
                    "actions" => {
                        ui.push_id(t.id, |ui| {
                            if ui
                                .add_enabled(file_ok, egui::Button::new("▶").small())
                                .on_hover_text("Open file")
                                .clicked()
                            {
                                out.open_path =
                                    Some(std::path::PathBuf::from(&t.output_path));
                            }
                            let stream_target = if t.is_active() {
                                fs_probes.target(&t.output_path)
                            } else if file_ok {
                                Some(StreamTarget::Finished(
                                    std::path::PathBuf::from(&t.output_path),
                                ))
                            } else {
                                None
                            };
                            let player_ok = !media_player.is_empty()
                                && stream_target
                                    .as_ref()
                                    .map(|st| playable_with(st, &media_player))
                                    .unwrap_or(false);
                            if ui
                                .add_enabled(
                                    player_ok,
                                    egui::Button::new("⏵").small(),
                                )
                                .on_hover_text(if t.is_active() {
                                    "Stream in player (opens the live capture)"
                                } else {
                                    "Open in player"
                                })
                                .on_disabled_hover_text(if media_player.is_empty() {
                                    "Set a media player in Settings → Defaults first"
                                } else if stream_target.is_some() {
                                    "In-progress SABR capture needs mpv (separate audio/video files)"
                                } else {
                                    "No playable capture file found"
                                })
                                .clicked()
                            {
                                out.open_in_player = stream_target;
                            }
                            if ui
                                .add_enabled(
                                    !media_player.is_empty(),
                                    egui::Button::new("▷").small(),
                                )
                                .on_hover_text("Play new instance in media player at the live edge (does not record)")
                                .on_disabled_hover_text("Set a media player in Settings → Defaults first")
                                .clicked()
                            {
                                out.play_new_instance_mid = Some(t.monitor_id);
                            }
                            let dir_ok =
                                dir.as_ref().is_some_and(|d| fs_probes.is_dir(d));
                            if ui
                                .add_enabled(dir_ok, egui::Button::new("📂").small())
                                .on_hover_text("Open folder")
                                .clicked()
                            {
                                out.open_path = dir.clone();
                            }
                            if ui
                                .add_enabled(
                                    !t.output_path.is_empty(),
                                    egui::Button::new("📋").small(),
                                )
                                .on_hover_text("Copy file path")
                                .clicked()
                            {
                                out.copy_text = Some(t.output_path.clone());
                            }
                            let del_hint = if t.is_active() {
                                "Stop the recording before removing this take"
                            } else {
                                "Remove this take from the list (keeps the file)"
                            };
                            if ui
                                .add_enabled(
                                    !t.is_active(),
                                    egui::Button::new("🗑").small(),
                                )
                                .on_hover_text(del_hint)
                                .clicked()
                            {
                                out.delete_recording = Some(t.id);
                            }
                        });
                    }
                    "name" => {
                        let label = match take_variant {
                            Some(v) => format!("Take {} · {}", ti + 1, v),
                            None => format!("Take {}", ti + 1),
                        };
                        tree_name(
                            ui, depth, false, false, None,
                            egui::RichText::new(label).weak(),
                        );
                    }
                    "state" => {
                        let finalizing =
                            t.status == "recording" && finalizing_recs.contains(&t.id);
                        let shown = if finalizing { "finalizing" } else { t.status.as_str() };
                        let (icon, color) = state_icon(shown);
                        let resp = ui.colored_label(color, icon);
                        if finalizing {
                            resp.on_hover_text(FINALIZING_HOVER);
                        } else if t.status == "failed" {
                            let mut msg = fail_hover(&t.log_excerpt);
                            if let Some(code) = t.exit_code {
                                msg = format!("{msg}\n(exit code {code})");
                            }
                            resp.on_hover_text(msg);
                        } else if t.status == "ended" {
                            resp.on_hover_text(
                                "The stream had already ended or wasn't live when we \
                                 tried — nothing to capture (not a failure).",
                            );
                        } else if let Some(code) = t.exit_code {
                            resp.on_hover_text(format!("exit code {code}"));
                        } else {
                            resp.on_hover_text(&t.status);
                        }
                        // CDN VOD-recovery status now has its own
                        // sibling row (Vis::VodJob) below this take —
                        // see the "🛟 VOD recovery" row.
                        // Post-stream published-VOD download status now
                        // has its own sibling row (Vis::VodJob) below
                        // this take — see the "📼 VOD backfill" row.
                        let vod_muted_secs = (t.vod_state.as_deref() == Some("found"))
                            .then(|| t.vod_muted_secs.unwrap_or(0));
                        take_status_badges(
                            ui,
                            &t.trigger_info,
                            t.vod_state.as_deref() == Some("not_published"),
                            vod_muted_secs,
                            t.full_path.is_some(),
                            t.backfill_path.is_some(),
                            head_backfill_running(background_tasks, t.id),
                            t.head_backfill_state == "queued",
                            gap_recover_running(background_tasks, t.id),
                        );
                        // Published-VOD view count (from the checker's Get
                        // Videos polls — free data, refreshed while the mute
                        // watch runs).
                        if let Some(v) = t.vod_views.filter(|v| *v > 0) {
                            ui.weak(format!("📼 {}", fmt_viewers(v))).on_hover_text(format!(
                                "The published VOD had {v} views when last checked \
                                 (the VOD checker polls it for ~2 h after publication)."
                            ));
                        }
                        // In-progress / needs-attention badges
                        let needs_remux = t.output_path.ends_with(".ts")
                            && crate::downloader::path_in_cache(&t.output_path);
                        let remuxing = background_tasks.iter().any(|bt| {
                            bt.kind == crate::events::BackgroundTaskKind::Remux
                                && bt.id == t.id as u64
                        });
                        if remuxing {
                            ui.colored_label(
                                egui::Color32::from_rgb(80, 160, 220),
                                "⏳ Remuxing…",
                            ).on_hover_text("Converting .ts capture to .mkv — check the Background tab for progress.");
                        } else if needs_remux {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 140, 30),
                                "⚠ needs remux",
                            ).on_hover_text("Automatic remux failed — right-click → Re-remux to MKV.");
                        }
                    }
                    "game" => {
                        meta_value_cell(ui, &t.category);
                    }
                    "title" => {
                        meta_value_cell(ui, &t.title);
                    }
                    "collab" => {
                        if let Some(sid) = &t.stream_id
                            && let Some(names) = collab_by_stream.get(&(mid, sid.clone()))
                        {
                            ui.add(egui::Label::new(names).truncate()).on_hover_text(
                                "Who this broadcast was streamed together with \
                                 (recorded collab history; @name = from the title)",
                            );
                        }
                    }
                    "changes" => {
                        let det = meta_logs.get(&t.id);
                        if meta_cell(ui, t.meta_change_count, det, true) {
                            out.open_meta_popup = Some(MetaPopup::Take(t.id));
                        }
                    }
                    "ads" => {
                        let det = ad_breaks.get(&t.id);
                        if let Some(r) = combined_ads_cell(
                            ui, t.ad_count, t.ad_secs, det, Some(t.id),
                        ) {
                            out.open_ad_popup = Some(r);
                        }
                    }
                    // Went Live is n/a per take (blank).
                    "started_on" => {
                        ts_label(ui, t.started_at);
                    }
                    "lost_time" => {
                        // Resolved lost time when known; else the
                        // provisional started - went_live (matches the
                        // monitor row, so a re-attached/in-progress take
                        // isn't blank while it's still catching up).
                        let lost = match t.lost_secs {
                            Some(l) => Some(fmt_duration(l.max(0))),
                            None => t
                                .went_live_at
                                .map(|w| fmt_duration((t.started_at - w).max(0))),
                        };
                        if let Some(s) = lost {
                            ui.label(s);
                        }
                    }
                    "duration" => {
                        let d = ui.label(fmt_duration(t.duration_secs(now)));
                        if t.bytes > 0 {
                            d.on_hover_text(fmt_bytes(t.bytes));
                        }
                    }
                    // "on"/"platform"/"tool"/"detection"/"polled"/
                    // "next_stream"/"went_live"/"ad_free"/"added" are
                    // n/a per take.
                    _ => {}
                });
            }
            tr.response().context_menu(|ui| {
                ui.set_min_width(180.0);
                // Offer re-remux when the finalized file is still a .ts
                // (the automatic remux failed at recording end).
                let needs_remux = t.output_path.ends_with(".ts")
                    && crate::downloader::path_in_cache(&t.output_path);
                if needs_remux {
                    let remux_dest = std::path::Path::new(&t.output_path)
                        .parent() // .cache/
                        .and_then(|p| p.parent()) // output dir
                        .and_then(|d| {
                            std::path::Path::new(&t.output_path)
                                .file_stem()
                                .map(|s| d.join(format!("{}.mkv", s.to_string_lossy())))
                        });
                    if ui
                        .button("🔄  Re-remux to MKV")
                        .on_hover_text("Convert the captured .ts to .mkv using ffmpeg (the automatic remux failed when the recording ended).")
                        .clicked()
                    {
                        if let Some(dest) = remux_dest {
                            core.manual(ManualCommand::ReRemux {
                                rec_id: t.id,
                                capture: std::path::PathBuf::from(&t.output_path),
                                final_: dest,
                            });
                            *status = "Re-remux started…".into();
                        }
                        ui.close();
                    }
                    ui.separator();
                }
                if ui
                    .add_enabled(file_ok, egui::Button::new("▶  Open file"))
                    .clicked()
                {
                    out.open_path =
                        Some(std::path::PathBuf::from(&t.output_path));
                    ui.close();
                }
                {
                    let stream_target = if t.is_active() {
                        fs_probes.target(&t.output_path)
                    } else if file_ok {
                        Some(StreamTarget::Finished(
                            std::path::PathBuf::from(&t.output_path),
                        ))
                    } else {
                        None
                    };
                    let player_ok = !media_player.is_empty()
                        && stream_target
                            .as_ref()
                            .map(|st| playable_with(st, &media_player))
                            .unwrap_or(false);
                    if ui
                        .add_enabled(
                            player_ok,
                            egui::Button::new("⏵  Stream in player"),
                        )
                        .on_hover_text(if t.is_active() {
                            "Open live capture in the configured media player"
                        } else {
                            "Open in the configured media player"
                        })
                        .on_disabled_hover_text(if media_player.is_empty() {
                            "Set a media player in Settings → Defaults first"
                        } else if stream_target.is_some() {
                            "In-progress SABR capture needs mpv (separate audio/video files)"
                        } else {
                            "No playable capture file found"
                        })
                        .clicked()
                    {
                        out.open_in_player = stream_target;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !media_player.is_empty(),
                            egui::Button::new("▷  Play new instance"),
                        )
                        .on_hover_text("Tune into the stream at the live edge in the media player (does not record)")
                        .on_disabled_hover_text("Set a media player in Settings → Defaults first")
                        .clicked()
                    {
                        out.play_new_instance_mid = Some(t.monitor_id);
                        ui.close();
                    }
                }
                let dir_ok =
                    dir.as_ref().is_some_and(|d| fs_probes.is_dir(d));
                if ui
                    .add_enabled(dir_ok, egui::Button::new("📂  Open folder"))
                    .clicked()
                {
                    out.open_path = dir.clone();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        // Probe cache: menu closures re-run per frame.
                        chat_file_for_recording_cached(&mut *fs_probes, t)
                            .is_some(),
                        egui::Button::new("💬  View chat"),
                    )
                    .on_disabled_hover_text(
                        "No chat log file found for this take",
                    )
                    .clicked()
                {
                    out.view_chat_rec = Some((t.monitor_id, t.id));
                    ui.close();
                }
                if let Some(vod_url) = t.vod_url() {
                    if ui.button("🌐  Open VOD").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(vod_url));
                        ui.close();
                    }
                }
                // Recover a deleted/muted VOD from the CDN (Twitch takes
                // that carry a broadcast/stream id).
                if t.stream_id.is_some()
                    && ui
                        .button("🛟  Recover VOD…")
                        .on_hover_text("Reconstruct this VOD from segments still on the Twitch CDN (deleted or DMCA-muted).")
                        .clicked()
                {
                    out.open_recover_take = Some(t.id);
                    ui.close();
                }
                // Post-stream published-VOD download (manual trigger).
                // Result actions ("Open recovered file" / "Open
                // downloaded VOD") live on the job's own sibling row
                // (Vis::VodJob) once a job exists. Not to be confused
                // with "Backfill head" below — that's the CDN intro
                // segments fetched during the live broadcast, this is
                // the full, already-published VOD downloaded after.
                if t.stream_id.is_some()
                    && ui
                        .button("📥  Download post-stream VOD")
                        .on_hover_text("Download the platform's full published VOD for this recording now (also retries a failed archive). For the missed intro of a from-start capture, use \"Backfill head\" instead.")
                        .clicked()
                {
                    out.archive_vod_now = Some(t.id);
                    ui.close();
                }
                // Manually (re)trigger the CDN head-backfill for this
                // take — Twitch capture-from-start only, and only while
                // the channel is live (the growing CDN playlist this
                // depends on stops being pre-mute-safe once the stream
                // ends). Forced regardless of the "fetch new head
                // backfill on new take" setting (user-initiated).
                let owning_monitor =
                    rows.iter().find(|r| r.monitor.id == mid).map(|r| &r.monitor);
                let is_twitch = owning_monitor
                    .map(|m| m.platform() == Platform::Twitch)
                    .unwrap_or(false);
                let is_live = owning_monitor
                    .map(|m| matches!(m.last_state.as_str(), "live" | "recording"))
                    .unwrap_or(false);
                if t.stream_id.is_some()
                    && is_twitch
                    && ui
                        .add_enabled(is_live, egui::Button::new("🧩  Backfill head"))
                        .on_hover_text(
                            "Fetch this take's missed intro from Twitch's still-growing \
                             live CDN playlist (pre-mute audio). Always forced — ignores \
                             the \"fetch new head backfill on new take\" setting.",
                        )
                        .on_disabled_hover_text(
                            "The channel isn't currently live — head backfill needs the \
                             still-growing live CDN playlist, which stops being reliably \
                             pre-mute-safe once the stream ends. Use \"Download \
                             post-stream VOD\" instead.",
                        )
                        .clicked()
                {
                    out.backfill_head_now = Some(t.id);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        !t.output_path.is_empty(),
                        egui::Button::new("📋  Copy file path"),
                    )
                    .clicked()
                {
                    out.copy_text = Some(t.output_path.clone());
                    ui.close();
                }
                ui.separator();
                if ui.button("📄  Properties…").clicked() {
                    out.open_recording_props = Some(t.id);
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(file_ok, egui::Button::new("📁  Re-organize files"))
                    .on_hover_text("Move this recording's files into/out of subdirectories based on File Management settings.")
                    .clicked()
                {
                    core.manual(ManualCommand::ReorganizeTake(t.id));
                    ui.close();
                }
                if ui
                    .add_enabled(file_ok, egui::Button::new("✏  Rename…"))
                    .on_hover_text("Rename this recording's file (and its companions) to a new stem.")
                    .clicked()
                {
                    *rename_rec_id = Some(t.id);
                    *rename_draft = std::path::Path::new(&t.output_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    *rename_preview = rename_draft.clone();
                    *show_rename_dialog = true;
                    ui.close();
                }
                ui.separator();
                let del_hint = if t.is_active() {
                    "Stop the recording before removing this take"
                } else {
                    "Remove this take from the list (keeps the file)"
                };
                if ui
                    .add_enabled(
                        !t.is_active(),
                        egui::Button::new("🗑  Delete from list"),
                    )
                    .on_hover_text(del_hint)
                    .clicked()
                {
                    out.delete_recording = Some(t.id);
                    ui.close();
                }
            });
        }
    }

    /// Render a VOD-recovery / VOD-backfill job sibling row under a take.
    /// Self-mutating picks land in `out`.
    #[allow(clippy::too_many_arguments)]
    fn vod_job_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        g: &StreamGroup,
        ti: usize,
        kind: VodJobKind,
        depth: usize,
        background_tasks: &[crate::events::BackgroundTask],
        vid_progress: &HashMap<i64, f32>,
        fs_probes: &mut FsProbes,
        col_order: &[usize],
        out: &mut StreamsOut,
    ) {
        let t = &g.takes[ti];
        let take_suffix = if g.takes.len() > 1 {
            format!(" · Take {}", ti + 1)
        } else {
            String::new()
        };
        for &ci2 in col_order {
            tr.col(|ui| match STREAM_COLUMNS[ci2].id {
                "name" => {
                    let label = match kind {
                        VodJobKind::Recovery => format!("🛟 VOD recovery{take_suffix}"),
                        VodJobKind::Backfill => format!("📼 VOD backfill{take_suffix}"),
                    };
                    tree_name(
                        ui, depth, false, false, None,
                        egui::RichText::new(label).weak(),
                    );
                }
                "state" => match kind {
                    VodJobKind::Recovery => {
                        let live = background_tasks.iter().find(|bt| {
                            matches!(
                                bt.kind,
                                crate::events::BackgroundTaskKind::RecoverVod(Some(rid)) if rid == t.id
                            )
                        });
                        if let Some(bt) = live {
                            ui.add(
                                egui::ProgressBar::new(bt.progress.unwrap_or(0.0))
                                    .show_percentage()
                                    .desired_width(90.0),
                            );
                            if let Some(info) = &bt.progress_info {
                                ui.label(info);
                            }
                        } else {
                            match t.recovery_state.as_deref() {
                                Some("recovering") => {
                                    ui.colored_label(egui::Color32::from_rgb(80, 160, 220), "recovering…")
                                        .on_hover_text("Reconstructing the VOD from CDN segments — see the Background tab.");
                                }
                                Some("recovered") => {
                                    ui.colored_label(egui::Color32::from_rgb(70, 180, 90), "recovered")
                                        .on_hover_text("A full VOD was recovered from the CDN — right-click → Open recovered file.");
                                }
                                Some("partial") => {
                                    ui.colored_label(egui::Color32::from_rgb(220, 160, 30), "partial")
                                        .on_hover_text("A partial VOD was recovered (some segments were gone) — right-click → Open recovered file.");
                                }
                                Some("unavailable") => {
                                    ui.colored_label(egui::Color32::from_rgb(150, 150, 150), "gone")
                                        .on_hover_text("No segments survived on the CDN — past the ~60-day recovery window.");
                                }
                                Some("failed") => {
                                    ui.colored_label(egui::Color32::from_rgb(200, 90, 90), "failed")
                                        .on_hover_text("The recovery attempt failed — right-click → Retry recovery.");
                                }
                                _ => {}
                            }
                        }
                    }
                    VodJobKind::Backfill => {
                        let live_progress = t
                            .vod_dl_video_id
                            .and_then(|vid| vid_progress.get(&vid).copied());
                        if t.vod_dl_state.as_deref() == Some("downloading") {
                            if let Some(f) = live_progress {
                                ui.add(
                                    egui::ProgressBar::new(f)
                                        .desired_width(90.0)
                                        .text(format!("{:.0}%", f * 100.0)),
                                );
                            } else {
                                ui.colored_label(egui::Color32::from_rgb(80, 160, 220), "downloading…")
                                    .on_hover_text("Downloading the published VOD — see the Videos tab.");
                            }
                        } else {
                            match t.vod_dl_state.as_deref() {
                                Some("archived") => {
                                    let text = if t.vod_muted_secs.unwrap_or(0) > 0 {
                                        "archived (pre-mute)"
                                    } else {
                                        "archived"
                                    };
                                    ui.colored_label(egui::Color32::from_rgb(70, 180, 90), text)
                                        .on_hover_text("The published VOD was downloaded alongside — right-click → Open downloaded VOD.");
                                }
                                Some("replaced") => {
                                    let text = if t.vod_muted_secs.unwrap_or(0) > 0 {
                                        "replaced (pre-mute)"
                                    } else {
                                        "replaced"
                                    };
                                    ui.colored_label(egui::Color32::from_rgb(70, 180, 90), text)
                                        .on_hover_text("The live capture was replaced by the published VOD.");
                                }
                                Some("muted") => {
                                    ui.colored_label(egui::Color32::from_rgb(220, 120, 30), "muted")
                                        .on_hover_text("The published VOD is DMCA-muted — un-muting via recovery; see the Issues panel.");
                                }
                                Some("failed") => {
                                    ui.colored_label(egui::Color32::from_rgb(200, 90, 90), "failed")
                                        .on_hover_text("The published-VOD download failed — right-click → Retry download.");
                                }
                                _ => {}
                            }
                        }
                    }
                },
                _ => {}
            });
        }
        tr.response().context_menu(|ui| {
            ui.set_min_width(180.0);
            match kind {
                VodJobKind::Recovery => {
                    if let Some(rp) = t.recovered_path.as_ref().filter(|p| !p.is_empty()) {
                        let rp_ok = fs_probes.is_file(std::path::Path::new(rp));
                        if ui
                            .add_enabled(rp_ok, egui::Button::new("🛟  Open recovered file"))
                            .clicked()
                        {
                            out.open_path = Some(std::path::PathBuf::from(rp));
                            ui.close();
                        }
                    }
                    if matches!(t.recovery_state.as_deref(), Some("failed") | Some("unavailable"))
                        && ui.button("🛟  Retry recovery").clicked()
                    {
                        out.open_recover_take = Some(t.id);
                        ui.close();
                    }
                }
                VodJobKind::Backfill => {
                    if let Some(vp) = t.vod_dl_path.as_ref().filter(|p| !p.is_empty()) {
                        let vp_ok = fs_probes.is_file(std::path::Path::new(vp));
                        if ui
                            .add_enabled(vp_ok, egui::Button::new("📼  Open downloaded VOD"))
                            .clicked()
                        {
                            out.open_path = Some(std::path::PathBuf::from(vp));
                            ui.close();
                        }
                    }
                    if t.vod_dl_state.as_deref() == Some("failed")
                        && ui.button("📥  Retry download").clicked()
                    {
                        out.archive_vod_now = Some(t.id);
                        ui.close();
                    }
                }
            }
        });
    }


    /// Kick off the Twitch device-code flow on the async runtime, updating the
    /// shared `twitch_flow` state as it progresses and waking the UI.
    pub(super) fn start_twitch_connect(&mut self, ctx: egui::Context) {
        let client_id = self.settings.twitch_client_id.trim().to_string();
        if client_id.is_empty() {
            self.status = "Enter and save a Twitch Client ID first.".into();
            return;
        }
        // Persist the Client ID so the flow + later refresh can read it.
        let _ = self.core.store.set_setting(K_TWITCH_ID, &client_id);

        let flow = self.twitch_flow.clone();
        let store = self.core.store.clone();
        *flow.lock().unwrap() = AuthFlow::Pending {
            user_code: String::new(),
            url: String::new(),
        };
        self.core.rt.spawn(async move {
            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed {
                        message: e.to_string(),
                    };
                    ctx.request_repaint();
                    return;
                }
            };
            let dc = match oauth::start_device(&http, &client_id).await {
                Ok(dc) => dc,
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed {
                        message: e.to_string(),
                    };
                    ctx.request_repaint();
                    return;
                }
            };
            *flow.lock().unwrap() = AuthFlow::Pending {
                user_code: dc.user_code.clone(),
                url: dc.verification_uri.clone(),
            };
            ctx.request_repaint();
            match oauth::poll_token(&http, &client_id, &dc).await {
                Ok(tokens) => match oauth::fetch_user(&http, &client_id, &tokens.access).await {
                    Ok((login, user_id)) => {
                        let _ = oauth::store_tokens(&store, &tokens, &login);
                        let _ = store.set_setting(oauth::K_USER_ID, &user_id);
                        *flow.lock().unwrap() = AuthFlow::Connected { login };
                    }
                    // Authorized, but the account lookup failed (after retries). Keep
                    // the valid tokens — detection only needs the token — but leave
                    // the user id unset, so sub-based ad-free detection stays off
                    // until a reconnect (rather than discarding the connection).
                    Err(e) => {
                        let _ = oauth::store_tokens(&store, &tokens, "");
                        warn!("Twitch connected, but Get Users failed: {e}");
                        *flow.lock().unwrap() = AuthFlow::Connected {
                            login: String::new(),
                        };
                    }
                },
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed {
                        message: e.to_string(),
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    /// Kick off the Google device-code flow (for YouTube subscriptions import),
    /// updating the shared `google_flow` state as it progresses.
    pub(super) fn start_google_connect(&mut self, ctx: egui::Context) {
        let client_id = self.settings.google_client_id.trim().to_string();
        let client_secret = self.settings.google_client_secret.trim().to_string();
        if client_id.is_empty() || client_secret.is_empty() {
            self.status = "Enter and save a Google Client ID and Secret first.".into();
            return;
        }
        let _ = self.core.store.set_setting(google_oauth::K_CLIENT_ID, &client_id);
        let _ = self
            .core
            .store
            .set_setting(google_oauth::K_CLIENT_SECRET, &client_secret);

        let flow = self.google_flow.clone();
        let store = self.core.store.clone();
        *flow.lock().unwrap() = AuthFlow::Pending {
            user_code: String::new(),
            url: String::new(),
        };
        self.core.rt.spawn(async move {
            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed { message: e.to_string() };
                    ctx.request_repaint();
                    return;
                }
            };
            let dc = match google_oauth::start_device(&http, &client_id).await {
                Ok(dc) => dc,
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed { message: e.to_string() };
                    ctx.request_repaint();
                    return;
                }
            };
            *flow.lock().unwrap() = AuthFlow::Pending {
                user_code: dc.user_code.clone(),
                url: dc.verification_uri.clone(),
            };
            ctx.request_repaint();
            match google_oauth::poll_token(&http, &client_id, &client_secret, &dc).await {
                Ok(tokens) => {
                    let _ = google_oauth::store_tokens(&store, &tokens);
                    let identity = google_oauth::fetch_identity(&http, &tokens.access)
                        .await
                        .unwrap_or_default();
                    let _ = store.set_setting(google_oauth::K_IDENTITY, &identity);
                    *flow.lock().unwrap() = AuthFlow::Connected { login: identity };
                }
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed { message: e.to_string() };
                }
            }
            ctx.request_repaint();
        });
    }

    /// Open the import dialog for `platform` and kick off the background fetch of
    /// the user's followed channels / subscriptions.
    pub(super) fn open_import(&mut self, platform: Platform, ctx: egui::Context) {
        let load = Arc::new(Mutex::new(ImportLoadState::Loading));
        let load2 = load.clone();
        let store = self.core.store.clone();
        // Existing YouTube monitors whose URL doesn't carry a `UC…` id (e.g.
        // added by @handle) — resolved to channel ids in the fetch task so the
        // dedup can match them exactly instead of only by name.
        let unresolved_yt: Vec<String> = if platform == Platform::YouTube {
            let mut seen = HashSet::new();
            self.rows
                .iter()
                .filter(|r| {
                    r.monitor.platform() == Platform::YouTube
                        && yt_channel_id(&r.monitor.url).is_none()
                })
                .map(|r| r.monitor.url.clone())
                .filter(|u| seen.insert(u.to_lowercase()))
                .collect()
        } else {
            Vec::new()
        };
        self.core.rt.spawn(async move {
            let result = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
            {
                Err(e) => ImportLoadState::Error(e.to_string()),
                Ok(http) => {
                    let fetched = match platform {
                        Platform::Twitch => imports::twitch_followed(&http, &store).await,
                        Platform::YouTube => imports::youtube_subscriptions(&http, &store).await,
                        _ => Err(anyhow::anyhow!("unsupported platform")),
                    };
                    match fetched {
                        Ok(cands) => {
                            let resolved =
                                imports::resolve_yt_identities(&http, &store, &unresolved_yt)
                                    .await;
                            ImportLoadState::Loaded { cands, resolved }
                        }
                        Err(e) => ImportLoadState::Error(e.to_string()),
                    }
                }
            };
            *load2.lock().unwrap() = result;
            ctx.request_repaint();
        });
        let title = match platform {
            Platform::Twitch => "Import followed Twitch channels",
            Platform::YouTube => "Import YouTube subscriptions",
            _ => "Import channels",
        }
        .to_string();
        self.import_dialog = Some(ImportDialog {
            title,
            load,
            rows: Vec::new(),
            loaded: false,
            search: String::new(),
            status: String::new(),
            quality_override: String::new(),
            out_dir_override: String::new(),
        });
    }

    #[allow(deprecated)]
    pub(super) fn import_window(&mut self, ctx: &egui::Context) {
        if self.import_dialog.is_none() {
            return;
        }
        // Promote a completed background fetch into editable rows once — this needs
        // `self.rows` to mark channels already added, so it happens before the
        // viewport closure borrows the dialog.
        let promote = {
            let d = self.import_dialog.as_ref().unwrap();
            !d.loaded && matches!(&*d.load.lock().unwrap(), ImportLoadState::Loaded { .. })
        };
        if promote {
            // Take the guard once (a second .lock() on the same thread would deadlock).
            let (cands, resolved) = {
                let d = self.import_dialog.as_ref().unwrap();
                let mut g = d.load.lock().unwrap();
                match std::mem::replace(&mut *g, ImportLoadState::Loading) {
                    ImportLoadState::Loaded { cands, resolved } => (cands, resolved),
                    other => {
                        *g = other;
                        (Vec::new(), Vec::new())
                    }
                }
            };
            // Confident dedup: per-platform identity (Twitch login / YouTube UC id).
            // For YouTube monitors whose URL hides the UC id (@handle form), the
            // background task's resolution (URL → id) supplies the exact identity.
            let resolved: std::collections::HashMap<String, String> =
                resolved.into_iter().collect();
            let existing_ids: HashSet<(Platform, String)> = self
                .rows
                .iter()
                .map(|r| {
                    let identity = resolved
                        .get(&r.monitor.url)
                        .cloned()
                        .unwrap_or_else(|| monitor_import_identity(&r.monitor.url));
                    (r.monitor.platform(), identity)
                })
                .collect();
            // Fuzzy dedup: existing container names (catches a channel added under a
            // URL form whose identity can't be matched, e.g. a YouTube @handle
            // whose page scrape failed).
            let existing_names: HashSet<String> =
                self.rows.iter().map(|r| r.channel.name.to_lowercase()).collect();
            let d = self.import_dialog.as_mut().unwrap();
            d.rows = cands
                .into_iter()
                .map(|c| {
                    let already = existing_ids.contains(&(c.platform, c.identity.clone()));
                    let maybe_dup = !already && existing_names.contains(&c.name.to_lowercase());
                    ImportRow {
                        cand: c,
                        selected: !already && !maybe_dup,
                        auto: false,
                        disabled: false,
                        already,
                        maybe_dup,
                    }
                })
                .collect();
            d.loaded = true;
        }

        let Some(dialog) = &mut self.import_dialog else {
            return;
        };
        let mut open = true;
        let mut do_import = false;
        let mut do_close = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("import_vp"),
            egui::ViewportBuilder::default()
                .with_title(dialog.title.clone())
                .with_inner_size([620.0, 560.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if !dialog.loaded {
                        match &*dialog.load.lock().unwrap() {
                            ImportLoadState::Loading => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Loading…");
                                });
                                ctx.request_repaint();
                            }
                            ImportLoadState::Error(e) => {
                                ui.add_space(8.0);
                                ui.colored_label(
                                    egui::Color32::from_rgb(0xE0, 0x6C, 0x6C),
                                    format!("Couldn't load: {e}"),
                                );
                            }
                            ImportLoadState::Loaded { .. } => {
                                ctx.request_repaint(); // will promote next frame
                            }
                        }
                        ui.add_space(8.0);
                        if ui.button("Close").clicked() {
                            do_close = true;
                        }
                        return;
                    }

                    ui.horizontal(|ui| {
                        ui.label(format!("{} channels found.", dialog.rows.len()));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut dialog.search)
                                        .hint_text("Filter…")
                                        .desired_width(160.0),
                                );
                                ui.label("🔍");
                            },
                        );
                    });
                    ui.label(
                        egui::RichText::new(
                            "Tick the channels to import. \"Auto\" lets the scheduler \
                             auto-record (off = monitor only); \"Disabled\" imports the \
                             channel fully turned off (no polling until you enable it). \
                             Already-added channels are greyed out.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.separator();

                    let q = dialog.search.to_lowercase();
                    let visible: Vec<usize> = (0..dialog.rows.len())
                        .filter(|&i| import_row_matches(&dialog.rows[i], &q))
                        .collect();
                    let selectable: Vec<usize> =
                        visible.iter().copied().filter(|&i| !dialog.rows[i].already).collect();

                    // Master controls.
                    ui.horizontal(|ui| {
                        let mut all = !selectable.is_empty()
                            && selectable.iter().all(|&i| dialog.rows[i].selected);
                        if ui
                            .checkbox(&mut all, "All")
                            .on_hover_text("Select/deselect every (not-already-added) channel")
                            .changed()
                        {
                            for &i in &selectable {
                                dialog.rows[i].selected = all;
                            }
                        }
                        ui.separator();
                        if ui.small_button("Auto: all").clicked() {
                            for &i in &selectable {
                                if dialog.rows[i].selected {
                                    dialog.rows[i].auto = true;
                                }
                            }
                        }
                        if ui.small_button("Auto: none").clicked() {
                            for &i in &selectable {
                                dialog.rows[i].auto = false;
                            }
                        }
                    });
                    egui::CollapsingHeader::new("Overrides for this import")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("import_overrides")
                                .num_columns(2)
                                .spacing([10.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Quality");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut dialog.quality_override)
                                            .hint_text("platform default")
                                            .desired_width(140.0),
                                    )
                                    .on_hover_text(
                                        "Quality for every channel imported in this batch \
                                         (e.g. \"best\" or \"720p\"). Empty = each monitor \
                                         gets its per-platform default quality, same as a \
                                         manual Add stream.",
                                    );
                                    ui.end_row();
                                    ui.label("Output dir");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut dialog.out_dir_override)
                                            .hint_text("platform default")
                                            .desired_width(320.0),
                                    )
                                    .on_hover_text(
                                        "Output directory for every channel imported in \
                                         this batch. Empty = the per-platform default \
                                         output directory.",
                                    );
                                    ui.end_row();
                                });
                        })
                        .header_response
                        .on_hover_text(
                            "Optional batch settings applied to every channel this import \
                             creates, instead of the per-platform defaults. Individual \
                             monitors can still be edited afterwards.",
                        );
                    ui.add_space(4.0);

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(380.0)
                        .show(ui, |ui| {
                            egui::Grid::new("import_grid")
                                .num_columns(6)
                                .striped(true)
                                .spacing([10.0, 4.0])
                                .show(ui, |ui| {
                                    ui.strong("Import")
                                        .on_hover_text("Create a monitor for this channel");
                                    ui.strong("Auto").on_hover_text(
                                        "Let the scheduler auto-record this channel when it \
                                         goes live (off = monitor only)",
                                    );
                                    ui.strong("Disabled").on_hover_text(
                                        "Import with the master Enabled switch off: fully \
                                         dormant (no polling, detection, or fetches) until \
                                         you enable it in the grid",
                                    );
                                    ui.strong("Channel");
                                    ui.strong("ID");
                                    ui.strong("Info");
                                    ui.end_row();
                                    for &i in &visible {
                                        let row = &mut dialog.rows[i];
                                        let on = row.selected && !row.already;
                                        ui.add_enabled(
                                            !row.already,
                                            egui::Checkbox::new(&mut row.selected, ""),
                                        );
                                        ui.add_enabled(
                                            on,
                                            egui::Checkbox::new(&mut row.auto, ""),
                                        );
                                        ui.add_enabled(
                                            on,
                                            egui::Checkbox::new(&mut row.disabled, ""),
                                        );
                                        if row.already {
                                            ui.weak(format!("{} (added)", row.cand.name));
                                        } else if row.maybe_dup {
                                            ui.horizontal(|ui| {
                                                ui.label(&row.cand.name);
                                                ui.weak("(maybe added)").on_hover_text(
                                                    "A channel with this name is already in your \
                                                     list — tick to import anyway.",
                                                );
                                            });
                                        } else {
                                            ui.label(&row.cand.name);
                                        }
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&row.cand.id).monospace().small(),
                                            )
                                            .truncate(),
                                        );
                                        ui.weak(&row.cand.detail);
                                        ui.end_row();
                                    }
                                });
                        });

                    ui.separator();
                    let n = dialog
                        .rows
                        .iter()
                        .filter(|r| r.selected && !r.already)
                        .count();
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                n > 0,
                                egui::Button::new(format!("Import {n} selected")),
                            )
                            .clicked()
                        {
                            do_import = true;
                        }
                        if ui.button("Cancel").clicked() {
                            do_close = true;
                        }
                        if !dialog.status.is_empty() {
                            ui.label(&dialog.status);
                        }
                    });
                });
            },
        );

        // Collect the chosen rows before the dialog borrow ends (the create + reload
        // below need `&mut self`).
        let to_create: Vec<(String, String, bool, bool)> = if do_import {
            dialog
                .rows
                .iter()
                .filter(|r| r.selected && !r.already)
                .map(|r| (r.cand.name.clone(), r.cand.url.clone(), r.auto, r.disabled))
                .collect()
        } else {
            Vec::new()
        };
        let quality_override = dialog.quality_override.clone();
        let out_dir_override = dialog.out_dir_override.clone();
        let close = do_close || !open;

        if do_import {
            let out = self.settings.default_output_dir.clone();
            let mut ok = 0usize;
            let mut failed = 0usize;
            let mut last_err: Option<String> = None;
            // Continue past a per-row failure so one bad channel can't drop the rest
            // of the batch.
            for (name, url, auto, disabled) in &to_create {
                match imports::create_monitor(
                    &self.core.store,
                    &self.monitor_defaults,
                    &out,
                    name,
                    url,
                    *auto,
                    !*disabled,
                    Some(&quality_override),
                    Some(&out_dir_override),
                ) {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        failed += 1;
                        last_err = Some(e.to_string());
                    }
                }
            }
            self.reload_rows();
            self.status = if failed == 0 {
                format!("Imported {ok} channel(s).")
            } else {
                format!(
                    "Imported {ok} channel(s); {failed} failed{}.",
                    last_err.map(|e| format!(" (last: {e})")).unwrap_or_default()
                )
            };
            self.import_dialog = None;
        } else if close {
            self.import_dialog = None;
        }
    }
}
